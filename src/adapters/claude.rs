//! Claude Code：`~/.claude/projects/<编码后的 cwd>/<session-id>.jsonl`
//!
//! 每行都是一个事件，`type` 为 user / assistant 的行带 `message.content`
//! （字符串或 text/tool_use/tool_result 块数组），并且每行都冗余带了 `cwd` 与 `sessionId`，
//! 因此增量解析也能拿到会话元信息。

use super::{
    collect_files, extract_text, file_stat, forward_fill_ts, looks_like_title, parse_ts,
    read_new_lines, uuid_from_filename, Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct ClaudeAdapter;

pub const TOOL: &str = "claude";

impl Adapter for ClaudeAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let root = crate::paths::claude_projects_dir();
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
            .ok_or_else(|| anyhow!("Claude 源缺少文件路径"))?;
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

            // Claude 自己写的会话摘要，作为标题兜底
            if title.is_none() {
                if let Some(summary) = value.get("summary").and_then(Value::as_str) {
                    title = first_line_summary(summary, 120);
                }
            }

            let line_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let role = match line_type {
                "user" => ROLE_USER,
                "assistant" => ROLE_ASSISTANT,
                _ => continue,
            };
            let Some(message) = value.get("message") else {
                continue;
            };
            let Some(content) = message.get("content") else {
                continue;
            };
            match content {
                Value::String(text) if !text.trim().is_empty() => {
                    if role == ROLE_USER && title.is_none() && looks_like_title(text) {
                        title = first_line_summary(text, 120);
                    }
                    messages.push(Message::text(role, text.clone(), ts));
                }
                Value::Array(blocks) => {
                    for block in blocks {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                let text = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if text.trim().is_empty() {
                                    continue;
                                }
                                if role == ROLE_USER && title.is_none() && looks_like_title(text) {
                                    title = first_line_summary(text, 120);
                                }
                                messages.push(Message::text(role, text, ts));
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let input = block
                                    .get("input")
                                    .map(|value| value.to_string())
                                    .unwrap_or_default();
                                messages.push(Message::tool_use(name, input, ts));
                            }
                            "tool_result" => {
                                let text =
                                    block.get("content").map(extract_text).unwrap_or_default();
                                if !text.trim().is_empty() {
                                    messages.push(Message::tool_result(None, text, ts));
                                }
                            }
                            _ => {}
                        }
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

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} --resume {session_id}")
}
