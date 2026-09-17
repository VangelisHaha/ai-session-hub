//! 生成命令时用的可执行文件定位。
//!
//! 生成的 resume / handoff 命令是要**直接被执行**的（用户不会自己去补 PATH），
//! 但有些工具的 CLI 默认不在 PATH 上：
//! - Kimi Code 装在 `~/.kimi-code/bin/kimi`；
//! - WorkBuddy 的 `codebuddy` 埋在 `WorkBuddy.app` 的 app.asar.unpacked 里。
//!
//! 所以按「PATH 优先 → 已知安装位置兜底 → 都没有就用裸名字」的顺序解析：
//! 在 PATH 上就保持裸名字（可读、可移植），不在就给绝对路径（当场可执行），
//! 完全没装则仍返回裸名字，让用户看到该装什么。

use std::path::{Path, PathBuf};

/// 取该工具生成命令时应使用的可执行文件（必要时是绝对路径，含空格会被引号包起来）
pub fn resolve(tool: &str) -> String {
    let name = binary_name(tool);
    pick(
        name,
        std::env::var(env_key(tool)).ok().as_deref(),
        std::env::var_os("PATH").as_deref().and_then(|p| p.to_str()),
        &fallbacks(tool),
    )
}

/// 各工具的可执行文件名
fn binary_name(tool: &str) -> &'static str {
    match tool {
        "claude" => "claude",
        "codex" => "codex",
        // Kiro IDE 没有命令行 resume，交接时退化成 Kiro CLI
        "kiro" | "kiro-ide" => "kiro-cli",
        "kimi" => "kimi",
        "pi" => "pi",
        "workbuddy" => "codebuddy",
        "gemini" => "gemini",
        "opencode" => "opencode",
        _ => "",
    }
}

/// 覆盖用环境变量名：workbuddy -> ASH_WORKBUDDY_BIN，kiro-ide -> ASH_KIRO_IDE_BIN
fn env_key(tool: &str) -> String {
    format!("ASH_{}_BIN", tool.to_ascii_uppercase().replace('-', "_"))
}

/// 不在 PATH 上时按顺序尝试的已知安装位置
fn fallbacks(tool: &str) -> Vec<PathBuf> {
    let home = crate::paths::home_dir();
    let local_appdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    match tool {
        "kimi" => {
            let mut dirs = vec![home.join(".kimi-code/bin/kimi")];
            // Windows 上是 kimi.cmd / kimi.exe
            dirs.push(home.join(".kimi-code/bin/kimi.cmd"));
            dirs.push(home.join(".kimi-code/bin/kimi.exe"));
            dirs
        }
        "workbuddy" => {
            const IN_APP: &str = "Contents/Resources/app.asar.unpacked/cli/bin/codebuddy";
            const IN_WIN: &str = "resources/app.asar.unpacked/cli/bin/codebuddy.cmd";
            let mut dirs = vec![
                PathBuf::from("/Applications/WorkBuddy.app").join(IN_APP),
                home.join("Applications/WorkBuddy.app").join(IN_APP),
                PathBuf::from("/Applications/CodeBuddy.app").join(IN_APP),
                home.join("Applications/CodeBuddy.app").join(IN_APP),
            ];
            for base in [local_appdata, program_files].into_iter().flatten() {
                for app in ["WorkBuddy", "CodeBuddy"] {
                    dirs.push(base.join(app).join(IN_WIN));
                    dirs.push(base.join("Programs").join(app).join(IN_WIN));
                }
            }
            dirs
        }
        _ => Vec::new(),
    }
}

/// 纯逻辑部分，便于单测：显式覆盖 > PATH 命中 > 已知位置 > 裸名字。
/// `sep` 与 `exts` 由调用方传入，Windows 行为在任意平台上都能测。
fn pick_with(
    name: &str,
    override_path: Option<&str>,
    path_var: Option<&str>,
    fallbacks: &[PathBuf],
    sep: char,
    exts: &[&str],
) -> String {
    if let Some(path) = override_path.map(str::trim).filter(|p| !p.is_empty()) {
        return quote_if_needed(path);
    }
    if name.is_empty() {
        return String::new();
    }
    if let Some(path_var) = path_var {
        for dir in path_var.split(sep).filter(|dir| !dir.is_empty()) {
            for ext in exts {
                if is_executable(&Path::new(dir).join(format!("{name}{ext}"))) {
                    return name.to_string();
                }
            }
        }
    }
    for candidate in fallbacks {
        if is_executable(candidate) {
            return quote_if_needed(&candidate.to_string_lossy());
        }
    }
    name.to_string()
}

