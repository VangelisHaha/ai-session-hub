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
    match tool {
        "kimi" => vec![home.join(".kimi-code/bin/kimi")],
        "workbuddy" => {
            const IN_APP: &str = "Contents/Resources/app.asar.unpacked/cli/bin/codebuddy";
            vec![
                PathBuf::from("/Applications/WorkBuddy.app").join(IN_APP),
                home.join("Applications/WorkBuddy.app").join(IN_APP),
                PathBuf::from("/Applications/CodeBuddy.app").join(IN_APP),
                home.join("Applications/CodeBuddy.app").join(IN_APP),
            ]
        }
        _ => Vec::new(),
    }
}

/// 纯逻辑部分，便于单测：显式覆盖 > PATH 命中 > 已知位置 > 裸名字
fn pick(
    name: &str,
    override_path: Option<&str>,
    path_var: Option<&str>,
    fallbacks: &[PathBuf],
) -> String {
    if let Some(path) = override_path.map(str::trim).filter(|p| !p.is_empty()) {
        return quote_if_needed(path);
    }
    if name.is_empty() {
        return String::new();
    }
    if let Some(path_var) = path_var {
        for dir in path_var.split(':').filter(|dir| !dir.is_empty()) {
            if is_executable(&Path::new(dir).join(name)) {
                return name.to_string();
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

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
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

/// 路径带空格时要包起来，否则拼进命令行会被拆成两个参数
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

    #[test]
    fn every_supported_tool_resolves_to_something() {
        for tool in crate::tools::SUPPORTED_TOOLS {
            assert!(!resolve(tool).is_empty(), "{tool} 没有解析出可执行文件");
        }
    }
}
