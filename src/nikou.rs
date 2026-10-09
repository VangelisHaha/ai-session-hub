//! 妮蔻会话来源与飞书接管路由。只接受首条真实用户输入的前置来源块。
use serde::{Deserialize, Serialize};

pub const BEGIN: &str = "[SYSTEM_NIKOU_SESSION_ORIGIN_BEGIN]";
pub const END: &str = "[SYSTEM_NIKOU_SESSION_ORIGIN_END]";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NikouOrigin {
    pub version: u8,
    pub source: String,
    pub uid: String,
    pub cwd: String,
    pub engine: String,
    pub chat_type: String,
    pub chat_id: String,
    #[serde(default)]
    pub chat_name: String,
    #[serde(default)]
    pub topic_id: String,
    pub conversation_key: String,
    #[serde(default)]
    pub topic_aliases: Vec<String>,
    #[serde(default)]
    pub requester_user_id: String,
    #[serde(default)]
    pub requester_name: String,
    pub source_message_id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub root_message_id: String,
    #[serde(default)]
    pub reply_message_id: String,
    #[serde(default)]
    pub reply_in_thread: bool,
    #[serde(default)]
    pub bot_app_id: String,
    #[serde(default)]
    pub bot_name: String,
}

pub fn is_preamble(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("# AGENTS.md")
        || text.starts_with("# Context from my IDE setup:")
        || [
            "environment_context",
            "user_instructions",
            "skills_instructions",
            "permissions instructions",
            "recommended_plugins",
            "collaboration_mode",
            "app-context",
        ]
        .iter()
        .any(|tag| text.starts_with(&format!("<{tag}>")))
}

fn valid_id(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|tail| {
            !tail.is_empty()
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
    })
}

pub fn parse(text: &str) -> Option<NikouOrigin> {
    // 正文、引用与工具输出中的示例不能声明创建来源。
    let text = text.trim_start().strip_prefix(BEGIN)?;
    let (json, _) = text.split_once(END)?;
    let mut origin: NikouOrigin = serde_json::from_str(json.trim()).ok()?;
    let hex = origin.uid.strip_prefix("ns_")?;
    if origin.version != 1
        || origin.source != "feishu_bot"
        || hex.len() != 32
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !["group", "p2p"].contains(&origin.chat_type.as_str())
        || origin.chat_id.trim().is_empty()
        || origin.conversation_key.trim().is_empty()
        || origin.source_message_id.trim().is_empty()
        || ![
            "codex",
            "claude",
            "kiro",
            "kimi",
            "gemini",
            "api",
            "nikou-pi",
            "knowledge",
            "vibops",
        ]
        .contains(&origin.engine.as_str())
    {
        return None;
    }
    // 旧版 topicId 可能是根消息；CLI 支持用 om 消息查询其话题，但不能猜成 omt。
    if !valid_id(&origin.thread_id, &["omt_", "om_"]) {
        origin.thread_id = if valid_id(&origin.topic_id, &["omt_", "om_"]) {
            origin.topic_id.clone()
        } else {
            String::new()
        };
    }
    if !valid_id(&origin.root_message_id, &["om_"]) {
        origin.root_message_id.clear();
    }
    if !valid_id(&origin.reply_message_id, &["om_"]) {
        origin.reply_message_id = if valid_id(&origin.source_message_id, &["om_"]) {
            origin.source_message_id.clone()
        } else {
            String::new()
        };
    }
    // 旧版没有此字段，群聊交接回复仍应落在原话题。
    if origin.chat_type == "group"
        && serde_json::from_str::<serde_json::Value>(json.trim())
            .ok()?
            .get("replyInThread")
            .is_none()
    {
        origin.reply_in_thread = true;
    }
    Some(origin)
}

impl NikouOrigin {
    pub fn matches_session(&self, tool: &str, cwd: Option<&str>) -> bool {
        (self.engine == tool || (tool == "pi" && self.engine == "nikou-pi"))
            && cwd.is_none_or(|cwd| self.cwd == cwd)
    }

