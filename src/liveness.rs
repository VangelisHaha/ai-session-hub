//! 会话活跃度与进度判定。
//!
//! ## 信号来源（按可靠性排序）
//! 1. **Kiro 的 `.lock` 文件**：内含 `{pid, started_at}`，进程存活即会话仍被持有 —— 精确。
//! 2. **进程命令行**：`kiro-cli chat --resume-id <id>` / `codex resume <id>` 里能直接读到会话 ID。
//! 3. **对话形态 + 时间**：Codex / Claude 写完日志就关闭 fd（`lsof` 抓不到持有者），
//!    只能靠"最后一条消息是什么、离现在多久"推断。
//!
//! ## 为什么状态不入索引
//! 状态是易失的（几十秒就会变），落库必然读到陈旧值。这里全部实时计算，
//! 成本只有一次 `ps` 快照 + 读锁目录。

use crate::model::SessionRow;
use serde::Serialize;
use std::collections::HashMap;

/// 会话状态。命名对齐用户会怎么问（"进行中 / 待确认 / 已完成"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// AI 正在跑：刚有新消息，或用户提问 / 工具调用还悬着
    Running,
    /// 在等我回复：AI 最后一句是提问或请求确认
    AwaitingInput,
    /// 在等我批准工具执行：最后是工具调用且没有结果，进程还活着
    AwaitingApproval,
    /// 被我打断了
    Interrupted,
    /// 会话进程还开着，但 AI 已交付、没在等我 —— 可以直接接着聊
    Idle,
    /// 会话已经结束（进程不在了），且没有遗留待办
    Done,
    /// 会话已经结束，但最后留有未完成事项
    Unfinished,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingInput => "awaiting_input",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Interrupted => "interrupted",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Unfinished => "unfinished",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "进行中",
            Self::AwaitingInput => "待我回复",
            Self::AwaitingApproval => "待我确认工具执行",
            Self::Interrupted => "已被打断",
            Self::Idle => "空闲可续聊",
            Self::Done => "已完成",
            Self::Unfinished => "已结束但有遗留",
        }
    }

    /// 会不会打扰到别人干活：running / awaiting_approval 时最好别插手
    pub fn busy(self) -> bool {
        matches!(self, Self::Running | Self::AwaitingApproval)
    }

    pub fn parse(input: &str) -> Option<Self> {
        let normalized = input.trim().to_ascii_lowercase();
        let state = match normalized.as_str() {
            "running" | "进行中" | "运行中" | "工作中" => Self::Running,
            "awaiting_input" | "待回复" | "待我回复" | "等我回复" => Self::AwaitingInput,
            "awaiting_approval" | "待确认" | "待审批" | "待我确认" => {
                Self::AwaitingApproval
            }
            "interrupted" | "已打断" | "被打断" | "中断" => Self::Interrupted,
            "idle" | "空闲" => Self::Idle,
            "done" | "已完成" | "完成" => Self::Done,
            "unfinished" | "未完成" | "有遗留" => Self::Unfinished,
            _ => return None,
        };
        Some(state)
    }
}

/// 判定所依据的证据，回答"凭什么说它在跑"
#[derive(Debug, Clone, Serialize)]
pub struct StateEvidence {
    /// 持有会话的进程 ID（Kiro 来自 .lock，其余来自命令行匹配）
    pub holder_pid: Option<i32>,
    /// 进程是否仍存活
    pub process_alive: bool,
    /// 距最后一次活动的秒数（取消息时间与会话文件 mtime 的较新者）
    pub idle_seconds: Option<i64>,
    /// idle 的计算基准：message_ts 或 file_mtime
    pub activity_source: &'static str,
    /// 最后一条消息的角色 / 类型
    pub last_role: Option<String>,
    pub last_kind: Option<String>,
    /// 判定说明
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub uid: String,
    pub tool: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub state: &'static str,
    pub label: &'static str,
    pub busy: bool,
    pub evidence: StateEvidence,
}

