//! Kimi Code CLI（`kimi`）：`~/.kimi-code/sessions/wd_<名字>_<hash>/session_<uuid>/`
//!
//! 一个会话是一个目录，而不是一个文件：
//! - `state.json`：`{id, cwd, createdAt, updatedAt, title, lastPrompt, agents{...}}`，元信息全在这里；
//! - `agents/<agent>/wire.jsonl`：append-only 的事件流，正文在这里，因此按字节偏移增量解析。
//!
//! wire.jsonl 里同一条消息会出现在多种事件上（`turn.prompt` / `context.append_message` /
//! `agent.message.appended`）。只认 `agent.message.appended`：它是唯一同时覆盖
//! user / assistant / tool 三种角色的事件，且 `meta.source` 区分了 input / llm / tool，
//! 而 `context.append_message` 只有 user，还混进了注入给模型的上下文（本机样本 76 条 vs 27 条真实提问）。

use super::{
    collect_files, extract_text, file_stat, forward_fill_ts, looks_like_title, parse_ts,
    read_new_lines, Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct KimiAdapter;

pub const TOOL: &str = "kimi";

impl Adapter for KimiAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let root = crate::paths::kimi_sessions_dir();
        let mut files = Vec::new();
        collect_files(&root, "jsonl", &mut files);
        files
            .into_iter()
            .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("wire.jsonl"))
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
            .ok_or_else(|| anyhow!("Kimi 源缺少文件路径"))?;
        let (lines, new_offset) = read_new_lines(path, from_offset)?;

        let session_dir = session_dir(path);
        let state = session_dir.as_deref().and_then(read_state);
        let state_ref = state.as_ref();

        let session_id = state_ref
            .and_then(|meta| meta.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| session_dir.as_deref().and_then(session_id_from_dir));
        let cwd = state_ref
            .and_then(|meta| meta.get("cwd").and_then(Value::as_str))
            .map(str::to_string);
        let mut created_at = state_ref
            .and_then(|meta| meta.get("createdAt"))
            .and_then(parse_ts);
        let mut updated_at = state_ref
            .and_then(|meta| meta.get("updatedAt"))
            .and_then(parse_ts);
        // 交互式会话的 state.json 会写 title；`kimi -p` 一次性模式两个字段都没有，靠首条提问兜底
        let mut title = state_ref
            .and_then(|meta| {
                meta.get("title")
                    .or_else(|| meta.get("lastPrompt"))
                    .and_then(Value::as_str)
            })
            .filter(|value| looks_like_title(value))
            .and_then(|value| first_line_summary(value, 120));

        let mut messages = Vec::new();
        for line in &lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("agent.message.appended") {
                continue;
            }
            let ts = value.get("time").and_then(parse_ts);
            if let Some(ts) = ts {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }
            let Some(wrapper) = value.get("message") else {
                continue;
            };
            let Some(message) = wrapper.get("message") else {
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
                    // content 里的 think 块是思考过程，不入正文；正文只取 text 块
                    let text = text_blocks(content);
                    if !text.trim().is_empty() {
                        messages.push(Message::text(ROLE_ASSISTANT, text, ts));
                    }
                    for call in message
                        .get("toolCalls")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                    {
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let arguments = match call.get("arguments") {
                            Some(Value::String(raw)) => raw.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        messages.push(Message::tool_use(name, arguments, ts));
                    }
                }
                Some("tool") => {
                    let text = text_blocks(content);
                    if !text.trim().is_empty() {
                        // Kimi 的工具结果只带 toolCallId，没有工具名
                        messages.push(Message::tool_result(None, text, ts));
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

/// 从 `.../session_<uuid>/agents/<agent>/wire.jsonl` 回溯到会话目录
fn session_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("session_"))
                .unwrap_or(false)
        })
        .map(Path::to_path_buf)
}

fn session_id_from_dir(dir: &Path) -> Option<String> {
    Some(dir.file_name()?.to_str()?.to_string())
}