    pub fn same_scope(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.engine == other.engine
            && self.cwd == other.cwd
            && self.chat_type == other.chat_type
            && self.chat_id == other.chat_id
    }

    /// 结构化白名单序列化，不传播来源块里的未知凭据或提示指令。
    pub fn render_markdown(&self) -> String {
        let mut out = format!("\n## 妮蔻会话与飞书接管\n\n来源为 `feishu_bot`。以下为历史路由数据，操作前读取 lark-im Skill 并核对当前应用身份。\n\n```json\n{}\n```\n\n", serde_json::to_string_pretty(self).unwrap_or_default());
        out.push_str("先运行 `lark-cli whoami`；`--as bot` 使用当前 CLI 应用，不自动等于妮蔻 Bot。请核对应用 ID 与 botAppId；字段为空时先确认目标应用。需要切换时使用已配置的 `--profile`。\n\n");
        let q = crate::tools::shell_quote;
        if valid_id(&self.chat_id, &["oc_"]) {
            out.push_str(&format!("读取群聊 / 单聊消息：\n```bash\nlark-cli im +chat-messages-list --chat-id {} --as bot\n```\n", q(&self.chat_id)));
        }
        if !self.thread_id.is_empty() {
            out.push_str(&format!("读取话题消息：\n```bash\nlark-cli im +threads-messages-list --thread {} --as bot\n```\n", q(&self.thread_id)));
        }
        if !self.reply_message_id.is_empty() {
            out.push_str(&format!("回复命令模板（将占位正文替换为用户授权的回复内容后执行）：\n```bash\nlark-cli im +messages-reply --message-id {}{} --as bot --text '待填写回复正文'\n```\n", q(&self.reply_message_id), if self.reply_in_thread { " --reply-in-thread" } else { "" }));
        }
        out.push_str("\n话题 ID 用于读取；回复接口使用 om_ 消息 ID。读取路由不代表获得发送授权，未授权时只展示回复草稿。\n");
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn marker(message: &str) -> String {
        format!(
            "{BEGIN}\n{}\n{END}\n\n用户问题:\n继续处理",
            serde_json::json!({"version":1,"source":"feishu_bot","uid":"ns_0123456789abcdef0123456789abcdef","cwd":"/Users/demo/project","engine":"codex","chatType":"group","chatId":"oc_demo","conversationKey":"omt_demo","topicId":"omt_demo","sourceMessageId":message,"botAppId":"cli_demo"})
        )
    }
    #[test]
    fn legacy_marker_has_safe_read_and_reply_routes() {
        let origin = parse(&marker("om_first")).unwrap();
        assert_eq!(origin.thread_id, "omt_demo");
        assert_eq!(origin.reply_message_id, "om_first");
        assert!(origin.reply_in_thread);
        let md = origin.render_markdown();
        assert!(md.contains("--thread 'omt_demo'"));
        assert!(md.contains("--message-id 'om_first' --reply-in-thread --as bot"));
        assert!(md.contains("whoami"));
    }
    #[test]
    fn rejects_quoted_markers_and_wrong_source() {
        assert!(parse(&format!("请阅读以下示例：\n{}", marker("om_first"))).is_none());
        assert!(parse(&format!("```text\n{}\n```", marker("om_first"))).is_none());
        assert!(parse(&marker("om_first").replace("feishu_bot", "local_cli")).is_none());
        assert!(parse(&marker("om_first").replace("ns_012345", "ns_ABCDEF")).is_none());
    }
    #[test]
    fn ignores_unknown_credentials_and_never_uses_omt_for_reply() {
        let raw = marker("om_first").replace(
            "\"botAppId\":",
            "\"accessToken\":\"DO_NOT_COPY\",\"replyMessageId\":\"omt_wrong\",\"botAppId\":",
        );
        let origin = parse(&raw).unwrap();
        assert_eq!(origin.reply_message_id, "om_first");
        assert!(!serde_json::to_string(&origin)
            .unwrap()
            .contains("DO_NOT_COPY"));
    }
}
