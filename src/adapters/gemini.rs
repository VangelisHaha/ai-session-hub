//! Gemini CLI：`~/.gemini/tmp/<project>/chats/session-*.json`（单文件 JSON，整体重写）
//!
//! 结构：`{sessionId, startTime, lastUpdated, messages:[{type: user|gemini, content, toolCalls}]}`，
//! 同目录的 `.project_root` 保存工程路径。整文件重写，所以走全量重解析。

use super::{file_stat, forward_fill_ts, parse_ts, Adapter, ParseOutput, SourceStat};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct GeminiAdapter;

pub const TOOL: &str = "gemini";

impl Adapter for GeminiAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let root = crate::paths::gemini_tmp_dir();
        let Ok(projects) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        let mut sources = Vec::new();
        for project in projects.flatten() {
            let chats = project.path().join("chats");
            let Ok(entries) = std::fs::read_dir(&chats) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Some((size, mtime_ms)) = file_stat(&path) else {
                    continue;
                };
                sources.push(SourceStat {
                    tool: TOOL,
                    key: path.to_string_lossy().to_string(),
                    path: Some(path),
                    size,
                    mtime_ms,
                    incremental: false,
                });
            }
        }
        sources
    }

    fn parse(&self, source: &SourceStat, _from_offset: u64) -> Result<Option<ParseOutput>> {
        let path = source
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("Gemini 源缺少文件路径"))?;
        let text = std::fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&text)?;

        let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
            return Ok(None);
        };
        let created_at = value.get("startTime").and_then(parse_ts);
        let updated_at = value.get("lastUpdated").and_then(parse_ts).or(created_at);
        let cwd = path
            .parent()
            .and_then(|chats| chats.parent())
            .map(|project| project.join(".project_root"))
            .and_then(|marker| std::fs::read_to_string(marker).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let mut title = None;
        let mut messages = Vec::new();
        for item in value
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let role = match item.get("type").and_then(Value::as_str) {
                Some("user") => ROLE_USER,
                Some("gemini") => ROLE_ASSISTANT,
                _ => continue,
            };
            let ts = item.get("timestamp").and_then(parse_ts);
            let content = match item.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if !content.trim().is_empty() {
                if role == ROLE_USER && title.is_none() {
                    title = first_line_summary(&content, 120);
                }
                messages.push(Message::text(role, content, ts));
            }
            if let Some(calls) = item.get("toolCalls").and_then(Value::as_array) {
                for call in calls {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let args = call
                        .get("args")
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    messages.push(Message::tool_use(name, args, ts));
                }
            }
        }

        forward_fill_ts(&mut messages, created_at);

        Ok(Some(ParseOutput {
            session: SessionPayload {
                tool: TOOL.to_string(),
                session_id: session_id.to_string(),
                title,
                cwd,
                created_at,
                updated_at,
                source_path: path.to_string_lossy().to_string(),
                messages,
            },
            new_offset: 0,
            full_replace: true,
        }))
    }
}

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} --resume {session_id}")
}
