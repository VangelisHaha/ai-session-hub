//! 目录约定：索引与交接包统一放在 `~/.ai-session-hub` 下，源目录一律只读。

use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
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

/// Kiro IDE（kiro.kiroagent 扩展）的会话目录，macOS 与 Linux 位置不同，两处都试
pub fn kiro_ide_session_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("ASH_KIRO_IDE_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let suffix = "User/globalStorage/kiro.kiroagent/workspace-sessions";
    vec![
        home_dir()
            .join("Library/Application Support/Kiro")
            .join(suffix),
        home_dir().join(".config/Kiro").join(suffix),
    ]
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
