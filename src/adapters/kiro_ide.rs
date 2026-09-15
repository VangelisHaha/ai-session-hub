//! Kiro IDE（VS Code 扩展 kiro.kiroagent）的聊天会话。
//!
//! 存储位置（每个工作区一个目录，目录名是工作区路径的 base64url）：
//! ```text
//! macOS: ~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/workspace-sessions/<b64>/
//! Linux: ~/.config/Kiro/User/globalStorage/kiro.kiroagent/workspace-sessions/<b64>/
//!   sessions.json        索引：[{sessionId, title, dateCreated, workspaceDirectory}]
//!   <sessionId>.json     正文：{history:[{message:{role, content}, ...}], title, sessionId, workspacePath}
//! ```
//!
//! 与 Kiro CLI 的差异：
//! - 整个 JSON 每轮重写，所以走全量重解析（同 Gemini），不做字节偏移增量。
//! - 消息级没有时间戳，只有 `sessions.json` 里的 `dateCreated`；updated_at 用文件 mtime。
//! - 会话 ID 与 CLI 不通用，无法用 `kiro-cli chat --resume-id` 恢复，只能在 IDE 里打开。
//!
//! `message.content` 既可能是字符串（助手），也可能是 `[{type:"text", text}]`（用户）。
//! 工具调用在本机样本里没出现过，按 Continue 系的 `toolCallState` 形状做容错解析：
//! 缺字段就跳过，不影响正文入库。
//!
//! 已知限制（源数据本身如此，不是解析问题）：
//! - agent 模式下助手回复常被写成占位符（如 `On it.`），真实回复不落盘；
//!   `promptLogs[].completion` 也是空的。用户提问、标题、工作区路径是完整的。
//! - 消息级没有时间戳。同目录 hash 文件里的 `executions` 只有 executionId 与起止时间，
//!   要靠未公开的 hash 规则才能关联，收益有限，这里不做关联。

use super::{
    extract_text, file_stat, forward_fill_ts, looks_like_title, parse_ts, uuid_from_filename,
    Adapter, ParseOutput, SourceStat,
};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::Path;

pub struct KiroIdeAdapter;

pub const TOOL: &str = "kiro-ide";

const SESSION_INDEX: &str = "sessions.json";

impl Adapter for KiroIdeAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let mut sources = Vec::new();
        for root in crate::paths::kiro_ide_session_dirs() {
            let Ok(workspaces) = std::fs::read_dir(&root) else {
                continue;
            };
            for workspace in workspaces.flatten() {
                let Ok(entries) = std::fs::read_dir(workspace.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if path.file_name().and_then(|name| name.to_str()) == Some(SESSION_INDEX) {
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
        }
        sources
    }

    fn parse(&self, source: &SourceStat, _from_offset: u64) -> Result<Option<ParseOutput>> {
        let path = source
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("Kiro IDE 源缺少文件路径"))?;
        let text = std::fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&text)?;

        let Some(session_id) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| uuid_from_filename(path))
        else {
            return Ok(None);
        };

        let index_entry = read_index_entry(path, &session_id);
        let created_at = index_entry
            .as_ref()
            .and_then(|entry| entry.get("dateCreated").map(parse_ts))
            .flatten();
        let updated_at = source.mtime_ms.checked_abs().filter(|ms| *ms > 0);

        let cwd = value
            .get("workspacePath")
            .and_then(Value::as_str)
            .or_else(|| {
                index_entry
                    .as_ref()
                    .and_then(|entry| entry.get("workspaceDirectory").and_then(Value::as_str))
            })
            .map(str::to_string)
            .filter(|value| !value.is_empty());

        let mut title = value
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| looks_like_title(value))
            .and_then(|value| first_line_summary(value, 120));

        let mut messages = Vec::new();
        for item in value
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let message = item.get("message").unwrap_or(&Value::Null);
            let role = match message.get("role").and_then(Value::as_str) {
                Some("user") => ROLE_USER,
                Some("assistant") => ROLE_ASSISTANT,
                // system / tool 角色不入库：前者是模板噪声，后者由 toolCallState 承载
                _ => {
                    push_tool_messages(&item, &mut messages);
                    continue;
                }
            };
            let content = message_text(message.get("content"));
            if !content.trim().is_empty() {
                if role == ROLE_USER && title.is_none() {
                    title = first_line_summary(&content, 120);
                }
                messages.push(Message::text(role, content, None));
            }
            push_tool_messages(&item, &mut messages);
        }

        // IDE 的历史里没有任何消息级时间戳，统一落在会话创建时间上
        forward_fill_ts(&mut messages, created_at);

        Ok(Some(ParseOutput {
            session: SessionPayload {
                tool: TOOL.to_string(),
                session_id,
                title,
                cwd,
                created_at,
                updated_at: updated_at.or(created_at),
                source_path: path.to_string_lossy().to_string(),
                messages,
            },
            new_offset: 0,
            full_replace: true,
        }))
    }
}

