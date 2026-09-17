//! 目录约定：索引与交接包统一放在 `~/.ai-session-hub` 下，源目录一律只读。

use std::path::PathBuf;

/// 用户主目录。Windows 上没有 `HOME`，要退回 `USERPROFILE` / `HOMEDRIVE`+`HOMEPATH`
pub fn home_dir() -> PathBuf {
    home_dir_from(|key| std::env::var_os(key).map(|value| value.to_string_lossy().to_string()))
}

/// 纯逻辑版本，便于单测 Windows 分支（不依赖当前进程的真实环境变量）
fn home_dir_from(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = lookup(key).filter(|value| !value.trim().is_empty()) {
            return PathBuf::from(value);
        }
    }
    // Windows 上偶尔只有 HOMEDRIVE + HOMEPATH（如 C: + \Users\x）
    if let (Some(drive), Some(path)) = (lookup("HOMEDRIVE"), lookup("HOMEPATH")) {
        if !drive.trim().is_empty() && !path.trim().is_empty() {
            return PathBuf::from(format!("{drive}{path}"));
        }
    }
    PathBuf::from("/")
}

/// 数据根目录，可用 AI_SESSION_HUB_HOME 覆盖（测试与多份索引隔离用）
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AI_SESSION_HUB_HOME") {
        return PathBuf::from(dir);
    }
    home_dir().join(".ai-session-hub")
}

pub fn index_db_path() -> PathBuf {
    data_dir().join("index.db")
}

pub fn handoff_dir() -> PathBuf {
    data_dir().join("handoff")
}

/// 各工具会话源目录，支持环境变量覆盖，方便测试与非默认安装位置
pub fn claude_projects_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_CLAUDE_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".claude/projects")
}

pub fn codex_session_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("ASH_CODEX_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let base = home_dir().join(".codex");
    vec![base.join("sessions"), base.join("archived_sessions")]
}

pub fn kiro_sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_KIRO_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".kiro/sessions/cli")
}

/// Kiro IDE（kiro.kiroagent 扩展）的会话目录，各平台位置不同，全部试一遍：
/// macOS `~/Library/Application Support/Kiro`、Linux `~/.config/Kiro`、Windows `%APPDATA%\Kiro`
pub fn kiro_ide_session_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("ASH_KIRO_IDE_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let suffix = "User/globalStorage/kiro.kiroagent/workspace-sessions";
    let mut dirs = vec![
        home_dir()
            .join("Library/Application Support/Kiro")
            .join(suffix),
        home_dir().join(".config/Kiro").join(suffix),
    ];
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("Kiro").join(suffix));
    }
    dirs
}

/// Kimi Code CLI（`kimi`）的会话根目录：
/// `~/.kimi-code/sessions/wd_<名字>_<hash>/session_<uuid>/agents/<agent>/wire.jsonl`
pub fn kimi_sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_KIMI_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".kimi-code/sessions")
}

/// pi（`pi` CLI agent）的会话目录：`~/.pi/agent/sessions/<编码后的 cwd>/<时间>_<uuid>.jsonl`
pub fn pi_sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_PI_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".pi/agent/sessions")
}

/// WorkBuddy（腾讯 CodeBuddy 换皮）的会话目录。
///
/// 桌面应用写 `~/.workbuddy/projects`，独立 CLI（`codebuddy` / `cbc`）写 `~/.codebuddy/projects`，
/// 两处格式完全一致，都要扫。
pub fn workbuddy_project_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("ASH_WORKBUDDY_DIR") {
        return vec![PathBuf::from(dir)];
    }
    vec![
        home_dir().join(".workbuddy/projects"),
        home_dir().join(".codebuddy/projects"),
    ]
}

/// WorkBuddy 的活跃会话心跳目录：`<root>/sessions/<pid>.json`，内含 pid 与 sessionId
pub fn workbuddy_session_dirs() -> Vec<PathBuf> {
    workbuddy_project_dirs()
        .into_iter()
        .filter_map(|dir| dir.parent().map(|base| base.join("sessions")))
        .collect()
}

pub fn gemini_tmp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_GEMINI_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".gemini/tmp")
}

pub fn opencode_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ASH_OPENCODE_DB") {
        return PathBuf::from(path);
    }
    home_dir().join(".local/share/opencode/opencode.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn home_prefers_home_then_userprofile() {
        assert_eq!(
            home_dir_from(env_of(&[
                ("HOME", "/Users/x"),
                ("USERPROFILE", r"C:\Users\x")
            ])),
            PathBuf::from("/Users/x")
        );
        // Windows 上通常没有 HOME
        assert_eq!(
            home_dir_from(env_of(&[("USERPROFILE", r"C:\Users\x")])),
            PathBuf::from(r"C:\Users\x")
        );
    }

    #[test]
    fn home_falls_back_to_homedrive_plus_homepath() {
        assert_eq!(
            home_dir_from(env_of(&[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\x")])),
            PathBuf::from(r"C:\Users\x")
        );
    }

    #[test]
    fn blank_values_are_ignored() {
        assert_eq!(
            home_dir_from(env_of(&[("HOME", "   "), ("USERPROFILE", r"D:\me")])),
            PathBuf::from(r"D:\me")
        );
        assert_eq!(home_dir_from(env_of(&[])), PathBuf::from("/"));
    }
}
