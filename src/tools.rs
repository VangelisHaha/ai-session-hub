//! 各工具的恢复 / 交接命令生成。
//!
//! **同工具**用原生 resume（无损，模型自己带全历史）；
//! **跨工具**无法迁移原生会话（历史格式、tool_use id、模型都不通），
//! 所以统一走「新会话 + 交接包」：把交接文件路径塞进第一条 prompt。

use crate::adapters::{claude, codex, gemini, kiro, opencode};

pub const SUPPORTED_TOOLS: [&str; 5] = ["claude", "codex", "kiro", "gemini", "opencode"];

pub fn resume_command(tool: &str, session_id: &str) -> String {
    match tool {
        "claude" => claude::resume_command(session_id),
        "codex" => codex::resume_command(session_id),
        "kiro" => kiro::resume_command(session_id),
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
        "gemini" => format!("gemini {quoted}"),
        "opencode" => format!("opencode run {quoted}"),
        other => format!("# 未知工具 {other}"),
    }
}

/// 按当前平台的 shell 规则引用 prompt。
///
/// POSIX：单引号包裹，内部单引号按 `'\''` 转义，反引号 / `$` 都不会被展开。
/// Windows：cmd.exe 不认单引号（会把 `'` 当字面量传进去），必须用双引号，
/// 内部双引号转义为 `""`；末尾反斜杠要成对，否则会把结尾的引号转义掉。
pub fn shell_quote(input: &str) -> String {
    if cfg!(windows) {
        quote_windows(input)
    } else {
        quote_posix(input)
    }
}

fn quote_posix(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

/// cmd.exe 风格引用：双引号包裹，内部 `"` → `""`，
/// 紧贴收尾引号的反斜杠序列需要加倍以免转义掉引号本身。
fn quote_windows(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    let mut pending_backslashes = 0usize;
    for ch in input.chars() {
        match ch {
            '\\' => {
                pending_backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 反斜杠在引号前需要加倍，然后把引号写成 ""
                for _ in 0..pending_backslashes {
                    out.push('\\');
                }
                pending_backslashes = 0;
                out.push_str("\"\"");
            }
            other => {
                pending_backslashes = 0;
                out.push(other);
            }
        }
    }
    // 收尾引号前的反斜杠同样要加倍
    for _ in 0..pending_backslashes {
        out.push('\\');
    }
    out.push('"');
    out
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
    }

    #[test]
    fn prompts_are_shell_safe_on_posix() {
        let quoted = quote_posix("别 `rm -rf /` 也别 'quote'");
        assert!(quoted.contains("'\\''quote'\\''"));
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        // 反引号留在单引号里不会被展开
        assert!(quoted.contains("`rm -rf /`"));
    }

    #[test]
    fn prompts_are_shell_safe_on_windows() {
        assert_eq!(quote_windows(r#"说 "你好""#), r#""说 ""你好""""#);
        // 末尾反斜杠必须加倍，否则会转义掉收尾引号
        assert_eq!(quote_windows(r"C:\path\"), r#""C:\path\\""#);
        // 反斜杠 + 引号：反斜杠加倍后引号写成 ""
        assert_eq!(quote_windows(r#"a\"b"#), r#""a\\""b""#);
    }

    #[test]
    fn resume_with_prompt_quotes_for_the_current_platform() {
        let cmd = resume_with_prompt("kiro", "id1", "继续 'x'");
        assert!(cmd.starts_with("kiro-cli chat --resume-id id1 "));
        if cfg!(windows) {
            assert!(cmd.contains('"'));
        } else {
            assert!(cmd.contains("'\\''x'\\''"));
        }
    }
}
