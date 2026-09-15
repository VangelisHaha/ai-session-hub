//! 业务操作层：CLI 与 MCP 共用同一套实现，避免两处逻辑漂移。
//!
//! 入参结构体直接用 serde 反序列化 MCP 的 arguments，CLI 侧手工构造同样的结构体。

use crate::digest;
use crate::handoff;
use crate::index::{Index, ListOptions, ReadOptions, SearchOptions};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};

/// 距上次同步超过这个间隔就先增量刷一遍（查询前自动保鲜）
const AUTO_SYNC_INTERVAL_MS: i64 = 60_000;

/// 需要实时判定状态时，最多扫多少个候选会话（状态无法在 SQL 里过滤）
const STATE_SCAN_LIMIT: usize = 200;

fn default_limit() -> usize {
    20
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct SearchArgs {
    pub query: String,
    pub tool: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub role: Option<String>,
    pub include_tool_results: bool,
    pub limit: Option<usize>,
    pub max_per_session: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ListArgs {
    pub tool: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<String>,
    pub title: Option<String>,
    pub limit: Option<usize>,
    /// 状态过滤：running / awaiting_input / awaiting_approval / interrupted / idle / done / unfinished
    /// 也接受中文（进行中 / 待我回复 / 待确认 / 已完成 …）
    pub state: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ReadArgs {
    pub uid: String,
    pub tail: Option<usize>,
    pub from_seq: Option<i64>,
    pub include_tool_results: bool,
    pub only_text: bool,
    pub budget_chars: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct UidArgs {
    pub uid: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct HandoffArgs {
    pub uid: String,
    pub to_tool: String,
    pub note: Option<String>,
    pub tail: Option<usize>,
    pub include_tool_results: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ResumeArgs {
    pub uid: String,
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct SyncArgs {
    pub full: bool,
    pub tool: Option<String>,
}

/// 查询前保证索引不过期
pub fn ensure_fresh(index: &mut Index) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    if now - index.last_sync_ms() > AUTO_SYNC_INTERVAL_MS {
        index.sync(false, None)?;
    }
    Ok(())
}

pub fn search(index: &mut Index, args: &SearchArgs) -> Result<Value> {
    if args.query.trim().is_empty() {
        return Err(anyhow!("query 不能为空"));
    }
    ensure_fresh(index)?;
    let options = SearchOptions {
        query: args.query.clone(),
        tool: normalize_tool(args.tool.as_deref())?,
        cwd: args.cwd.clone(),
        since: parse_time(args.since.as_deref())?,
        until: parse_time(args.until.as_deref())?,
        role: args.role.clone(),
        include_tool_results: args.include_tool_results,
        limit: args.limit.unwrap_or_else(default_limit),
        max_per_session: args.max_per_session.unwrap_or(3),
    };
    let hits = index.search(&options)?;
    let terms: Vec<String> = args.query.split_whitespace().map(str::to_string).collect();
    Ok(json!({
        "query": args.query,
        "count": hits.len(),
        "hits": hits.iter().map(|hit| json!({
            "uid": hit.uid,
            "tool": hit.tool,
            "title": hit.title,
            "cwd": hit.cwd,
            "role": hit.role,
            "kind": hit.kind,
            "seq": hit.seq,
            "time": fmt_ts(hit.ts),
            "snippet": focus_snippet(&hit.snippet, &terms),
        })).collect::<Vec<_>>(),
        "hint": "用 session_read 读全文；用 session_handoff 交接到其他工具",
    }))
}

pub fn list(index: &mut Index, args: &ListArgs) -> Result<Value> {
    ensure_fresh(index)?;
    let limit = args.limit.unwrap_or_else(default_limit);
    let wanted = match args.state.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(
            crate::liveness::SessionState::parse(raw)
                .ok_or_else(|| anyhow!("无法识别的状态 {raw}（可用：进行中/待我回复/待确认/已被打断/空闲/已完成/未完成）"))?,
        ),
        _ => None,
    };
    let options = ListOptions {
        tool: normalize_tool(args.tool.as_deref())?,
        cwd: args.cwd.clone(),
        since: parse_time(args.since.as_deref())?,
        title_like: args.title.clone(),
        // 状态是实时算的，SQL 里没法过滤，只能先多捞候选再筛
        limit: if wanted.is_some() {
            (limit * 10).clamp(50, STATE_SCAN_LIMIT)
        } else {
            limit
        },
    };
    let sessions = index.list_sessions(&options)?;

    let probe = crate::liveness::LivenessProbe::capture();
    let mut rows = Vec::new();
    for session in &sessions {
        let last = index.last_message(&session.uid)?;
        let status = probe.evaluate(session, &last);
        if let Some(wanted) = wanted {
            if status.state != wanted.as_str() {
                continue;
            }
        }
        rows.push(json!({
            "uid": session.uid,
            "tool": session.tool,
            "title": session.title,
            "cwd": session.cwd,
            "created": fmt_ts(session.created_at),
            "updated": fmt_ts(session.updated_at),
            "messages": session.message_count,
            "state": status.state,
            "state_label": status.label,
            "busy": status.busy,
            "resume": session.resume_command,
        }));
        if rows.len() >= limit {
            break;
        }
    }
    Ok(json!({
        "count": rows.len(),
        "scanned": sessions.len(),
        "state_filter": wanted.map(|state| state.as_str()),
        "sessions": rows,
    }))
}

/// 查询单个会话的状态与判定依据
pub fn status(index: &mut Index, args: &UidArgs) -> Result<Value> {
    ensure_fresh(index)?;
    let uid = resolve(index, &args.uid)?;
    let session = index
        .get_session(&uid)?
        .ok_or_else(|| anyhow!("未找到会话 {uid}"))?;
    let last = index.last_message(&uid)?;
    let probe = crate::liveness::LivenessProbe::capture();
    let status = probe.evaluate(&session, &last);
    let mut value = serde_json::to_value(&status)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("updated".to_string(), json!(fmt_ts(session.updated_at)));
        object.insert("messages".to_string(), json!(session.message_count));
        object.insert("resume".to_string(), json!(session.resume_command));
        object.insert(
            "advice".to_string(),
            json!(advice_for(status.state, &session.tool)),
        );
    }
    Ok(value)
}

/// 当前"还开着"的会话总览：谁在忙、谁在等我回话
pub fn active(index: &mut Index, args: &ListArgs) -> Result<Value> {
    ensure_fresh(index)?;
    let options = ListOptions {
        tool: normalize_tool(args.tool.as_deref())?,
        cwd: args.cwd.clone(),
        // 只看最近两天，更早的会话不可能还挂着
        since: parse_time(args.since.as_deref().or(Some("2d")))?,
        title_like: None,
        limit: STATE_SCAN_LIMIT,
    };
    let sessions = index.list_sessions(&options)?;
    let probe = crate::liveness::LivenessProbe::capture();

    let mut busy = Vec::new();
    let mut waiting = Vec::new();
    let mut open = Vec::new();
    for session in &sessions {
        let last = index.last_message(&session.uid)?;
        let status = probe.evaluate(session, &last);
        let entry = json!({
            "uid": session.uid,
            "tool": session.tool,
            "title": session.title,
            "cwd": session.cwd,
            "updated": fmt_ts(session.updated_at),
            "state": status.state,
            "state_label": status.label,
            "idle_seconds": status.evidence.idle_seconds,
            "pid": status.evidence.holder_pid,
            "reason": status.evidence.reason,
            "resume": session.resume_command,
        });
        if status.busy {
            busy.push(entry);
        } else if status.state == "awaiting_input" || status.state == "interrupted" {
            waiting.push(entry);
        } else if status.evidence.process_alive {
            open.push(entry);
        }
    }
    Ok(json!({
        "busy": busy,
        "waiting_for_me": waiting,
        "open_but_idle": open,
        "hint": "busy 里的会话正在跑或在等批准，别去打扰；waiting_for_me 是在等你回话的",
    }))
}

/// 针对状态给出下一步建议，让调用方不用自己猜
fn advice_for(state: &str, tool: &str) -> String {
    match state {
        "running" => format!("{tool} 正在干活，先别插手；要看进展用 session_read --tail"),
        "awaiting_approval" => {
            format!("{tool} 卡在工具执行/审批上，去那个终端确认一下")
        }
        "awaiting_input" => "AI 在等你回话，可以用 session_resume_cmd 拿到续聊命令".to_string(),
        "interrupted" => "上次被你打断了，可以续聊并说明要从哪继续".to_string(),
        "idle" => "会话还开着，直接接着聊即可".to_string(),
        "unfinished" => "会话结束了但有遗留，建议先看 session_digest 的未完成事项".to_string(),
        _ => "会话已收尾，无需处理".to_string(),
    }
}

/// 读会话正文，返回 (Markdown 文本, 结构化元信息)
pub fn read(index: &mut Index, args: &ReadArgs) -> Result<(String, Value)> {
    ensure_fresh(index)?;
    let uid = resolve(index, &args.uid)?;
    let session = index
        .get_session(&uid)?
        .ok_or_else(|| anyhow!("未找到会话 {uid}"))?;
    let messages = index.read_messages(
        &uid,
        &ReadOptions {
            tail: args.tail,
            from_seq: args.from_seq,
            include_tool_results: args.include_tool_results,
            only_text: args.only_text,
            budget_chars: Some(args.budget_chars.unwrap_or(12_000)),
        },
    )?;

    let mut out = String::new();
    out.push_str(&format!(
        "# {} ({})\n\n- 会话：`{}`\n- 目录：{}\n- 时间：{} → {}\n- 全量消息数：{}（本次返回 {}）\n- 原生续聊：`{}`\n",
        session.title.clone().unwrap_or_else(|| "未命名会话".to_string()),
        session.tool,
        session.uid,
        session.cwd.clone().unwrap_or_else(|| "未知".to_string()),
        fmt_ts(session.created_at).unwrap_or_else(|| "未知".to_string()),
        fmt_ts(session.updated_at).unwrap_or_else(|| "未知".to_string()),
        session.message_count,
        messages.len(),
        session.resume_command.clone().unwrap_or_default(),
    ));
    for message in &messages {
        let who = match (message.role.as_str(), message.kind.as_str()) {
            ("user", _) => "用户".to_string(),
            ("assistant", "tool_use") => format!(
                "助手·工具 {}",
                message.tool_name.clone().unwrap_or_default()
            ),
            ("assistant", _) => "助手".to_string(),
            ("tool", _) => "工具结果".to_string(),
            (other, _) => other.to_string(),
        };
        out.push_str(&format!("\n## [{}] {}\n\n", message.seq, who));
        out.push_str(&digest::strip_prompt_wrapper(&message.content));
        out.push('\n');
    }

    let meta = json!({
        "uid": session.uid,
        "tool": session.tool,
        "returned": messages.len(),
        "total": session.message_count,
        "first_seq": messages.first().map(|message| message.seq),
        "last_seq": messages.last().map(|message| message.seq),
    });
    Ok((out, meta))
}

pub fn digest(index: &mut Index, args: &UidArgs) -> Result<(String, Value)> {
    ensure_fresh(index)?;
    let uid = resolve(index, &args.uid)?;
    let session = index
        .get_session(&uid)?
        .ok_or_else(|| anyhow!("未找到会话 {uid}"))?;
    let messages = index.read_messages(
        &uid,
        &ReadOptions {
            tail: None,
            from_seq: None,
            include_tool_results: false,
            only_text: false,
            budget_chars: None,
        },
    )?;
    let digest = digest::build(&session, &messages);
    let markdown = digest.render_markdown();
    Ok((markdown, serde_json::to_value(&digest)?))
}

pub fn handoff(index: &mut Index, args: &HandoffArgs) -> Result<Value> {
    ensure_fresh(index)?;
    let uid = resolve(index, &args.uid)?;
    let to_tool =
        normalize_tool(Some(&args.to_tool))?.ok_or_else(|| anyhow!("to_tool 不能为空"))?;
    let result = handoff::create(
        index,
        &uid,
        &to_tool,
        args.note.as_deref().unwrap_or(""),
        args.tail,
        args.include_tool_results,
    )?;
    Ok(serde_json::to_value(&result)?)
}

pub fn resume_cmd(index: &mut Index, args: &ResumeArgs) -> Result<Value> {
    ensure_fresh(index)?;
    let uid = resolve(index, &args.uid)?;
    let session = index
        .get_session(&uid)?
        .ok_or_else(|| anyhow!("未找到会话 {uid}"))?;
    let command = match args.prompt.as_deref() {
        Some(prompt) if !prompt.trim().is_empty() => {
            crate::tools::resume_with_prompt(&session.tool, &session.session_id, prompt)
        }
        _ => crate::tools::resume_command(&session.tool, &session.session_id),
    };
    Ok(json!({
        "uid": session.uid,
        "tool": session.tool,
        "cwd": session.cwd,
        "command": command,
        "note": "命令需要在对应工程目录下执行；跨工具请用 session_handoff",
    }))
}

pub fn sync(index: &mut Index, args: &SyncArgs) -> Result<Value> {
    let tool = normalize_tool(args.tool.as_deref())?;
    let report = index.sync(args.full, tool.as_deref())?;
    Ok(serde_json::to_value(&report)?)
}

pub fn stats(index: &Index) -> Result<Value> {
    let rows = index.stats()?;
    Ok(json!({
        "index_db": crate::paths::index_db_path().to_string_lossy(),
        "last_sync": fmt_ts(Some(index.last_sync_ms())),
        "tools": rows.iter().map(|(tool, sessions, messages)| json!({
            "tool": tool,
            "sessions": sessions,
            "messages": messages,
        })).collect::<Vec<_>>(),
    }))
}

fn resolve(index: &Index, input: &str) -> Result<String> {
    if input.trim().is_empty() {
        return Err(anyhow!("uid 不能为空"));
    }
    index
        .resolve_uid(input.trim())?
        .ok_or_else(|| anyhow!("未找到会话 {input}（可先用 session_search / session_list 定位）"))
}

fn normalize_tool(tool: Option<&str>) -> Result<Option<String>> {
    let Some(tool) = tool.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let lowered = tool.to_ascii_lowercase();
    let normalized = match lowered.as_str() {
        "claude-code" | "cc" => "claude",
        "kiro-cli" => "kiro",
        "kiroide" | "kiro_ide" | "kiro-agent" | "kiroagent" | "ide" => "kiro-ide",
        "gemini-cli" => "gemini",
        other => other,
    };
    if !crate::tools::SUPPORTED_TOOLS.contains(&normalized) {
        return Err(anyhow!(
            "不支持的工具 {tool}，可选：{}",
            crate::tools::SUPPORTED_TOOLS.join(" / ")
        ));
    }
    Ok(Some(normalized.to_string()))
}

/// 支持 `7d` / `36h` / `90m` / `2026-07-30` / 毫秒时间戳
pub fn parse_time(input: Option<&str>) -> Result<Option<i64>> {
    let Some(raw) = input.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(rest) = raw.strip_suffix('d') {
        if let Ok(days) = rest.parse::<i64>() {
            return Ok(Some(now - days * 86_400_000));
        }
    }
    if let Some(rest) = raw.strip_suffix('h') {
        if let Ok(hours) = rest.parse::<i64>() {
            return Ok(Some(now - hours * 3_600_000));
        }
    }
    if let Some(rest) = raw.strip_suffix('m') {
        if let Ok(minutes) = rest.parse::<i64>() {
            return Ok(Some(now - minutes * 60_000));
        }
    }
    if let Ok(millis) = raw.parse::<i64>() {
        // 10 位当秒，13 位当毫秒
        return Ok(Some(if millis < 100_000_000_000 {
            millis * 1000
        } else {
            millis
        }));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).unwrap();
        let local = naive
            .and_local_timezone(chrono::Local)
            .single()
            .ok_or_else(|| anyhow!("无法解析时间 {raw}"))?;
        return Ok(Some(local.timestamp_millis()));
    }
    Err(anyhow!(
        "无法解析时间 {raw}（支持 7d / 36h / 90m / 2026-07-30 / 时间戳）"
    ))
}

/// 命中片段整理：压掉换行，并把视窗对齐到第一个高亮词附近。
///
/// 自动化会在 prompt 前后塞大段 SYSTEM_CONTEXT，若直接从消息开头截取，
/// 返回给 AI 的全是包裹噪声，看不到真正命中的上下文。
pub fn focus_snippet(snippet: &str, terms: &[String]) -> String {
    let compact = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = compact.chars().collect();
    // 优先用 FTS 的高亮标记定位；多词 AND 时 FTS 的窗口可能落在开头，
    // 这时退回用关键词自身在文本中的位置对齐视窗
    let mark = chars
        .iter()
        .position(|c| *c == '《')
        .or_else(|| first_term_position(&compact, terms));
    let Some(mark) = mark else {
        return crate::model::truncate_chars(&compact, 220);
    };
    let start = mark.saturating_sub(60);
    let end = (mark + 160).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// 关键词在文本中的字符位置（取最早命中的那个）
fn first_term_position(text: &str, terms: &[String]) -> Option<usize> {
    terms
        .iter()
        .filter_map(|term| text.find(term.as_str()))
        .min()
        .map(|byte_index| text[..byte_index].chars().count())
}

pub fn fmt_ts(ts: Option<i64>) -> Option<String> {
    let millis = ts.filter(|value| *value > 0)?;
    chrono::DateTime::from_timestamp_millis(millis).map(|dt| {
        dt.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_window_follows_the_highlight() {
        let snippet = format!("{}《命中》{}", "噪".repeat(200), "尾".repeat(300));
        let focused = focus_snippet(&snippet, &[]);
        assert!(focused.starts_with('…'));
        assert!(focused.contains("《命中》"));
        assert!(focused.chars().count() < 240);
    }

    #[test]
    fn snippet_falls_back_to_term_position() {
        let snippet = format!("{}关键词在后面{}", "噪".repeat(300), "尾".repeat(50));
        let focused = focus_snippet(&snippet, &["关键词".to_string()]);
        assert!(focused.contains("关键词在后面"));
        assert!(focused.starts_with('…'));
    }

    #[test]
    fn relative_time_is_parsed() {
        let now = chrono::Utc::now().timestamp_millis();
        let since = parse_time(Some("7d")).unwrap().unwrap();
        let delta = now - since;
        assert!((delta - 7 * 86_400_000).abs() < 5_000);
    }

    #[test]
    fn tool_aliases_are_normalized() {
        assert_eq!(
            normalize_tool(Some("CC")).unwrap().as_deref(),
            Some("claude")
        );
        assert_eq!(
            normalize_tool(Some("kiro-cli")).unwrap().as_deref(),
            Some("kiro")
        );
        assert!(normalize_tool(Some("cursor")).is_err());
        assert_eq!(normalize_tool(Some("  ")).unwrap(), None);
    }

    #[test]
    fn absolute_date_and_epoch_are_parsed() {
        assert!(parse_time(Some("2026-07-30")).unwrap().is_some());
        assert_eq!(
            parse_time(Some("1785489248089")).unwrap(),
            Some(1785489248089)
        );
        assert_eq!(parse_time(Some("1785489248")).unwrap(), Some(1785489248000));
        assert!(parse_time(Some("昨天")).is_err());
    }
}
