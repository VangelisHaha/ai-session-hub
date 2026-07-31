//! 规则化摘要：不调用任何模型，纯结构化裁剪。
//!
//! 设计取舍：摘要的消费者本身就是 AI（调用方），它需要的是**可信的原材料**而不是二次概括。
//! 所以这里只做确定性的抽取（提问清单、命令、文件、待办信号），把"理解"留给调用方，
//! 既没有幻觉风险，也不产生额外 token 成本。

use crate::index::MessageRow;
use crate::model::{SessionRow, KIND_TEXT, KIND_TOOL_USE, ROLE_ASSISTANT, ROLE_USER};
use serde::Serialize;
use serde_json::Value;

/// 命令类工具名（各家叫法不同）
const SHELL_TOOLS: [&str; 7] = [
    "shell",
    "bash",
    "Bash",
    "execute_bash",
    "run_command",
    "local_shell",
    "container.exec",
];

/// 待办信号词
const OPEN_SIGNALS: [&str; 8] = [
    "待确认",
    "待补",
    "下一步",
    "未完成",
    "TODO",
    "尚未",
    "遗留",
    "还需",
];

#[derive(Debug, Serialize)]
pub struct Digest {
    pub uid: String,
    pub tool: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub message_count: i64,
    pub user_requests: Vec<String>,
    pub assistant_points: Vec<String>,
    pub commands: Vec<String>,
    pub files: Vec<String>,
    pub open_items: Vec<String>,
    pub resume_command: Option<String>,
}

pub fn build(session: &SessionRow, messages: &[MessageRow]) -> Digest {
    let mut user_requests = Vec::new();
    let mut assistant_points = Vec::new();
    let mut commands = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut open_items = Vec::new();

    for message in messages {
        match (message.role.as_str(), message.kind.as_str()) {
            (ROLE_USER, KIND_TEXT) => {
                let cleaned = strip_prompt_wrapper(&message.content);
                if let Some(line) = crate::model::first_line_summary(&cleaned, 160) {
                    push_unique(&mut user_requests, line, 30);
                }
                collect_paths(&cleaned, &mut files);
            }
            (ROLE_ASSISTANT, KIND_TEXT) => {
                if let Some(line) = crate::model::first_line_summary(&message.content, 200) {
                    push_unique(&mut assistant_points, line, 40);
                }
                collect_paths(&message.content, &mut files);
                for sentence in split_sentences(&message.content) {
                    if OPEN_SIGNALS.iter().any(|signal| sentence.contains(signal)) {
                        push_unique(
                            &mut open_items,
                            crate::model::truncate_chars(&sentence, 180),
                            15,
                        );
                    }
                }
            }
            (_, KIND_TOOL_USE) => {
                if let Some(command) = extract_command(message) {
                    push_unique(
                        &mut commands,
                        crate::model::truncate_chars(&command, 240),
                        40,
                    );
                }
            }
            _ => {}
        }
    }

    // 助手要点只保留最近的若干条，早期铺垫价值低
    if assistant_points.len() > 12 {
        assistant_points = assistant_points.split_off(assistant_points.len() - 12);
    }

    Digest {
        uid: session.uid.clone(),
        tool: session.tool.clone(),
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        message_count: session.message_count,
        user_requests,
        assistant_points,
        commands,
        files,
        open_items,
        resume_command: session.resume_command.clone(),
    }
}

impl Digest {
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# 会话摘要 {}\n\n- 工具：{}\n- 会话：`{}`\n",
            self.title.clone().unwrap_or_else(|| self.uid.clone()),
            self.tool,
            self.uid
        ));
        if let Some(cwd) = &self.cwd {
            out.push_str(&format!("- 工程目录：`{cwd}`\n"));
        }
        out.push_str(&format!(
            "- 时间：{} → {}\n- 消息数：{}\n",
            fmt_ts(self.created_at),
            fmt_ts(self.updated_at),
            self.message_count
        ));
        if let Some(resume) = &self.resume_command {
            out.push_str(&format!("- 原生续聊：`{resume}`\n"));
        }

        section(&mut out, "用户诉求（按时间序）", &self.user_requests, true);
        section(
            &mut out,
            "助手关键结论（最近）",
            &self.assistant_points,
            true,
        );
        section(&mut out, "执行过的命令", &self.commands, false);
        section(&mut out, "涉及的文件与路径", &self.files, false);
        section(&mut out, "可能未完成的事项", &self.open_items, true);
        out
    }
}

fn section(out: &mut String, title: &str, items: &[String], numbered: bool) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title}\n\n"));
    for (idx, item) in items.iter().enumerate() {
        if numbered {
            out.push_str(&format!("{}. {}\n", idx + 1, item));
        } else {
            out.push_str(&format!("- `{}`\n", item.replace('`', "'")));
        }
    }
}

fn fmt_ts(ts: Option<i64>) -> String {
    match ts {
        Some(millis) => chrono::DateTime::from_timestamp_millis(millis)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| millis.to_string()),
        None => "未知".to_string(),
    }
}

fn push_unique(target: &mut Vec<String>, value: String, cap: usize) {
    if value.trim().is_empty() || target.len() >= cap || target.iter().any(|item| item == &value) {
        return;
    }
    target.push(value);
}

