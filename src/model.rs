//! 统一会话模型：所有工具的原生格式都先归一到这里，索引与上层功能只认这套结构。

use serde::{Deserialize, Serialize};

/// 消息角色
pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_TOOL: &str = "tool";

/// 消息类型
pub const KIND_TEXT: &str = "text";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";

/// 各工具原生内容体积差异极大（Codex/Kiro 的工具结果单条能到几十 KB），
/// 入库前统一裁剪，索引只保留可检索、可回溯的部分。
pub const MAX_TEXT_CHARS: usize = 24_000;
pub const MAX_TOOL_USE_CHARS: usize = 600;
pub const MAX_TOOL_RESULT_CHARS: usize = 1_200;

/// 一条归一化后的消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub kind: String,
    /// 工具名（kind 为 tool_use / tool_result 时有值）
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
    pub content: String,
}

impl Message {
    pub fn text(role: &str, content: impl Into<String>, ts: Option<i64>) -> Self {
        Self {
            role: role.to_string(),
            kind: KIND_TEXT.to_string(),
            tool_name: None,
            ts,
            content: truncate_chars(&content.into(), MAX_TEXT_CHARS),
        }
    }

    pub fn tool_use(name: &str, content: impl Into<String>, ts: Option<i64>) -> Self {
        Self {
            role: ROLE_ASSISTANT.to_string(),
            kind: KIND_TOOL_USE.to_string(),
            tool_name: Some(name.to_string()),
            ts,
            content: truncate_chars(&content.into(), MAX_TOOL_USE_CHARS),
        }
    }

    pub fn tool_result(name: Option<&str>, content: impl Into<String>, ts: Option<i64>) -> Self {
        Self {
            role: ROLE_TOOL.to_string(),
            kind: KIND_TOOL_RESULT.to_string(),
            tool_name: name.map(str::to_string),
            ts,
            content: truncate_chars(&content.into(), MAX_TOOL_RESULT_CHARS),
        }
    }
}

/// 一个会话（含消息，adapter 解析产物）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPayload {
    pub tool: String,
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub source_path: String,
    pub messages: Vec<Message>,
}

impl SessionPayload {
    pub fn uid(&self) -> String {
        format!("{}:{}", self.tool, self.session_id)
    }
}

/// 会话元信息（索引查询产物）
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub uid: String,
    pub tool: String,
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub source_path: Option<String>,
    pub message_count: i64,
    pub resume_command: Option<String>,
}

/// 按字符数（而非字节）裁剪，避免切坏中文
pub fn truncate_chars(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max).collect();
    out.push_str("…[truncated]");
    out
}

/// 取首行并压缩空白，用于标题
pub fn first_line_summary(input: &str, max: usize) -> Option<String> {
    let line = input
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();
    let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(truncate_chars(&compact, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_chinese_intact() {
        let out = truncate_chars("一二三四五", 3);
        assert!(out.starts_with("一二三"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(
            first_line_summary("\n\n  改一下  代码 \n第二行", 100).as_deref(),
            Some("改一下 代码")
        );
    }

    #[test]
    fn tool_result_is_capped() {
        let long = "x".repeat(MAX_TOOL_RESULT_CHARS + 500);
        let msg = Message::tool_result(Some("shell"), long, None);
        assert!(msg.content.chars().count() <= MAX_TOOL_RESULT_CHARS + 20);
        assert_eq!(msg.role, ROLE_TOOL);
    }
}
