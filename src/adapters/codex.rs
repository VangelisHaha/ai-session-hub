//! Codex：`~/.codex/sessions/**/rollout-*.jsonl`（含 `archived_sessions`）
//!
//! 行结构：`{timestamp, type, payload}`；`type=session_meta` 带 id/cwd，
//! `type=response_item` 的 payload 再按 message / function_call / function_call_output 分流。

use super::{
    collect_files, extract_text, file_stat, forward_fill_ts, looks_like_title, parse_ts,
    read_new_lines, uuid_from_filename, Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct CodexAdapter;

pub const TOOL: &str = "codex";

/// 这些包裹内容不是真正的用户提问，不能当标题
const NOISE_PREFIXES: [&str; 4] = [
    "# AGENTS.md",
    "<environment_context>",
    "<user_instructions>",
    "# Context from my IDE setup:",
];

impl Adapter for CodexAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let mut files = Vec::new();
        for root in crate::paths::codex_session_dirs() {
            collect_files(&root, "jsonl", &mut files);
        }
        files
            .into_iter()
            .filter(|path| {
                // 会话索引文件不是会话内容
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name != "session_index.jsonl")
                    .unwrap_or(false)
            })
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
            .ok_or_else(|| anyhow!("Codex 源缺少文件路径"))?;
        let (lines, new_offset) = read_new_lines(path, from_offset)?;

        let mut session_id = None;
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
            let line_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let Some(payload) = value.get("payload") else {
                continue;
            };

            match line_type {
                "session_meta" => {
                    if session_id.is_none() {
                        session_id = payload
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if cwd.is_none() {
                        cwd = payload
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                "turn_context" if cwd.is_none() => {
                    cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                "response_item" => {
                    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                    match payload_type {
                        "message" => {
                            let role = match payload.get("role").and_then(Value::as_str) {
                                Some("user") => ROLE_USER,
                                Some("assistant") => ROLE_ASSISTANT,
                                _ => continue,
                            };
                            let text = payload.get("content").map(extract_text).unwrap_or_default();
                            if text.trim().is_empty() {
                                continue;
                            }
                            if role == ROLE_USER
                                && title.is_none()
                                && !is_noise(&text)
                                && looks_like_title(&text)
                            {
                                title = first_line_summary(&text, 120);
                            }
                            messages.push(Message::text(role, text, ts));
                        }
                        "function_call" | "local_shell_call" | "custom_tool_call" => {
                            let name = payload
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let args = payload
                                .get("arguments")
                                .or_else(|| payload.get("input"))
                                .or_else(|| payload.get("action"))
                                .map(|value| match value {
                                    Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                })
                                .unwrap_or_default();
                            messages.push(Message::tool_use(name, args, ts));
                        }
                        "function_call_output" | "custom_tool_call_output" => {
                            let output = payload
                                .get("output")
                                .map(|value| match value {
                                    Value::String(text) => text.clone(),
                                    other => extract_text(other),
                                })
                                .unwrap_or_default();
                            if !output.trim().is_empty() {
                                messages.push(Message::tool_result(None, output, ts));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        forward_fill_ts(&mut messages, created_at);

        let session_id = session_id.or_else(|| uuid_from_filename(path));
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

fn is_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    NOISE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} resume {session_id}")
}
