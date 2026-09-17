//! 平台差异集中在这里。
//!
//! 刻意**不用 `#[cfg(windows)]` 包函数体**，而是用运行期的 `cfg!(windows)` 选分支：
//! `#[cfg]` 掉的代码在另一个平台上根本不参与编译，改错了也发现不了；
//! 运行期分支两条路都会被类型检查，且下面的纯函数可以在任意平台上把
//! Windows 行为直接单测出来（分隔符、可执行扩展名、引号规则）。

/// PATH 环境变量的分隔符
pub fn path_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

/// 查找可执行文件时要试的扩展名。Windows 上 `codebuddy` 实际是 `codebuddy.cmd` 之类
pub fn executable_extensions() -> &'static [&'static str] {
    if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat", ".ps1"]
    } else {
        &[""]
    }
}

/// 生成命令时的引号风格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    /// POSIX sh：单引号包裹，内部单引号写成 `'\''`
    Posix,
    /// PowerShell：单引号包裹，内部单引号写成两个单引号
    PowerShell,
}

/// 当前平台的引号风格。Windows 上生成的命令按 PowerShell 约定
/// （cmd.exe 不认单引号，而 Windows 上跑 AI CLI 基本都在 PowerShell 里）
pub fn quote_style() -> QuoteStyle {
    if cfg!(windows) {
        QuoteStyle::PowerShell
    } else {
        QuoteStyle::Posix
    }
}

/// 按指定风格加引号
pub fn quote_with(style: QuoteStyle, input: &str) -> String {
    match style {
        QuoteStyle::Posix => format!("'{}'", input.replace('\'', "'\\''")),
        QuoteStyle::PowerShell => format!("'{}'", input.replace('\'', "''")),
    }
}

/// 拿全部进程的 `pid 命令行` 快照文本。两个平台的取法不同，但都在这里编译。
pub fn process_snapshot_text() -> Option<String> {
    let output = if cfg!(windows) {
        // 输出成 "pid 命令行" 一行一条，与 ps 的形状一致，上层解析逻辑可以复用
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }",
            ])
            .output()
    } else {
        std::process::Command::new("ps")
            .args(["-eo", "pid=,command="])
            .output()
    };
    let output = output.ok()?;
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_and_extensions_match_the_platform() {
        if cfg!(windows) {
            assert_eq!(path_separator(), ';');
            assert!(executable_extensions().contains(&".exe"));
        } else {
            assert_eq!(path_separator(), ':');
            assert_eq!(executable_extensions(), &[""]);
        }
    }

    /// 两种引号规则都在任意平台上验证，避免只在对应平台才发现写错
    #[test]
    fn both_quote_styles_are_correct_everywhere() {
        assert_eq!(
            quote_with(QuoteStyle::Posix, "别 'quote' 我"),
            "'别 '\\''quote'\\'' 我'"
        );
        assert_eq!(
            quote_with(QuoteStyle::PowerShell, "别 'quote' 我"),
            "'别 ''quote'' 我'"
        );
        // 反引号 / $ / 分号都不该被展开，靠整体单引号兜住
        for style in [QuoteStyle::Posix, QuoteStyle::PowerShell] {
            let out = quote_with(style, "rm -rf / `whoami` $HOME; echo");
            assert!(out.starts_with('\'') && out.ends_with('\''));
            assert!(out.contains("`whoami`"));
        }
    }

    #[test]
    fn process_snapshot_returns_something_on_this_platform() {
        let text = process_snapshot_text().expect("拿不到进程快照");
        assert!(
            text.lines().count() > 1,
            "进程快照只有 {} 行",
            text.lines().count()
        );
    }
}