fn pick(
    name: &str,
    override_path: Option<&str>,
    path_var: Option<&str>,
    fallbacks: &[PathBuf],
) -> String {
    pick_with(
        name,
        override_path,
        path_var,
        fallbacks,
        crate::platform::path_separator(),
        crate::platform::executable_extensions(),
    )
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    // Windows 没有执行位，靠扩展名判定（调用方已按 PATHEXT 逐个试过）
    if cfg!(windows) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 路径带空格时要包起来，否则拼进命令行会被拆成两个参数。
/// Windows 上装在 `C:\Program Files\…` 是常态，这条比 unix 更要紧。
fn quote_if_needed(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        crate::tools::shell_quote(path)
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_executable(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"#!/bin/sh\n").unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn path_hit_keeps_the_bare_name() {
        let dir = std::env::temp_dir().join(format!("ash-launcher-path-{}", std::process::id()));
        make_executable(&dir, "kimi");
        let out = pick("kimi", None, Some(dir.to_str().unwrap()), &[]);
        assert_eq!(out, "kimi");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_from_path_falls_back_to_the_absolute_install_path() {
        let base = std::env::temp_dir().join(format!("ash-launcher-fb-{}", std::process::id()));
        let installed = make_executable(&base.join("bin"), "codebuddy");
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let out = pick(
            "codebuddy",
            None,
            Some(empty.to_str().unwrap()),
            &[base.join("nope/codebuddy"), installed.clone()],
        );
        assert_eq!(out, installed.to_string_lossy());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn nothing_installed_still_returns_the_name() {
        let missing =
            std::env::temp_dir().join(format!("ash-launcher-none-{}", std::process::id()));
        let out = pick(
            "codebuddy",
            None,
            Some(missing.to_str().unwrap()),
            &[missing.join("codebuddy")],
        );
        assert_eq!(out, "codebuddy");
    }

    #[test]
    fn explicit_override_wins_and_spaces_are_quoted() {
        let dir = std::env::temp_dir().join(format!("ash-launcher-ov-{}", std::process::id()));
        make_executable(&dir, "kimi");
        // 覆盖值优先于 PATH 命中
        let out = pick(
            "kimi",
            Some("/opt/My Tools/kimi"),
            Some(dir.to_str().unwrap()),
            &[],
        );
        assert_eq!(out, "'/opt/My Tools/kimi'");
        // 空字符串视为未设置
        assert_eq!(
            pick("kimi", Some("  "), Some(dir.to_str().unwrap()), &[]),
            "kimi"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_keys_follow_the_tool_id() {
        assert_eq!(env_key("workbuddy"), "ASH_WORKBUDDY_BIN");
        assert_eq!(env_key("kiro-ide"), "ASH_KIRO_IDE_BIN");
    }

    /// Windows 用 `;` 分隔 PATH、靠扩展名找可执行文件。
    /// 这里显式传入 Windows 的分隔符与扩展名，在 macOS 上就能验证那条分支。
    #[test]
    fn windows_path_rules_are_honored() {
        let base = std::env::temp_dir().join(format!("ash-launcher-win-{}", std::process::id()));
        let bin_dir = base.join("Tools");
        // 只有 codebuddy.cmd，没有无扩展名的 codebuddy
        make_executable(&bin_dir, "codebuddy.cmd");
        let other = base.join("Other");
        std::fs::create_dir_all(&other).unwrap();

        let path_var = format!("{};{}", other.display(), bin_dir.display());
        let win_exts = ["", ".exe", ".cmd", ".bat", ".ps1"];

        // 分号分隔 + .cmd 扩展名 → 命中，保持裸名字
        assert_eq!(
            pick_with("codebuddy", None, Some(&path_var), &[], ';', &win_exts),
            "codebuddy"
        );
        // 同样的 PATH 按 unix 规则（冒号 + 无扩展名）解析不出来，会退回裸名字
        assert_eq!(
            pick_with("codebuddy", None, Some(&path_var), &[], ':', &[""]),
            "codebuddy"
        );
        // 但按 unix 规则时 PATH 命中失败，已知位置就该生效
        let installed = bin_dir.join("codebuddy.cmd");
        assert_eq!(
            pick_with(
                "codebuddy",
                None,
                Some(&path_var),
                std::slice::from_ref(&installed),
                ':',
                &[""]
            ),
            installed.to_string_lossy()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// Windows 常见的 `C:\Program Files\...` 带空格，必须整体加引号
    #[test]
    fn windows_style_spaced_paths_are_quoted() {
        let out = pick_with(
            "codebuddy",
            Some(r"C:\Program Files\WorkBuddy\codebuddy.cmd"),
            None,
            &[],
            ';',
            &["", ".cmd"],
        );
        assert!(out.starts_with('\'') && out.ends_with('\''));
        assert!(out.contains("Program Files"));
    }

    #[test]
    fn every_supported_tool_resolves_to_something() {
        for tool in crate::tools::SUPPORTED_TOOLS {
            assert!(!resolve(tool).is_empty(), "{tool} 没有解析出可执行文件");
        }
    }
}