fn extract_command(message: &MessageRow) -> Option<String> {
    let tool_name = message.tool_name.as_deref().unwrap_or("");
    let payload: Option<Value> = serde_json::from_str(&message.content).ok();
    if SHELL_TOOLS
        .iter()
        .any(|name| tool_name.eq_ignore_ascii_case(name))
    {
        if let Some(value) = &payload {
            for key in ["command", "cmd", "script"] {
                if let Some(command) = value.get(key).and_then(Value::as_str) {
                    return Some(command.trim().to_string());
                }
            }
        }
        // 入库时长命令会被截断，JSON 解析不出来，退回抓 "command" 字段的原文
        if let Some(command) = scrape_json_string_field(&message.content, "command") {
            return Some(command);
        }
        return Some(message.content.trim().to_string());
    }
    // 非命令类工具，记录一行 "工具名 参数摘要"，便于回溯做过什么
    let brief = payload
        .as_ref()
        .and_then(|value| {
            for key in ["path", "file_path", "query", "pattern", "sql", "url"] {
                if let Some(text) = value.get(key).and_then(Value::as_str) {
                    return Some(text.to_string());
                }
            }
            None
        })
        .unwrap_or_default();
    if tool_name.is_empty() {
        None
    } else if brief.is_empty() {
        Some(format!("[{tool_name}]"))
    } else {
        Some(format!("[{tool_name}] {brief}"))
    }
}

/// 从残缺的 JSON 文本里抠出某个字符串字段的值（处理入库截断的情况）
fn scrape_json_string_field(text: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('n') | Some('t') | Some('r') => out.push(' '),
                Some(other) => out.push(other),
                None => break,
            },
            '"' => break,
            other => out.push(other),
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 抓取形似路径的 token（相对/绝对路径、带扩展名的文件）
fn collect_paths(text: &str, out: &mut Vec<String>) {
    for raw in text.split(|c: char| c.is_whitespace() || "\"'`,()[]{}<>：:；;".contains(c)) {
        let token = raw.trim_matches(|c: char| c == '.' || c == '，' || c == '。');
        if token.len() < 4 || token.len() > 200 {
            continue;
        }
        if token.starts_with("http://") || token.starts_with("https://") {
            continue;
        }
        let looks_like_path = token.contains('/') && !token.contains("//");
        let has_ext = token.rsplit_once('.').map(|(_, ext)| {
            (1..=6).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }) == Some(true);
        if looks_like_path && has_ext {
            push_unique(out, token.to_string(), 30);
        }
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    text.split(['\n', '。', '；', ';'])
        .map(|part| part.trim().to_string())
        .filter(|part| part.len() > 4)
        .collect()
}

/// 自动化包装（妮蔻的 SYSTEM_CONTEXT、IDE 注入的环境块）不是用户真实诉求
pub fn strip_prompt_wrapper(text: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[SYSTEM_CONTEXT_BEGIN]")
            || trimmed.starts_with("--- CONTEXT ENTRY BEGIN ---")
            || trimmed.starts_with("<environment_context>")
        {
            skipping = true;
            continue;
        }
        if trimmed.starts_with("[SYSTEM_CONTEXT_END]")
            || trimmed.starts_with("--- CONTEXT ENTRY END ---")
            || trimmed.starts_with("</environment_context>")
        {
            skipping = false;
            continue;
        }
        if skipping || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("--- USER MESSAGE")
            || trimmed == "用户问题:"
            || trimmed == "要求："
            || trimmed == "注意："
            || trimmed.starts_with("* ")
        {
            continue;
        }
        out.push(trimmed.to_string());
    }
    if out.is_empty() {
        text.trim().to_string()
    } else {
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, kind: &str, content: &str, tool: Option<&str>) -> MessageRow {
        MessageRow {
            seq: 0,
            role: role.to_string(),
            kind: kind.to_string(),
            tool_name: tool.map(str::to_string),
            ts: Some(1_700_000_000_000),
            content: content.to_string(),
        }
    }

    fn session() -> SessionRow {
        SessionRow {
            uid: "kiro:s1".to_string(),
            tool: "kiro".to_string(),
            session_id: "s1".to_string(),
            title: Some("刷新额度".to_string()),
            cwd: Some("/Users/demo/repo".to_string()),
            created_at: Some(1_700_000_000_000),
            updated_at: Some(1_700_000_900_000),
            source_path: None,
            message_count: 4,
            resume_command: Some("kiro-cli chat --resume-id s1".to_string()),
        }
    }

    #[test]
    fn digest_extracts_requests_commands_files_and_open_items() {
        let messages = vec![
            msg(
                "user",
                "text",
                "要求：\n* 简洁\n用户问题:\n刷新一下 src/index.rs 里的逻辑",
                None,
            ),
            msg(
                "assistant",
                "tool_use",
                "{\"command\":\"cargo test --all\"}",
                Some("shell"),
            ),
            msg(
                "assistant",
                "text",
                "已经改完 src/index.rs。下一步还需补集成测试。",
                None,
            ),
        ];
        let digest = build(&session(), &messages);
        assert_eq!(digest.user_requests.len(), 1);
        assert!(digest.user_requests[0].contains("刷新一下"));
        assert!(!digest.user_requests[0].contains("要求"));
        assert_eq!(digest.commands, vec!["cargo test --all".to_string()]);
        assert!(digest.files.contains(&"src/index.rs".to_string()));
        assert_eq!(digest.open_items.len(), 1);

        let markdown = digest.render_markdown();
        assert!(markdown.contains("## 用户诉求"));
        assert!(markdown.contains("kiro-cli chat --resume-id s1"));
    }

    #[test]
    fn truncated_tool_use_still_yields_the_command() {
        let raw =
            "{\"command\":\"cargo test --all\\n && echo ok\",\"working_dir\":\"/tmp…[truncated]";
        let message = msg("assistant", "tool_use", raw, Some("shell"));
        assert_eq!(
            extract_command(&message).as_deref(),
            Some("cargo test --all  && echo ok")
        );
    }

    #[test]
    fn urls_are_not_treated_as_files() {
        let mut files = Vec::new();
        collect_paths("见 https://example.com/a.html 与 src/main.rs", &mut files);
        assert_eq!(files, vec!["src/main.rs".to_string()]);
    }
}