/// 最后一条消息的形态（索引里缓存，避免逐会话再查消息表）
#[derive(Debug, Clone, Default)]
pub struct LastMessage {
    pub role: Option<String>,
    pub kind: Option<String>,
    pub tool_name: Option<String>,
    pub head: Option<String>,
    pub ts: Option<i64>,
    /// 该会话是否留有未完成信号（来自摘要的 open_items）
    pub has_open_items: bool,
}

/// 一次进程快照 + 锁目录快照，供批量判定复用
pub struct LivenessProbe {
    /// pid -> 命令行
    processes: HashMap<i32, String>,
    /// Kiro session_id -> pid
    kiro_locks: HashMap<String, i32>,
    /// WorkBuddy session_id -> pid（来自 ~/.workbuddy/sessions/<pid>.json 心跳文件）
    workbuddy_sessions: HashMap<String, i32>,
    /// 命令行里显式带出的会话 ID -> pid
    resumed_sessions: HashMap<String, i32>,
    now_ms: i64,
}

/// 中断标记：Kiro / Claude 打断时会把这句写进会话，pi 则写 stopReason=aborted + errorMessage
const INTERRUPT_MARKS: [&str; 5] = [
    "Response was interrupted",
    "[Request interrupted",
    "Operation aborted",
    "用户打断",
    "已被用户中断",
];

/// AI 在等我回话的句式特征
const QUESTION_MARKS: [&str; 12] = [
    "？",
    "?",
    "请确认",
    "是否继续",
    "要不要",
    "需要我",
    "确认一下",
    "你看行吗",
    "怎么处理",
    "请选择",
    "等你确认",
    "告诉我",
];

/// 最近多久算"刚刚还在动"
const RUNNING_WINDOW_SECONDS: i64 = 90;
/// 悬空的提问 / 工具调用在多久内算"还在跑"
const PENDING_WINDOW_SECONDS: i64 = 600;

impl LivenessProbe {
    pub fn capture() -> Self {
        Self {
            processes: snapshot_processes(),
            kiro_locks: read_kiro_locks(),
            workbuddy_sessions: read_workbuddy_sessions(),
            resumed_sessions: HashMap::new(),
            now_ms: chrono::Utc::now().timestamp_millis(),
        }
        .with_resumed_sessions()
    }

    /// 从命令行里抠出 `--resume-id <id>` / `resume <id>` / `--resume <id>`
    fn with_resumed_sessions(mut self) -> Self {
        for (pid, command) in &self.processes {
            for id in extract_session_ids(command) {
                self.resumed_sessions.insert(id, *pid);
            }
        }
        self
    }

    fn holder_pid(&self, session: &SessionRow) -> Option<i32> {
        if session.tool == "kiro" {
            if let Some(pid) = self.kiro_locks.get(&session.session_id) {
                return Some(*pid);
            }
        }
        if session.tool == "workbuddy" {
            if let Some(pid) = self.workbuddy_sessions.get(&session.session_id) {
                return Some(*pid);
            }
        }
        self.resumed_sessions.get(&session.session_id).copied()
    }

    pub fn evaluate(&self, session: &SessionRow, last: &LastMessage) -> SessionStatus {
        let holder_pid = self.holder_pid(session);
        let process_alive = holder_pid
            .map(|pid| self.processes.contains_key(&pid))
            .unwrap_or(false);
        // Kiro 的助手消息与工具结果都没有时间戳，只能靠会话文件 mtime 当心跳；
        // 光看消息时间会把正在连续跑工具的会话误判成"卡住了"。
        let message_ts = last.ts.or(session.updated_at);
        let file_ts = session
            .source_path
            .as_deref()
            .filter(|path| !path.starts_with("sqlite:"))
            .and_then(file_mtime_ms);
        let (activity_ts, activity_source) = match (message_ts, file_ts) {
            (Some(msg), Some(file)) if file > msg => (Some(file), "file_mtime"),
            (Some(msg), _) => (Some(msg), "message_ts"),
            (None, file) => (file, "file_mtime"),
        };
        let idle_seconds = activity_ts.map(|ts| (self.now_ms - ts) / 1000);

        let (state, reason) = classify(last, idle_seconds, process_alive);
        SessionStatus {
            uid: session.uid.clone(),
            tool: session.tool.clone(),
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            state: state.as_str(),
            label: state.label(),
            busy: state.busy(),
            evidence: StateEvidence {
                holder_pid,
                process_alive,
                idle_seconds,
                activity_source,
                last_role: last.role.clone(),
                last_kind: last.kind.clone(),
                reason,
            },
        }
    }
}

