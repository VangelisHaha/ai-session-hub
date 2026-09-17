//! 各工具的恢复 / 交接命令生成。
//!
//! **同工具**用原生 resume（无损，模型自己带全历史）；
//! **跨工具**无法迁移原生会话（历史格式、tool_use id、模型都不通），
//! 所以统一走「新会话 + 交接包」：把交接文件路径塞进第一条 prompt。
//!
//! 命令里的可执行文件一律由 `launcher::resolve` 解析：Kimi、WorkBuddy 的 CLI
//! 默认不在 PATH 上，直接写裸名字生成出来的命令是跑不了的。

use crate::adapters::{claude, codex, gemini, kimi, kiro, kiro_ide, opencode, pi, workbuddy};

pub const SUPPORTED_TOOLS: [&str; 9] = [
    "claude",
    "codex",
    "kiro",
    "kiro-ide",
    "kimi",
    "pi",
    "workbuddy",
    "gemini",
    "opencode",
];

pub fn resume_command(tool: &str, session_id: &str) -> String {
    let bin = crate::launcher::resolve(tool);
    match tool {
        "claude" => claude::resume_command(&bin, session_id),
        "codex" => codex::resume_command(&bin, session_id),
        "kiro" => kiro::resume_command(&bin, session_id),
        "kiro-ide" => kiro_ide::resume_command(session_id),
        "kimi" => kimi::resume_command(&bin, session_id),
        "pi" => pi::resume_command(&bin, session_id),
        "workbuddy" => workbuddy::resume_command(&bin, session_id),
        "gemini" => gemini::resume_command(&bin, session_id),
        "opencode" => opencode::resume_command(&bin, session_id),
        other => format!("# 未知工具 {other}，无法生成恢复命令"),
    }
}

/// 同工具续聊时，支持直接把新指令一起带上（三家都支持 resume + 首个 prompt）
pub fn resume_with_prompt(tool: &str, session_id: &str, prompt: &str) -> String {
    let quoted = shell_quote(prompt);
    let bin = crate::launcher::resolve(tool);
    match tool {
        "claude" => format!("{bin} --resume {session_id} {quoted}"),
        "codex" => format!("{bin} exec resume {session_id} {quoted}"),
        "kiro" => format!("{bin} chat --resume-id {session_id} {quoted}"),
        // IDE 会话没有命令行 resume，退化成「交接给 Kiro CLI」
        "kiro-ide" => format!("{bin} chat {quoted}"),
        // kimi 不接受位置参数形式的 prompt，只能走 -p 一次性模式
        "kimi" => format!("{bin} -r {session_id} -p {quoted}"),
        "pi" => format!("{bin} --session {session_id} {quoted}"),
        "workbuddy" => format!("{bin} --resume {session_id} {quoted}"),
        "gemini" => format!("{bin} --resume {session_id} {quoted}"),
        "opencode" => format!("{bin} run -s {session_id} {quoted}"),
        other => format!("# 未知工具 {other}"),
    }
}

pub fn handoff_command(tool: &str, brief_path: &str, note: &str) -> String {
    let prompt = format!("先读 {brief_path} 了解上下文，然后继续：{note}");
    let quoted = shell_quote(&prompt);
    let bin = crate::launcher::resolve(tool);
    match tool {
        "claude" => format!("{bin} {quoted}"),
        "codex" => format!("{bin} {quoted}"),
        "kiro" => format!("{bin} chat {quoted}"),
        // Kiro IDE 只能手动开会话，给出要粘贴的 prompt
        "kiro-ide" => {
            format!(
                "# 在 Kiro IDE 中新建聊天并粘贴：先读 {brief_path} 了解上下文，然后继续：{note}"
            )
        }
        "gemini" => format!("{bin} {quoted}"),
        "kimi" => format!("{bin} -p {quoted}"),
        "pi" => format!("{bin} {quoted}"),
        "workbuddy" => format!("{bin} {quoted}"),
        "opencode" => format!("{bin} run {quoted}"),
        other => format!("# 未知工具 {other}"),
    }
}

/// 给 prompt 加引号，避免里面的引号 / 反引号 / `$` 被 shell 展开。
/// unix 用 POSIX sh 规则，Windows 用 PowerShell 规则（cmd.exe 不认单引号）。
pub fn shell_quote(input: &str) -> String {
    crate::platform::quote_with(crate::platform::quote_style(), input)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令里的可执行文件由 launcher 解析（本机装在哪就用哪），
    /// 所以断言只校验**参数形状**，不把裸名字写死，否则换台机器就红。
    #[test]
    fn resume_commands_match_each_cli() {
        let bin = |tool: &str| crate::launcher::resolve(tool);
        assert_eq!(
            resume_command("claude", "id1"),
            format!("{} --resume id1", bin("claude"))
        );
        assert_eq!(
            resume_command("codex", "id1"),
            format!("{} resume id1", bin("codex"))
        );
        assert_eq!(
            resume_command("kiro", "id1"),
            format!("{} chat --resume-id id1", bin("kiro"))
        );
        assert_eq!(
            resume_command("kimi", "session_id1"),
            format!("{} -r session_id1", bin("kimi"))
        );
        assert_eq!(
            resume_command("workbuddy", "id1"),
            format!("{} --resume id1", bin("workbuddy"))
        );
    }

    #[test]
    fn kimi_prompts_go_through_the_p_flag() {
        let bin = crate::launcher::resolve("kimi");
        assert_eq!(
            resume_with_prompt("kimi", "session_id1", "继续"),
            format!("{bin} -r session_id1 -p '继续'")
        );
        let prompt = shell_quote("先读 /tmp/b.md 了解上下文，然后继续：收尾");
        assert_eq!(
            handoff_command("kimi", "/tmp/b.md", "收尾"),
            format!("{bin} -p {prompt}")
        );
    }

    #[test]
    fn prompts_are_shell_safe() {
        let cmd = resume_with_prompt("kiro", "id1", "别 `rm -rf /` 也别 'quote'");
        assert!(cmd.contains("'\\''quote'\\''"));
        let bin = crate::launcher::resolve("kiro");
        assert!(cmd.starts_with(&format!("{bin} chat --resume-id id1 '")));
    }
}