fn read_state(session_dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(session_dir.join("state.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// content 是 `[{type:"text"|"think", ...}]`，只拼 text；非数组时退回通用拍平
fn text_blocks(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return extract_text(content);
    };
    items
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn resume_command(session_id: &str) -> String {
    format!("kimi -r {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn session_dir_is_resolved_from_wire_path() {
        let path = Path::new(
            "/Users/x/.kimi-code/sessions/wd_demo_ab12/session_e1af002a-a4d1-4a7d-ab31-b07c8f703ab1/agents/main/wire.jsonl",
        );
        let dir = session_dir(path).unwrap();
        assert_eq!(
            session_id_from_dir(&dir).as_deref(),
            Some("session_e1af002a-a4d1-4a7d-ab31-b07c8f703ab1")
        );
    }

    #[test]
    fn think_blocks_are_excluded_from_text() {
        let content = serde_json::json!([
            {"type": "think", "think": "内部推理"},
            {"type": "text", "text": "对外结论"},
        ]);
        assert_eq!(text_blocks(&content), "对外结论");
    }

    #[test]
    fn only_agent_message_appended_is_parsed() {
        let dir = std::env::temp_dir().join(format!("ash-kimi-test-{}", std::process::id()));
        let session = dir.join("wd_demo_ab12/session_11111111-2222-3333-4444-555555555555");
        let agent = session.join("agents/main");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            session.join("state.json"),
            r#"{"id":"session_11111111-2222-3333-4444-555555555555","cwd":"/tmp/demo","createdAt":1789634149281,"updatedAt":1789634161586}"#,
        )
        .unwrap();
        let wire = agent.join("wire.jsonl");
        std::fs::write(
            &wire,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":\"1.5\"}\n",
                "{\"type\":\"turn.prompt\",\"input\":[{\"type\":\"text\",\"text\":\"重复的提问事件\"}]}\n",
                "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"注入的上下文\"}]}}\n",
                "{\"type\":\"agent.message.appended\",\"time\":1789634149369,\"message\":{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"读一下 probe.txt\"}]},\"meta\":{\"source\":\"input\"}}}\n",
                "{\"type\":\"agent.message.appended\",\"time\":1789634155000,\"message\":{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"think\",\"think\":\"要先读文件\"}],\"toolCalls\":[{\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"probe.txt\\\"}\"}]},\"meta\":{\"source\":\"llm\"}}}\n",
                "{\"type\":\"agent.message.appended\",\"time\":1789634156000,\"message\":{\"message\":{\"role\":\"tool\",\"content\":[{\"type\":\"text\",\"text\":\"ASH_PROBE_TOKEN=ZANJIAO4271\"}],\"toolCallId\":\"tooluse_1\"},\"meta\":{\"source\":\"tool\"}}}\n",
                "{\"type\":\"agent.message.appended\",\"time\":1789634161578,\"message\":{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ZANJIAO4271\"}],\"toolCalls\":[]},\"meta\":{\"source\":\"llm\"}}}\n",
            ),
        )
        .unwrap();

        let (size, mtime_ms) = file_stat(&wire).unwrap();
        let source = SourceStat {
            tool: TOOL,
            key: wire.to_string_lossy().to_string(),
            path: Some(wire.clone()),
            size,
            mtime_ms,
            incremental: true,
        };
        let output = KimiAdapter.parse(&source, 0).unwrap().unwrap();
        let session_payload = output.session;

        assert_eq!(
            session_payload.session_id,
            "session_11111111-2222-3333-4444-555555555555"
        );
        assert_eq!(session_payload.cwd.as_deref(), Some("/tmp/demo"));
        // state.json 没有 title，用首条提问兜底
        assert_eq!(session_payload.title.as_deref(), Some("读一下 probe.txt"));
        let shapes: Vec<(&str, &str)> = session_payload
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.kind.as_str()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("user", "text"),
                ("assistant", "tool_use"),
                ("tool", "tool_result"),
                ("assistant", "text"),
            ]
        );
        assert!(session_payload.messages.iter().all(|m| m.ts.is_some()));

        // 追加一行后增量解析只拿新消息
        let (_, offset) = read_new_lines(&wire, 0).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            "{\"type\":\"agent.message.appended\",\"time\":1789634170000,\"message\":{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"再确认一次\"}]},\"meta\":{\"source\":\"input\"}}}\n".as_bytes(),
        )
        .unwrap();
        drop(file);
        let incremental = KimiAdapter.parse(&source, offset).unwrap().unwrap();
        assert_eq!(incremental.session.messages.len(), 1);
        assert!(!incremental.full_replace);

        std::fs::remove_dir_all(&dir).ok();
    }
}