/// 状态判定的纯逻辑，便于单测覆盖各分支
pub fn classify(
    last: &LastMessage,
    idle_seconds: Option<i64>,
    process_alive: bool,
) -> (SessionState, String) {
    let head = last.head.clone().unwrap_or_default();
    let role = last.role.clone().unwrap_or_default();
    let kind = last.kind.clone().unwrap_or_default();
    let idle = idle_seconds.unwrap_or(i64::MAX);

    if INTERRUPT_MARKS.iter().any(|mark| head.contains(mark)) {
        return (
            SessionState::Interrupted,
            "最后一条消息带有中断标记".to_string(),
        );
    }

    // 工具调用没有结果 —— 刚发起就是在执行，挂久了才是卡在审批上
    if kind == crate::model::KIND_TOOL_USE {
        let tool = last.tool_name.clone().unwrap_or_default();
        if idle <= RUNNING_WINDOW_SECONDS {
            return (
                SessionState::Running,
                format!("{idle} 秒前刚发起工具 {tool}，正在执行"),
            );
        }
        if process_alive {
            return (
                SessionState::AwaitingApproval,
                format!("工具 {tool} 发起后 {idle} 秒没有结果，进程仍在 —— 大概卡在你的确认上"),
            );
        }
        if idle <= PENDING_WINDOW_SECONDS {
            return (
                SessionState::Running,
                "最后是工具调用且尚无结果，时间很近，判为仍在执行".to_string(),
            );
        }
        return (
            SessionState::Unfinished,
            "最后是工具调用且没有结果，进程已退出，动作没走完".to_string(),
        );
    }

    // 工具结果是最后一条 —— AI 还没就这个结果收尾
    if kind == crate::model::KIND_TOOL_RESULT {
        if idle <= PENDING_WINDOW_SECONDS {
            return (
                SessionState::Running,
                "刚拿到工具结果，AI 正在继续处理".to_string(),
            );
        }
        return (
            SessionState::Unfinished,
            "最后是工具结果，AI 没有收尾就停了".to_string(),
        );
    }

    // 用户说完话但 AI 还没回 —— 正在生成
    if role == crate::model::ROLE_USER {
        if idle <= PENDING_WINDOW_SECONDS {
            return (
                SessionState::Running,
                "最后一条是你的提问，AI 还没回复，判为正在生成".to_string(),
            );
        }
        return (
            SessionState::Unfinished,
            "最后一条是你的提问但没有回复，会话可能中途断掉了".to_string(),
        );
    }

    if idle <= RUNNING_WINDOW_SECONDS {
        return (
            SessionState::Running,
            format!("{idle} 秒前刚有新消息，正在进行"),
        );
    }

    let asking = QUESTION_MARKS.iter().any(|mark| head.contains(mark));
    if asking {
        // 进程都没了就谈不上"在等我回复"，但这类会话确实是没聊完，标成遗留更有用
        return if process_alive {
            (
                SessionState::AwaitingInput,
                "AI 最后一句在提问或请你确认，会话仍开着".to_string(),
            )
        } else {
            (
                SessionState::Unfinished,
                "AI 最后在等你回话，但会话已经关闭 —— 你漏回了".to_string(),
            )
        };
    }

    if process_alive {
        return (
            SessionState::Idle,
            "会话进程还开着，AI 已交付且没在等你 —— 可以直接接着聊".to_string(),
        );
    }

    if last.has_open_items {
        return (
            SessionState::Unfinished,
            "会话已结束，但摘要里还有未完成事项".to_string(),
        );
    }

    (
        SessionState::Done,
        "会话已结束，最后是 AI 的交付且无遗留".to_string(),
    )
}

fn file_mtime_ms(path: &str) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

