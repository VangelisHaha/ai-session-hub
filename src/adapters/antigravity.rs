//! Antigravity（桌面端 2.0 与 `agy` CLI 共用同一份本地数据）：`~/.gemini/antigravity`
//!
//! - 正文：`brain/<conversation-id>/.system_generated/logs/transcript_full.jsonl`，
//!   append-only JSONL，按字节偏移增量解析。用 `_full` 而不是被截断的 `transcript.jsonl`。
//! - 元信息：`conversation_summaries.db`（只读）里的 AI 标题与 `workspace_uris`（cwd）。
//!   transcript 本身没有 cwd，查不到就留空，不影响检索。
//!
//! 行结构：`{step_index, source, type, status, created_at, content, tool_calls[{name,args}]}`
//! - `USER_INPUT`：content 被 `<USER_REQUEST>` 包住，后面还跟着 metadata 块，只索引请求本身；
//! - `PLANNER_RESPONSE`：助手文本 + `tool_calls`（`thinking` 字段不入索引）；
//! - `GENERIC`（source=MODEL）：工具结果，开头有 `Created At:` / `Completed At:` 时间头，剥掉；
//! - `SYSTEM_MESSAGE`：系统通知，不入索引。

use super::{file_stat, forward_fill_ts, looks_like_title, parse_ts, read_new_lines, Adapter};
use super::{ParseOutput, SourceStat};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;

pub struct AntigravityAdapter;

pub const TOOL: &str = "antigravity";

const TRANSCRIPT: &str = ".system_generated/logs/transcript_full.jsonl";

impl Adapter for AntigravityAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let brain = crate::paths::antigravity_dir().join("brain");
        let Ok(entries) = std::fs::read_dir(&brain) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                // brain 下还有 tempmediaStorage 等非会话目录，只认 UUID 命名且有 transcript 的
                let id = entry.file_name().to_string_lossy().to_string();
                if !super::is_uuid(&id) {
                    return None;
                }
                let path = entry.path().join(TRANSCRIPT);
                let (size, mtime_ms) = file_stat(&path)?;
                Some(SourceStat {
                    tool: TOOL,
                    key: path.to_string_lossy().to_string(),
                    path: Some(path),
                    size,
                    mtime_ms,
                    incremental: true,
                })
            })
            .collect()
    }

    fn parse(&self, source: &SourceStat, from_offset: u64) -> Result<Option<ParseOutput>> {
        let path = source
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("Antigravity 源缺少文件路径"))?;
        // <root>/brain/<id>/.system_generated/logs/transcript_full.jsonl
        let Some(conversation_dir) = path.ancestors().nth(3) else {
            return Ok(None);
        };
        let Some(session_id) = conversation_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|id| super::is_uuid(id))
        else {
            return Ok(None);
        };
        let root = conversation_dir.parent().and_then(Path::parent);

        let (lines, new_offset) = read_new_lines(path, from_offset)?;

        let mut created_at = None;
        let mut updated_at = None;
        let mut first_user_title = None;
        let mut messages = Vec::new();
        // 一次 PLANNER_RESPONSE 的多个 tool_calls，对应后面按序到来的 GENERIC 结果
        let mut pending_tools: VecDeque<String> = VecDeque::new();

        for line in &lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = value.get("created_at").and_then(parse_ts);
            if let Some(ts) = ts {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();

            match value.get("type").and_then(Value::as_str) {
                Some("USER_INPUT") => {
                    let text = user_request(content);
                    if text.trim().is_empty() {
                        continue;
                    }
                    if first_user_title.is_none() && looks_like_title(&text) {
                        first_user_title = first_line_summary(&text, 120);
                    }
                    messages.push(Message::text(ROLE_USER, text, ts));
                }
                Some("PLANNER_RESPONSE") => {
                    pending_tools.clear();
                    if !content.trim().is_empty() {
                        messages.push(Message::text(ROLE_ASSISTANT, content, ts));
                    }
                    for call in value
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                    {
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let args = call
                            .get("args")
                            .map(|value| value.to_string())
                            .unwrap_or_default();
                        pending_tools.push_back(name.to_string());
                        messages.push(Message::tool_use(name, args, ts));
                    }
                }
                Some("GENERIC") if value.get("source").and_then(Value::as_str) == Some("MODEL") => {
                    let text = strip_time_header(content);
                    if text.trim().is_empty() {
                        continue;
                    }
                    let name = pending_tools.pop_front();
                    messages.push(Message::tool_result(name.as_deref(), text, ts));
                }
                _ => {}
            }
        }

        forward_fill_ts(&mut messages, created_at);

        let summary = root.and_then(|root| read_summary(root, &session_id));
        let (summary_title, cwd) = summary.unzip();
        let title = summary_title.flatten().or(first_user_title);

        Ok(Some(ParseOutput {
            session: SessionPayload {
                tool: TOOL.to_string(),
                session_id,
                title,
                cwd: cwd.flatten(),
                created_at,
                updated_at,
                source_path: path.to_string_lossy().to_string(),
                messages,
            },
            new_offset,
            full_replace: from_offset == 0,
        }))
    }
}

