//! 各工具的恢复 / 交接命令生成。
//!
//! **同工具**用原生 resume（无损，模型自己带全历史）；
//! **跨工具**无法迁移原生会话（历史格式、tool_use id、模型都不通），
//! 所以统一走「新会话 + 交接包」：把交接文件路径塞进第一条 prompt。

use crate::adapters::{claude, codex, gemini, kimi, kiro, kiro_ide, opencode};

pub const SUPPORTED_TOOLS: [&str; 7] = [
    "claude", "codex", "kiro", "kiro-ide", "kimi", "gemini", "opencode",
];

pub fn resume_command(tool: &str, session_id: &str) -> String {
    match tool {
        "claude" => claude::resume_command(session_id),
        "codex" => codex::resume_command(session_id),
        "kiro" => kiro::resume_command(session_id),
        "kiro-ide" => kiro_ide::resume_command(session_id),
        "kimi" => kimi::resume_command(session_id),
        "gemini" => gemini::resume_command(session_id),
        "opencode" => opencode::resume_command(session_id),
        other => format!("# 未知工具 {other}，无法生成恢复命令"),
    }
}

/// 同工具续聊时，支持直接把新指令一起带上（三家都支持 resume + 首个 prompt）
pub fn resume_with_prompt(tool: &str, session_id: &str, prompt: &str) -> String {
    let quoted = shell_quote(prompt);
    match tool {
        "claude" => format!("claude --resume {session_id} {quoted}"),
        "codex" => format!("codex exec resume {session_id} {quoted}"),
        "kiro" => format!("kiro-cli chat --resume-id {session_id} {quoted}"),
        // IDE 会话没有命令行 resume，退化成「交接给 Kiro CLI」
        "kiro-ide" => format!("kiro-cli chat {quoted}"),
        // kimi 不接受位置参数形式的 prompt，只能走 -p 一次性模式
        "kimi" => format!("kimi -r {session_id} -p {quoted}"),
        "gemini" => format!("gemini --resume {session_id} {quoted}"),
        "opencode" => format!("opencode run -s {session_id} {quoted}"),
        other => format!("# 未知工具 {other}"),
    }
}

pub fn handoff_command(tool: &str, brief_path: &str, note: &str) -> String {
    let prompt = format!("先读 {brief_path} 了解上下文，然后继续：{note}");
    let quoted = shell_quote(&prompt);
    match tool {
        "claude" => format!("claude {quoted}"),
        "codex" => format!("codex {quoted}"),
        "kiro" => format!("kiro-cli chat {quoted}"),
        // Kiro IDE 只能手动开会话，给出要粘贴的 prompt
        "kiro-ide" => {
            format!(
                "# 在 Kiro IDE 中新建聊天并粘贴：先读 {brief_path} 了解上下文，然后继续：{note}"
            )
        }
        "gemini" => format!("gemini {quoted}"),
        "kimi" => format!("kimi -p {quoted}"),
        "opencode" => format!("opencode run {quoted}"),
        other => format!("# 未知工具 {other}"),
    }
}

/// 单引号包裹，内部单引号按 shell 规则转义，避免 prompt 里的引号 / 反引号被展开
pub fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_commands_match_each_cli() {
        assert_eq!(resume_command("claude", "id1"), "claude --resume id1");
        assert_eq!(resume_command("codex", "id1"), "codex resume id1");
        assert_eq!(
            resume_command("kiro", "id1"),
            "kiro-cli chat --resume-id id1"
        );
        assert_eq!(resume_command("kimi", "session_id1"), "kimi -r session_id1");
    }

    #[test]
    fn kimi_prompts_go_through_the_p_flag() {
        assert_eq!(
            resume_with_prompt("kimi", "session_id1", "继续"),
            "kimi -r session_id1 -p '继续'"
        );
        assert_eq!(handoff_command("kimi", "/tmp/b.md", "收尾"), {
            let prompt = shell_quote("先读 /tmp/b.md 了解上下文，然后继续：收尾");
            format!("kimi -p {prompt}")
        });
    }

    #[test]
    fn prompts_are_shell_safe() {
        let cmd = resume_with_prompt("kiro", "id1", "别 `rm -rf /` 也别 'quote'");
        assert!(cmd.contains("'\\''quote'\\''"));
        assert!(cmd.starts_with("kiro-cli chat --resume-id id1 '"));
    }
}