/// 一次性进程快照，避免逐个会话 fork 进程。
/// unix 走 `ps`，Windows 走 PowerShell 的 Win32_Process，两边都输出 "pid 命令行"，共用下面的解析。
fn snapshot_processes() -> HashMap<i32, String> {
    let Some(text) = crate::platform::process_snapshot_text() else {
        return HashMap::new();
    };
    parse_process_snapshot(&text)
}

fn parse_process_snapshot(text: &str) -> HashMap<i32, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some((pid, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if let Ok(pid) = pid.parse::<i32>() {
            map.insert(pid, command.trim().to_string());
        }
    }
    map
}

/// 读 Kiro 的会话锁：`~/.kiro/sessions/cli/<session>.lock` → `{"pid":123,...}`
fn read_kiro_locks() -> HashMap<String, i32> {
    let dir = crate::paths::kiro_sessions_dir();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("lock") {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(pid) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("pid").and_then(serde_json::Value::as_i64))
        {
            map.insert(session_id.to_string(), pid as i32);
        }
    }
    map
}

/// 读 WorkBuddy 的会话心跳：`~/.workbuddy/sessions/<pid>.json` → `{"pid":123,"sessionId":"..."}`。
/// 比命令行匹配可靠：WorkBuddy 是 Electron 应用，会话 ID 不出现在命令行里。
/// prewarm / interactive-<pid> 这类非真实会话的 sessionId 不会命中索引，无需额外过滤。
fn read_workbuddy_sessions() -> HashMap<String, i32> {
    let mut map = HashMap::new();
    for dir in crate::paths::workbuddy_session_dirs() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let session_id = value.get("sessionId").and_then(serde_json::Value::as_str);
            let pid = value.get("pid").and_then(serde_json::Value::as_i64);
            if let (Some(session_id), Some(pid)) = (session_id, pid) {
                map.insert(session_id.to_string(), pid as i32);
            }
        }
    }
    map
}