/// content 可能是字符串，也可能是 `[{type:"text", text}]` 这样的块数组
fn message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // imageUrl 之类的非文本块不入库，避免把 base64 塞进索引
                Some(_) => String::new(),
                None => extract_text(block),
            })
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => extract_text(other),
        None => String::new(),
    }
}

/// 容错解析工具调用：`toolCallState` 或 `toolCallStates`，
/// 形如 `{toolCall:{function:{name, arguments}}, output:[...]}`
fn push_tool_messages(item: &Value, messages: &mut Vec<Message>) {
    let mut states = Vec::new();
    if let Some(state) = item.get("toolCallState") {
        states.push(state);
    }
    if let Some(list) = item.get("toolCallStates").and_then(Value::as_array) {
        states.extend(list.iter());
    }

    for state in states {
        let call = state.get("toolCall").unwrap_or(&Value::Null);
        let function = call.get("function").unwrap_or(&Value::Null);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| call.get("name").and_then(Value::as_str))
            .unwrap_or("unknown");
        let arguments = function
            .get("arguments")
            .map(stringify)
            .or_else(|| state.get("parsedArgs").map(stringify))
            .unwrap_or_default();
        if !arguments.trim().is_empty() || function.get("name").is_some() {
            messages.push(Message::tool_use(name, arguments, None));
        }

        if let Some(output) = state.get("output") {
            let text = extract_text(output);
            if !text.trim().is_empty() {
                messages.push(Message::tool_result(Some(name), text, None));
            }
        }
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 同目录 `sessions.json` 里取该会话的索引项（创建时间、工作区路径）
fn read_index_entry(path: &Path, session_id: &str) -> Option<Value> {
    let index_path = path.parent()?.join(SESSION_INDEX);
    let text = std::fs::read_to_string(index_path).ok()?;
    let entries: Vec<Value> = serde_json::from_str(&text).ok()?;
    entries
        .into_iter()
        .find(|entry| entry.get("sessionId").and_then(Value::as_str) == Some(session_id))
}

/// IDE 会话没有命令行入口，只能提示在 IDE 里打开
pub fn resume_command(session_id: &str) -> String {
    format!(
        "# Kiro IDE 会话无法用命令行恢复：请在 Kiro IDE 中打开对应工作区，\
         从聊天历史里选择会话 {session_id}（可先用 session_read 拿到正文）"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_blocks_and_assistant_string_are_both_read() {
        let content = serde_json::json!([
            {"type": "text", "text": "第一段"},
            {"type": "imageUrl", "imageUrl": {"url": "data:image/png;base64,AAAA"}},
            {"type": "text", "text": "第二段"}
        ]);
        assert_eq!(message_text(Some(&content)), "第一段\n第二段");
        assert_eq!(
            message_text(Some(&serde_json::json!("助手回复"))),
            "助手回复"
        );
        assert_eq!(message_text(None), "");
    }

    #[test]
    fn tool_call_state_is_best_effort_parsed() {
        let item = serde_json::json!({
            "toolCallState": {
                "toolCall": {"function": {"name": "fsWrite", "arguments": "{\"path\":\"a.rs\"}"}},
                "output": [{"content": "written"}]
            }
        });
        let mut messages = Vec::new();
        push_tool_messages(&item, &mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool_name.as_deref(), Some("fsWrite"));
        assert_eq!(messages[0].kind, crate::model::KIND_TOOL_USE);
        assert_eq!(messages[1].kind, crate::model::KIND_TOOL_RESULT);
        assert_eq!(messages[1].content, "written");
    }

    #[test]
    fn history_without_tool_calls_is_ignored_quietly() {
        let mut messages = Vec::new();
        push_tool_messages(
            &serde_json::json!({"message": {"role": "user"}}),
            &mut messages,
        );
        assert!(messages.is_empty());
    }
}
