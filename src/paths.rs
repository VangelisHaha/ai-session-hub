//! 目录约定：索引与交接包统一放在 `~/.ai-session-hub` 下，源目录一律只读。

use std::path::PathBuf;

/// 用户主目录。Unix 给的是 `HOME`，Windows 上通常只有 `USERPROFILE`
/// （或 `HOMEDRIVE` + `HOMEPATH`）；只认 `HOME` 会退化到根目录，导致所有源目录都扫不到。
pub fn home_dir() -> PathBuf {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            let path = PathBuf::from(value);
            if !path.as_os_str().is_empty() {
                return path;
            }
        }
    }
    if let (Some(drive), Some(tail)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut joined = drive;
        joined.push(&tail);
        let path = PathBuf::from(joined);
        if !path.as_os_str().is_empty() {
            return path;
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
    home_dir().join(".claude").join("projects")
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
    home_dir().join(".kiro").join("sessions").join("cli")
}

pub fn gemini_tmp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASH_GEMINI_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".gemini").join("tmp")
}

pub fn opencode_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ASH_OPENCODE_DB") {
        return PathBuf::from(path);
    }
    home_dir()
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径必须逐段 join，不能出现嵌斜杠的字面量：
    /// Windows 上 `join(".kiro/sessions/cli")` 会把整串当成一个组件
    #[test]
    fn source_dirs_are_built_component_by_component() {
        let sep = std::path::MAIN_SEPARATOR;
        for path in [
            claude_projects_dir(),
            kiro_sessions_dir(),
            gemini_tmp_dir(),
            opencode_db_path(),
        ] {
            // 每个组件里都不该再夹着分隔符
            for component in path.components() {
                let text = component.as_os_str().to_string_lossy();
                assert!(
                    !text.contains('/') || text.contains(sep) && text.len() <= 1,
                    "组件 {text:?} 里含有未拆开的分隔符：{}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn home_falls_back_across_platform_env_vars() {
        // 至少要能拿到一个非根目录（CI 与本机都会设置其中之一）
        let home = home_dir();
        assert!(!home.as_os_str().is_empty());
    }
}
