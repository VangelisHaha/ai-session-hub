//! Kiro CLI：`~/.kiro/sessions/cli/<session-id>.jsonl` + 同名 `.json` 元信息
//!
//! JSONL 行结构：`{version, kind, data{message_id, content[], meta{timestamp}}}`，
//! kind 为 Prompt / AssistantMessage / ToolResults；content 内的 kind 为 text / toolUse / toolResult。
//! 同名 `.json` 里有 session_id / cwd / title / created_at / updated_at，直接拿来做元信息。

use super::{
    file_stat, forward_fill_ts, looks_like_title, parse_ts, read_new_lines, uuid_from_filename,
    Adapter, ParseOutput, SourceStat,
};
use crate::model::{
    first_line_summary, truncate_chars, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER,
};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::Path;

pub struct KiroAdapter;

pub const TOOL: &str = "kiro";

impl Adapter for KiroAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let root = crate::paths::kiro_sessions_dir();
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    return None;
                }
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
            .ok_or_else(|| anyhow!("Kiro 源缺少文件路径"))?;
        let (lines, new_offset) = read_new_lines(path, from_offset)?;
        let sidecar = read_sidecar(path);

        let mut session_id = sidecar
            .as_ref()
            .and_then(|meta| meta.get("session_id").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| uuid_from_filename(path));
        let cwd = sidecar
            .as_ref()
            .and_then(|meta| meta.get("cwd").and_then(Value::as_str))
            .map(str::to_string);
        let mut created_at = sidecar
            .as_ref()
            .and_then(|meta| meta.get("created_at").map(parse_ts))
            .flatten();
        let mut updated_at = sidecar
            .as_ref()
            .and_then(|meta| meta.get("updated_at").map(parse_ts))
            .flatten();
        let mut title = sidecar
            .as_ref()
            .and_then(|meta| meta.get("title").and_then(Value::as_str))
            .map(strip_wrapper)
            // Kiro 自带的标题常常就是提示词包裹的首行（如"要求："），这种要丢掉让首条提问兜底
            .filter(|value| looks_like_title(value) && value.chars().count() >= 4)
            .and_then(|value| first_line_summary(&value, 120));

        let mut messages = Vec::new();
        for line in &lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(data) = value.get("data") else {
                continue;
            };
            let ts = data
                .get("meta")
                .and_then(|meta| meta.get("timestamp"))
                .and_then(parse_ts);
            if let Some(ts) = ts {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }
            let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
            let blocks = data
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            match kind {
                "Prompt" => {
                    let text = blocks
                        .iter()
                        .filter(|block| block.get("kind").and_then(Value::as_str) == Some("text"))
                        .filter_map(|block| block.get("data").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.trim().is_empty() {
                        continue;
                    }
                    if title.is_none() {
                        let candidate = strip_wrapper(&text);
                        if looks_like_title(&candidate) {
                            title = first_line_summary(&candidate, 120);
                        }
                    }
                    messages.push(Message::text(ROLE_USER, text, ts));
                }
                "AssistantMessage" => {
                    for block in &blocks {
                        match block.get("kind").and_then(Value::as_str) {
                            Some("text") => {
                                let text = block
                                    .get("data")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if !text.trim().is_empty() {
                                    messages.push(Message::text(ROLE_ASSISTANT, text, ts));
                                }
                            }
                            Some("toolUse") => {
                                let payload = block.get("data").cloned().unwrap_or(Value::Null);
                                let name = payload
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let input = payload
                                    .get("input")
                                    .map(|value| value.to_string())
                                    .unwrap_or_default();
                                messages.push(Message::tool_use(name, input, ts));
                            }
                            _ => {}
                        }
                    }
                }
                "ToolResults" => {
                    for block in &blocks {
                        if block.get("kind").and_then(Value::as_str) != Some("toolResult") {
                            continue;
                        }
                        let payload = block.get("data").cloned().unwrap_or(Value::Null);
                        let text = flatten_tool_result(&payload);
                        if !text.trim().is_empty() {
                            messages.push(Message::tool_result(None, text, ts));
                        }
                    }
                }
                _ => {}
            }
        }

        // Kiro 只在 Prompt 行写 timestamp，其余行补齐为上一条已知时间
        forward_fill_ts(&mut messages, created_at);

        if session_id.is_none() {
            session_id = uuid_from_filename(path);
        }
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

fn read_sidecar(path: &Path) -> Option<Value> {
    let json_path = path.with_extension("json");
    let text = std::fs::read_to_string(json_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Kiro 的 toolResult.content 是 `[{kind: json|text, data: ...}]`
fn flatten_tool_result(payload: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = payload.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("data").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                _ => {
                    if let Some(data) = item.get("data") {
                        parts.push(match data {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        });
                    }
                }
            }
        }
    }
    truncate_chars(&parts.join("\n"), crate::model::MAX_TOOL_RESULT_CHARS)
}

/// 妮蔻/自动化会在 prompt 前后包一层固定说明与 SYSTEM_CONTEXT，标题要跳过这些噪声
fn strip_wrapper(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_context = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[SYSTEM_CONTEXT_BEGIN]")
            || trimmed.starts_with("--- CONTEXT ENTRY BEGIN ---")
        {
            in_context = true;
            continue;
        }
        if trimmed.starts_with("[SYSTEM_CONTEXT_END]")
            || trimmed.starts_with("--- CONTEXT ENTRY END ---")
        {
            in_context = false;
            continue;
        }
        if in_context || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('*')
            || trimmed.starts_with("要求：")
            || trimmed.starts_with("注意：")
            || trimmed.starts_with("用户问题")
            || trimmed.starts_with("--- USER MESSAGE")
        {
            continue;
        }
        out.push(trimmed.to_string());
    }
    if out.is_empty() {
        text.to_string()
    } else {
        out.join("\n")
    }
}

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} chat --resume-id {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_noise_is_stripped_for_title() {
        let raw = "要求：\n* 简洁\n[SYSTEM_CONTEXT_BEGIN]\n会话元信息: xxx\n[SYSTEM_CONTEXT_END]\n用户问题:\n当前代码都提交了么";
        assert_eq!(strip_wrapper(raw), "当前代码都提交了么");
    }
}
