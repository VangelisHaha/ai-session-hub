//! WorkBuddy（腾讯 CodeBuddy 换皮，CLI 入口是 `codebuddy` / `cbc`）：
//! `<root>/projects/<编码后的 cwd>/<sessionId>.jsonl`
//!
//! 桌面应用写 `~/.workbuddy`，独立 CLI 写 `~/.codebuddy`，两处格式完全一致，都要扫。
//! 布局与 Claude Code 同构，几乎每行都冗余带 `sessionId` 与 `cwd`，增量解析也拿得到元信息。
//! 行类型：
//! - `ai-title`：模型生成的会话标题，直接当 title，比首条提问兜底准得多；
//! - `message`：role=user 的 content 块是 `input_text`，role=assistant 是 `output_text`；
//! - `function_call` / `function_call_result`：成对的工具调用与结果，后者的 `output`
//!   有 `{type,text}` 与块数组两种形态；
//! - `reasoning` / `file-history-snapshot`：思考过程与文件快照，不入索引。
//!
//! 用户消息整条被 `<system-reminder>` 包住（注入的 identity 文件动辄 14KB），
//! 真实输入在尾部的 `<user_query>` 里。不抠出来的话索引里全是重复的注入上下文。

use super::{
    collect_files, extract_text, file_stat, forward_fill_ts, looks_like_title, parse_ts,
    read_new_lines, uuid_from_filename, Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct WorkBuddyAdapter;

pub const TOOL: &str = "workbuddy";

impl Adapter for WorkBuddyAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let mut files = Vec::new();
        for root in crate::paths::workbuddy_project_dirs() {
            collect_files(&root, "jsonl", &mut files);
        }
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
            .ok_or_else(|| anyhow!("WorkBuddy 源缺少文件路径"))?;
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
            if session_id.is_none() {
                session_id = value
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if cwd.is_none() {
                cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
            }
            let ts = value.get("timestamp").and_then(parse_ts);
            if let Some(ts) = ts {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }

            match value.get("type").and_then(Value::as_str) {
                Some("ai-title") => {
                    if let Some(candidate) = value.get("aiTitle").and_then(Value::as_str) {
                        if looks_like_title(candidate) {
                            title = first_line_summary(candidate, 120);
                        }
                    }
                }
                Some("message") => {
                    let content = value.get("content").unwrap_or(&Value::Null);
                    match value.get("role").and_then(Value::as_str) {
                        Some("user") => {
                            let raw = text_blocks(content, "input_text");
                            let text = user_query(&raw);
                            if text.trim().is_empty() {
                                continue;
                            }
                            if title.is_none() && looks_like_title(&text) {
                                title = first_line_summary(&text, 120);
                            }
                            messages.push(Message::text(ROLE_USER, text, ts));
                        }
                        Some("assistant") => {
                            let text = text_blocks(content, "output_text");
                            if !text.trim().is_empty() {
                                messages.push(Message::text(ROLE_ASSISTANT, text, ts));
                            }
                        }
                        _ => {}
                    }
                }
                Some("function_call") => {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let arguments = match value.get("arguments") {
                        Some(Value::String(raw)) => raw.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    messages.push(Message::tool_use(name, arguments, ts));
                }
                Some("function_call_result") => {
                    let name = value.get("name").and_then(Value::as_str);
                    let text = value.get("output").map(extract_text).unwrap_or_default();
                    if !text.trim().is_empty() {
                        messages.push(Message::tool_result(name, text, ts));
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

/// 按块类型拼文本；content 不是数组时退回通用拍平
fn text_blocks(content: &Value, want: &str) -> String {
    let Some(items) = content.as_array() else {
        return extract_text(content);
    };
    items
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(want))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从 `<user_query>…</user_query>` 里取真实提问；没有该标记时退回剥掉 system-reminder 的剩余部分
fn user_query(raw: &str) -> String {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let mut queries = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        match after.find(CLOSE) {
            Some(end) => {
                queries.push(after[..end].trim().to_string());
                rest = &after[end + CLOSE.len()..];
            }
            // 未闭合（会话正在写入时可能截断）：把剩下的全当提问
            None => {
                queries.push(after.trim().to_string());
                break;
            }
        }
    }
    if !queries.is_empty() {
        return queries
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    strip_system_reminder(raw)
}

/// 兜底：丢掉所有 `<system-reminder>…</system-reminder>` 段落
fn strip_system_reminder(raw: &str) -> String {
    const OPEN: &str = "<system-reminder";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(CLOSE) {
            Some(end) => rest = &after[end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// WorkBuddy 的 CLI 没有进 PATH，装在 app 包里，名字仍是 codebuddy / cbc
pub fn resume_command(session_id: &str) -> String {
    format!("codebuddy --resume {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7.jsonl");
        let lines = [
            r#"{"id":"a1","timestamp":1786010074206,"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"user-context\">\n<user_info>\nOS Version: darwin\n</user_info>\n注入的 identity 文件很长\n</system-reminder>\n<user_query>帮我优化这个展示网页的设计</user_query>"}],"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7","cwd":"/Users/x/nikou-site"}"#,
            r#"{"id":"a2","timestamp":1786010074690,"type":"file-history-snapshot","snapshot":{"messageId":"a1"},"cwd":"/Users/x/nikou-site"}"#,
            r#"{"timestamp":1786010080494,"type":"ai-title","aiTitle":"优化妮蔻系列展示网页设计","sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7","cwd":"/Users/x/nikou-site"}"#,
            r#"{"id":"a3","timestamp":1786010081000,"type":"reasoning","rawContent":[{"type":"reasoning_text","text":"内部推理不该入索引"}],"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"}"#,
            r#"{"id":"a4","timestamp":1786010082000,"type":"function_call","name":"Read","callId":"call_1","arguments":"{\"path\":\"index.html\"}","sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"}"#,
            r#"{"id":"a5","timestamp":1786010083000,"type":"function_call_result","name":"Read","callId":"call_1","output":{"type":"text","text":"WB_TOKEN=6042"},"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"}"#,
            r#"{"id":"a6","timestamp":1786010084000,"type":"function_call_result","name":"Skill","callId":"call_2","output":[{"type":"input_text","text":"块数组形态的结果"}],"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"}"#,
            r#"{"id":"a7","timestamp":1786010085000,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"设计改完了，token 是 6042"}],"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7","cwd":"/Users/x/nikou-site"}"#,
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
    fn user_query_is_extracted_and_injections_dropped() {
        let raw = "<system-reminder data-role=\"user-context\">\n14KB 的 identity 注入\n</system-reminder>\n<user_query>真实提问</user_query>";
        assert_eq!(user_query(raw), "真实提问");
        // 没有 user_query 标记时退回剥 system-reminder
        let fallback = "<system-reminder>噪声</system-reminder>\n裸提问";
        assert_eq!(user_query(fallback), "裸提问");
        // 多个 user_query 段全部保留
        assert_eq!(
            user_query("<user_query>一</user_query>中间<user_query>二</user_query>"),
            "一\n二"
        );
    }

    #[test]
    fn ai_title_wins_and_noise_stays_out() {
        let dir = std::env::temp_dir().join(format!("ash-wb-test-{}", std::process::id()));
        let path = write_session(&dir.join("Users-x-nikou-site"));
        let output = WorkBuddyAdapter
            .parse(&source_for(&path), 0)
            .unwrap()
            .unwrap();
        let session = output.session;

        assert_eq!(session.session_id, "7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7");
        assert_eq!(session.cwd.as_deref(), Some("/Users/x/nikou-site"));
        assert_eq!(session.title.as_deref(), Some("优化妮蔻系列展示网页设计"));

        let shapes: Vec<(&str, &str, Option<&str>)> = session
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.kind.as_str(), m.tool_name.as_deref()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("user", "text", None),
                ("assistant", "tool_use", Some("Read")),
                ("tool", "tool_result", Some("Read")),
                ("tool", "tool_result", Some("Skill")),
                ("assistant", "text", None),
            ]
        );
        assert_eq!(session.messages[0].content, "帮我优化这个展示网页的设计");
        assert_eq!(session.messages[2].content, "WB_TOKEN=6042");
        assert_eq!(session.messages[3].content, "块数组形态的结果");

        let joined = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("identity"));
        assert!(!joined.contains("内部推理"));
        assert!(session.messages.iter().all(|m| m.ts.is_some()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_parse_only_returns_new_messages() {
        let dir = std::env::temp_dir().join(format!("ash-wb-inc-{}", std::process::id()));
        let path = write_session(&dir.join("Users-x-nikou-site"));
        let source = source_for(&path);
        let (_, offset) = read_new_lines(&path, 0).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            concat!(
                r#"{"id":"a8","timestamp":1786010090000,"type":"message","role":"user","content":[{"type":"input_text","text":"<user_query>再看一遍</user_query>"}],"sessionId":"7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap();
        drop(file);

        let output = WorkBuddyAdapter.parse(&source, offset).unwrap().unwrap();
        assert!(!output.full_replace);
        assert_eq!(output.session.messages.len(), 1);
        assert_eq!(output.session.messages[0].content, "再看一遍");
        // 增量行也带 sessionId，会话 ID 不会丢
        assert_eq!(
            output.session.session_id,
            "7de92ddd-63e4-4fbc-87e1-ed21d01b3cc7"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