/// 从命令行提取会话 ID：`--resume-id X` / `--resume X` / `resume X` / `-s X` / `-r X`（kimi）
pub fn extract_session_ids(command: &str) -> Vec<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut ids = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let is_flag = matches!(
            *token,
            "--resume-id" | "--resume" | "resume" | "-s" | "--session" | "-r" | "-S"
        );
        if !is_flag {
            continue;
        }
        if let Some(candidate) = tokens.get(index + 1) {
            let cleaned = candidate.trim_matches(|c| c == '"' || c == '\'');
            if cleaned.len() >= 8 && !cleaned.starts_with('-') {
                ids.push(cleaned.to_string());
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(role: &str, kind: &str, head: &str, has_open: bool) -> LastMessage {
        LastMessage {
            role: Some(role.to_string()),
            kind: Some(kind.to_string()),
            tool_name: Some("shell".to_string()),
            head: Some(head.to_string()),
            ts: Some(0),
            has_open_items: has_open,
        }
    }

    #[test]
    fn fresh_assistant_message_means_running() {
        let (state, _) = classify(
            &last("assistant", "text", "已经改完了", false),
            Some(10),
            true,
        );
        assert_eq!(state, SessionState::Running);
        assert!(state.busy());
    }

    #[test]
    fn fresh_tool_use_is_running_but_a_stale_one_awaits_approval() {
        let (fresh, _) = classify(&last("assistant", "tool_use", "{}", false), Some(20), true);
        assert_eq!(fresh, SessionState::Running);
        let (stale, reason) =
            classify(&last("assistant", "tool_use", "{}", false), Some(300), true);
        assert_eq!(stale, SessionState::AwaitingApproval);
        assert!(reason.contains("shell"));
    }

    #[test]
    fn dangling_tool_use_without_process_is_unfinished() {
        let (state, _) = classify(
            &last("assistant", "tool_use", "{}", false),
            Some(9000),
            false,
        );
        assert_eq!(state, SessionState::Unfinished);
    }

    #[test]
    fn unanswered_user_message_is_running_then_unfinished() {
        let (fresh, _) = classify(&last("user", "text", "继续", false), Some(60), false);
        assert_eq!(fresh, SessionState::Running);
        let (stale, _) = classify(&last("user", "text", "继续", false), Some(9000), false);
        assert_eq!(stale, SessionState::Unfinished);
    }

    #[test]
    fn question_awaits_reply_only_while_the_session_is_open() {
        let (open, _) = classify(
            &last("assistant", "text", "两个方案你选哪个？", false),
            Some(3600),
            true,
        );
        assert_eq!(open, SessionState::AwaitingInput);
        assert!(!open.busy());
        // 进程已退出：算"你漏回了"的遗留，而不是还在等你
        let (closed, reason) = classify(
            &last("assistant", "text", "两个方案你选哪个？", false),
            Some(3600),
            false,
        );
        assert_eq!(closed, SessionState::Unfinished);
        assert!(reason.contains("漏回"));
    }

    #[test]
    fn interrupt_mark_wins() {
        let (state, _) = classify(
            &last(
                "assistant",
                "text",
                "Response was interrupted by the user",
                false,
            ),
            Some(30),
            true,
        );
        assert_eq!(state, SessionState::Interrupted);
    }

    #[test]
    fn live_process_without_question_is_idle_and_closed_is_done() {
        let (idle, _) = classify(
            &last("assistant", "text", "改完了", false),
            Some(3600),
            true,
        );
        assert_eq!(idle, SessionState::Idle);
        let (done, _) = classify(
            &last("assistant", "text", "改完了", false),
            Some(3600),
            false,
        );
        assert_eq!(done, SessionState::Done);
        let (unfinished, _) = classify(
            &last("assistant", "text", "改完了", true),
            Some(3600),
            false,
        );
        assert_eq!(unfinished, SessionState::Unfinished);
    }

    /// unix 的 `ps` 与 Windows 的 Win32_Process 都被规整成 "pid 命令行"，共用一个解析器。
    /// 这里用 Windows 形状的输出（反斜杠路径、带空格的目录、引号）验证那一侧也能解析。
    #[test]
    fn windows_shaped_snapshot_is_parsed_too() {
        let unix = "  501 /usr/bin/pi --session 01a0aece-8e56\n 1234 ps -eo pid=,command=\n";
        let parsed = parse_process_snapshot(unix);
        assert_eq!(
            parsed.get(&501).map(String::as_str),
            Some("/usr/bin/pi --session 01a0aece-8e56")
        );

        let windows = concat!(
            "4512 \"C:\\Program Files\\WorkBuddy\\codebuddy.cmd\" --resume f5068740-b383\r\n",
            "9001 powershell -NoProfile -Command Get-CimInstance\r\n",
            "\r\n",
            "not-a-pid something\r\n",
        );
        let parsed = parse_process_snapshot(windows);
        assert_eq!(parsed.len(), 2, "解析结果: {parsed:?}");
        let cmd = parsed.get(&4512).expect("缺少 4512");
        assert!(cmd.contains("Program Files"));
        assert!(cmd.ends_with("f5068740-b383"), "结尾没去掉 \\r：{cmd:?}");
        // 命令行里的 --resume <id> 仍能被抠出来
        assert_eq!(extract_session_ids(cmd), vec!["f5068740-b383".to_string()]);
    }

    #[test]
    fn session_ids_are_extracted_from_command_lines() {
        assert_eq!(
            extract_session_ids("kiro-cli chat --resume-id b2daab07-0161-4897 --trust-all-tools"),
            vec!["b2daab07-0161-4897".to_string()]
        );
        assert_eq!(
            extract_session_ids("codex resume 019f88fb-f715-7700"),
            vec!["019f88fb-f715-7700".to_string()]
        );
        assert!(extract_session_ids("codex exec --skip-git-repo-check hello").is_empty());
    }

    #[test]
    fn natural_language_states_are_recognized() {
        assert_eq!(SessionState::parse("进行中"), Some(SessionState::Running));
        assert_eq!(
            SessionState::parse("待确认"),
            Some(SessionState::AwaitingApproval)
        );
        assert_eq!(SessionState::parse("已完成"), Some(SessionState::Done));
        assert_eq!(SessionState::parse("DONE"), Some(SessionState::Done));
        assert_eq!(SessionState::parse("胡说"), None);
    }
}
