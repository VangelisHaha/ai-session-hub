//! pi（`pi` CLI agent）：`~/.pi/agent/sessions/<编码后的 cwd>/<ISO 时间>_<uuid>.jsonl`
//!
//! 布局与 Claude Code 同构（按 cwd 分目录 + 每会话一个 append-only JSONL），但更省事：
//! 首行 `{"type":"session","id","cwd","timestamp"}` 直接给出真实 cwd，无需反解目录名。
//!
//! 正文行是 `{"type":"message","timestamp":ISO,"message":{role,content[]}}`，
//! role 为 user / assistant / toolResult；content 块有 text / thinking / toolCall，
//! toolResult 额外带 toolName 与 toolCallId。thinking 块不入索引；
//! toolResult 里可能夹带 base64 图片块，只取 text，避免把图片塞进全文索引。

use super::{
    collect_files, file_stat, forward_fill_ts, looks_like_title, parse_ts, read_new_lines,
    uuid_from_filename, Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct PiAdapter;

pub const TOOL: &str = "pi";

impl Adapter for PiAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let root = crate::paths::pi_sessions_dir();
        let mut files = Vec::new();
        collect_files(&root, "jsonl", &mut files);
        files
            .into_iter()
            .filter_map(|path| {
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
            .ok_or_else(|| anyhow!("pi 源缺少文件路径"))?;
        let (lines, new_offset) = read_new_lines(path, from_offset)?;

        let mut session_id = uuid_from_filename(path);
        let mut cwd = None;
        let mut created_at = None;
        let mut updated_at = None;
        let mut title = None;
        let mut messages = Vec::new();

        for line in &lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = value.get("timestamp").and_then(parse_ts);
            if let Some(ts) = ts {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }

            match value.get("type").and_then(Value::as_str) {
                Some("session") => {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        session_id = Some(id.to_string());
                    }
                    if cwd.is_none() {
                        cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
                    }
                }
                Some("message") => {
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let content = message.get("content").unwrap_or(&Value::Null);
                    match message.get("role").and_then(Value::as_str) {
                        Some("user") => {
                            let text = text_blocks(content);
                            if text.trim().is_empty() {
                                continue;
                            }
                            if title.is_none() && looks_like_title(&text) {
                                title = first_line_summary(&text, 120);
                            }
                            messages.push(Message::text(ROLE_USER, text, ts));
                        }
                        Some("assistant") => {
                            let text = text_blocks(content);
                            if !text.trim().is_empty() {
                                messages.push(Message::text(ROLE_ASSISTANT, text, ts));
                            }
                            for block in content.as_array().map(Vec::as_slice).unwrap_or_default() {
                                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                                    continue;
                                }
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let arguments = block
                                    .get("arguments")
                                    .map(|value| value.to_string())
                                    .unwrap_or_default();
                                messages.push(Message::tool_use(name, arguments, ts));
                            }
                            // 中断时 content 往往为空，只有 stopReason/errorMessage；
                            // 不落一条消息，状态判定就看不到"被打断"这件事
                            if message.get("stopReason").and_then(Value::as_str) == Some("aborted")
                            {
                                let note = message
                                    .get("errorMessage")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Operation aborted");
                                messages.push(Message::text(ROLE_ASSISTANT, note, ts));
                            }
                        }
                        Some("toolResult") => {
                            let name = message.get("toolName").and_then(Value::as_str);
                            let text = text_blocks(content);
                            if !text.trim().is_empty() {
                                messages.push(Message::tool_result(name, text, ts));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        forward_fill_ts(&mut messages, created_at);

        let Some(session_id) = session_id else {
            return Ok(None);
        };
        Ok(Some(ParseOutput {
            session: SessionPayload {
                tool: TOOL.to_string(),
                session_id,
                title,
                cwd,
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

/// 只拼 text 块：thinking 是思考过程，image 是 base64 图片，都不该进全文索引
fn text_blocks(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return content.as_str().unwrap_or_default().to_string();
    };
    items
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} --session {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("2026-09-01T12-01-03-219Z_01a05cd8-20f3-792a-bf92-7404316fbad5.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"01a05cd8-20f3-792a-bf92-7404316fbad5","timestamp":"2026-09-01T12:01:03.219Z","cwd":"/Users/x/demo"}"#,
            r#"{"type":"model_change","timestamp":"2026-09-01T12:01:03.286Z","provider":"deepseek","modelId":"deepseek-v4-pro"}"#,
            r#"{"type":"message","timestamp":"2026-09-01T12:01:29.435Z","message":{"role":"user","content":[{"type":"text","text":"看一下 probe.txt"}]}}"#,
            r#"{"type":"message","timestamp":"2026-09-01T12:01:31.974Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"内部推理不该入索引"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"cat probe.txt"}}]}}"#,
            r#"{"type":"message","timestamp":"2026-09-01T12:01:32.100Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"PI_TOKEN=9137"},{"type":"image","data":"iVBORw0KGgoAAAA"}]}}"#,
            r#"{"type":"message","timestamp":"2026-09-01T12:01:33.000Z","message":{"role":"assistant","content":[{"type":"text","text":"9137"}]}}"#,
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn source_for(path: &std::path::Path) -> SourceStat {
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
    fn thinking_and_images_stay_out_of_the_index() {
        let dir = std::env::temp_dir().join(format!("ash-pi-test-{}", std::process::id()));
        let path = write_session(&dir.join("--Users-x-demo--"));
        let output = PiAdapter.parse(&source_for(&path), 0).unwrap().unwrap();
        let session = output.session;

        assert_eq!(session.session_id, "01a05cd8-20f3-792a-bf92-7404316fbad5");
        assert_eq!(session.cwd.as_deref(), Some("/Users/x/demo"));
        assert_eq!(session.title.as_deref(), Some("看一下 probe.txt"));

        let shapes: Vec<(&str, &str, Option<&str>)> = session
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.kind.as_str(), m.tool_name.as_deref()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("user", "text", None),
                ("assistant", "tool_use", Some("bash")),
                ("tool", "tool_result", Some("bash")),
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
        assert!(!joined.contains("iVBORw0KGgo"));
        assert!(joined.contains("PI_TOKEN=9137"));
        assert!(session.messages.iter().all(|m| m.ts.is_some()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn aborted_turn_leaves_an_interrupt_marker() {
        let dir = std::env::temp_dir().join(format!("ash-pi-abort-{}", std::process::id()));
        let session_dir = dir.join("--Users-x-demo--");
        let path = write_session(&session_dir);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            concat!(
                r#"{"type":"message","timestamp":"2026-09-01T12:02:00.000Z","message":{"role":"assistant","content":[],"stopReason":"aborted","errorMessage":"Operation aborted"}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap();
        drop(file);

        let output = PiAdapter.parse(&source_for(&path), 0).unwrap().unwrap();
        let last = output.session.messages.last().unwrap();
        assert_eq!(last.content, "Operation aborted");
        let (state, _) = crate::liveness::classify(
            &crate::liveness::LastMessage {
                role: Some(last.role.clone()),
                kind: Some(last.kind.clone()),
                tool_name: None,
                head: Some(last.content.clone()),
                ts: last.ts,
                has_open_items: false,
            },
            Some(3600),
            false,
        );
        assert_eq!(state, crate::liveness::SessionState::Interrupted);

        std::fs::remove_dir_all(&dir).ok();
    }
}