/// 取 `<USER_REQUEST>` 里的正文；没有包裹（如系统注入的输入）就原样返回
fn user_request(content: &str) -> String {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    if let Some(start) = content.find(OPEN) {
        let rest = &content[start + OPEN.len()..];
        let end = rest.find(CLOSE).unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    content.trim().to_string()
}

/// 工具结果开头的 `Created At:` / `Completed At:` 行（可能重复出现）是噪声
fn strip_time_header(content: &str) -> String {
    content
        .lines()
        .skip_while(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with("Created At:") || line.starts_with("Completed At:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只读查 `conversation_summaries.db`，返回 (AI 标题, cwd)。库被占用或不存在时返回 None
fn read_summary(root: &Path, session_id: &str) -> Option<(Option<String>, Option<String>)> {
    let db = root.join("conversation_summaries.db");
    if !db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(1)).ok()?;
    let (title, uris): (String, String) = conn
        .query_row(
            "SELECT title, workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    let title = Some(title.trim().to_string()).filter(|title| !title.is_empty());
    let cwd = serde_json::from_str::<Vec<String>>(&uris)
        .ok()
        .and_then(|uris| uris.into_iter().next())
        .map(|uri| file_uri_to_path(&uri))
        .filter(|path| !path.is_empty());
    Some((title, cwd))
}

/// `file:///Users/x/a%20b` → `/Users/x/a b`；Windows 的 `file:///C:/x` → `C:/x`
fn file_uri_to_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8_lossy(&out).to_string();
    // `/C:/x` 去掉前导斜杠
    let mut chars = path.chars();
    if chars.next() == Some('/')
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
    {
        return path[1..].to_string();
    }
    path
}

/// `agy` 与桌面端共用会话库，`--conversation` 按 ID 续聊
pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} --conversation {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn write_session(root: &Path) -> std::path::PathBuf {
        let logs = root.join("brain").join(ID).join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript_full.jsonl");
        let lines = [
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-17T03:37:47Z","content":"<USER_REQUEST>\n看下 probe.txt\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nnow\n</ADDITIONAL_METADATA>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-17T03:37:48Z","thinking":"内部推理不该入索引","content":"我来读。","tool_calls":[{"name":"read_file","args":{"Path":"/tmp/probe.txt"}},{"name":"run_command","args":{"CommandLine":"ls"}}]}"#,
            r#"{"step_index":2,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-17T03:37:49Z","content":"系统通知不入索引"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-17T03:37:50Z","content":"Created At: 2026-11-17T11:37:50+08:00\nCompleted At: 2026-11-17T11:37:50+08:00\nPROBE_TOKEN=9137"}"#,
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-17T03:37:51Z","content":"Created At: x\nfile list"}"#,
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-17T03:37:52Z","content":"9137"}"#,
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn source_for(path: &Path) -> SourceStat {
        let (size, mtime_ms) = file_stat(path).unwrap();
        SourceStat {
            tool: TOOL,
            key: path.to_string_lossy().to_string(),
            path: Some(path.to_path_buf()),
            size,
            mtime_ms,
            incremental: true,
        }
    }

    #[test]
    fn parses_steps_and_keeps_noise_out() {
        let root = std::env::temp_dir().join(format!("ash-agy-test-{}", std::process::id()));
        let path = write_session(&root);
        let session = AntigravityAdapter
            .parse(&source_for(&path), 0)
            .unwrap()
            .unwrap()
            .session;

        assert_eq!(session.session_id, ID);
        assert_eq!(session.title.as_deref(), Some("看下 probe.txt"));
        let shapes: Vec<(&str, &str, Option<&str>)> = session
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.kind.as_str(), m.tool_name.as_deref()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("user", "text", None),
                ("assistant", "text", None),
                ("assistant", "tool_use", Some("read_file")),
                ("assistant", "tool_use", Some("run_command")),
                ("tool", "tool_result", Some("read_file")),
                ("tool", "tool_result", Some("run_command")),
                ("assistant", "text", None),
            ]
        );
        let joined = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("内部推理"));
        assert!(!joined.contains("系统通知"));
        assert!(!joined.contains("ADDITIONAL_METADATA"));
        assert!(!joined.contains("Created At"));
        assert!(joined.contains("PROBE_TOKEN=9137"));
        assert!(session.messages.iter().all(|m| m.ts.is_some()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn title_and_cwd_come_from_the_summaries_db() {
        let root = std::env::temp_dir().join(format!("ash-agy-db-{}", std::process::id()));
        let path = write_session(&root);
        let conn = Connection::open(root.join("conversation_summaries.db")).unwrap();
        conn.execute(
            "CREATE TABLE conversation_summaries (conversation_id text, title text, workspace_uris text)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, 'AI 标题', '[\"file:///Users/x/my%20proj\"]')",
            [ID],
        )
        .unwrap();
        drop(conn);

        let session = AntigravityAdapter
            .parse(&source_for(&path), 0)
            .unwrap()
            .unwrap()
            .session;
        assert_eq!(session.title.as_deref(), Some("AI 标题"));
        assert_eq!(session.cwd.as_deref(), Some("/Users/x/my proj"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(file_uri_to_path("file:///Users/x/a"), "/Users/x/a");
        assert_eq!(file_uri_to_path("file:///Users/x/a%20b"), "/Users/x/a b");
        assert_eq!(file_uri_to_path("file:///C:/Users/x"), "C:/Users/x");
    }
}
