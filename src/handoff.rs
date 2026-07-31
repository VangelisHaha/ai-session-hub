//! 跨工具交接：产出人可读、AI 可直接消费的交接包（Markdown），并给出目标工具的启动命令。
//!
//! 为什么不直接迁移原生会话：各工具的历史格式、tool_use ID 体系、模型都不通，
//! 伪造对方的会话文件既脆弱又容易被对方的校验拒绝。交接包是显式、可复查、可重放的。

use crate::digest::Digest;
use crate::index::{Index, MessageRow, ReadOptions};
use crate::model::SessionRow;
use anyhow::{anyhow, Result};
use serde::Serialize;

/// 交接包默认带的最近消息条数与字符预算
pub const DEFAULT_TAIL: usize = 30;
pub const DEFAULT_BUDGET_CHARS: usize = 16_000;

#[derive(Debug, Serialize)]
pub struct HandoffResult {
    pub uid: String,
    pub from_tool: String,
    pub to_tool: String,
    pub brief_path: String,
    pub command: String,
    pub chars: usize,
    pub message_count: usize,
    /// 同工具时给出原生续聊命令（无损）
    pub native_resume: Option<String>,
}

pub fn create(
    index: &Index,
    uid: &str,
    to_tool: &str,
    note: &str,
    tail: Option<usize>,
    include_tool_results: bool,
) -> Result<HandoffResult> {
    if !crate::tools::SUPPORTED_TOOLS.contains(&to_tool) {
        return Err(anyhow!(
            "不支持的目标工具 {to_tool}，可选：{}",
            crate::tools::SUPPORTED_TOOLS.join(" / ")
        ));
    }
    let session = index
        .get_session(uid)?
        .ok_or_else(|| anyhow!("未找到会话 {uid}"))?;
    let messages = index.read_messages(
        uid,
        &ReadOptions {
            tail: Some(tail.unwrap_or(DEFAULT_TAIL)),
            from_seq: None,
            include_tool_results,
            // 交接包带的是"对话"，命令与文件已在摘要里结构化列出，
            // 再塞一堆 tool_use JSON 只会挤占接手方的上下文
            only_text: !include_tool_results,
            budget_chars: Some(DEFAULT_BUDGET_CHARS),
        },
    )?;
    // 摘要用全量消息（不含工具结果），要点才完整
    let all_messages = index.read_messages(
        uid,
        &ReadOptions {
            tail: None,
            from_seq: None,
            include_tool_results: false,
            only_text: false,
            budget_chars: None,
        },
    )?;
    let digest = crate::digest::build(&session, &all_messages);

    let body = render_brief(&session, &digest, &messages, note);
    let dir = crate::paths::handoff_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let file_name = format!(
        "{stamp}-{}-to-{}-{}.md",
        session.tool,
        to_tool,
        short_id(&session.session_id)
    );
    let path = dir.join(file_name);
    std::fs::write(&path, &body)?;
    let brief_path = path.to_string_lossy().to_string();

    let effective_note = if note.trim().is_empty() {
        "接着上文继续，先复述你对当前进度的理解再动手"
    } else {
        note
    };
    let command = crate::tools::handoff_command(to_tool, &brief_path, effective_note);
    let native_resume = if session.tool == to_tool {
        Some(crate::tools::resume_with_prompt(
            to_tool,
            &session.session_id,
            effective_note,
        ))
    } else {
        None
    };

    Ok(HandoffResult {
        uid: session.uid,
        from_tool: session.tool,
        to_tool: to_tool.to_string(),
        brief_path,
        command,
        chars: body.chars().count(),
        message_count: messages.len(),
        native_resume,
    })
}

fn render_brief(
    session: &SessionRow,
    digest: &Digest,
    messages: &[MessageRow],
    note: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# 会话交接包\n\n");
    out.push_str(&format!(
        "> 来源工具：**{}**　会话：`{}`　导出时间：{}\n",
        session.tool,
        session.uid,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str("> 本文件由 ai-session-hub 生成，供另一个 AI 工具接手上下文使用。\n");
    if !note.trim().is_empty() {
        out.push_str(&format!("\n**接手后的目标**：{note}\n"));
    }
    out.push('\n');
    out.push_str(&digest.render_markdown());
    out.push_str("\n\n## 最近对话原文\n");
    for message in messages {
        let who = match (message.role.as_str(), message.kind.as_str()) {
            ("user", _) => "用户".to_string(),
            ("assistant", "tool_use") => format!(
                "助手·调用工具 {}",
                message.tool_name.clone().unwrap_or_default()
            ),
            ("assistant", _) => "助手".to_string(),
            ("tool", _) => "工具结果".to_string(),
            (other, _) => other.to_string(),
        };
        out.push_str(&format!("\n### [{}] {}\n\n", message.seq, who));
        let content = crate::digest::strip_prompt_wrapper(&message.content);
        out.push_str(&content);
        out.push('\n');
    }
    out
}

fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_stable() {
        assert_eq!(short_id("b2daab07-0161-4897"), "b2daab07");
    }

    #[test]
    fn brief_contains_digest_and_raw_tail() {
        let session = SessionRow {
            uid: "kiro:s1".into(),
            tool: "kiro".into(),
            session_id: "s1".into(),
            title: Some("测试".into()),
            cwd: None,
            created_at: None,
            updated_at: None,
            source_path: None,
            message_count: 1,
            resume_command: Some("kiro-cli chat --resume-id s1".into()),
        };
        let messages = vec![MessageRow {
            seq: 3,
            role: "user".into(),
            kind: "text".into(),
            tool_name: None,
            ts: None,
            content: "继续做交接".into(),
        }];
        let digest = crate::digest::build(&session, &messages);
        let brief = render_brief(&session, &digest, &messages, "接着改代码");
        assert!(brief.contains("会话交接包"));
        assert!(brief.contains("接手后的目标"));
        assert!(brief.contains("### [3] 用户"));
    }
}
