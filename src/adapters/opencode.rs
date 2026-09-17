//! OpenCode：`~/.local/share/opencode/opencode.db`（SQLite）
//!
//! `session` 表存元信息（id / directory / title / time_created / time_updated），
//! `message` 表的 data JSON 里有 role，正文按 `part` 表的 data JSON 拆分（text / tool）。
//! 数据量小且会被整体更新，走全量重解析；只读打开，绝不写入源库。

use super::{forward_fill_ts, parse_ts, Adapter, ParseOutput, SourceStat};
use crate::model::{first_line_summary, Message, SessionPayload, ROLE_ASSISTANT, ROLE_USER};
use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

pub struct OpenCodeAdapter;

pub const TOOL: &str = "opencode";

fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    Ok(conn)
}

impl Adapter for OpenCodeAdapter {
    fn tool(&self) -> &'static str {
        TOOL
    }

    fn list_sources(&self) -> Vec<SourceStat> {
        let db_path = crate::paths::opencode_db_path();
        if !db_path.exists() {
            return Vec::new();
        }
        let Ok(conn) = open_readonly(&db_path) else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT id, time_updated, time_created FROM session")
        else {
            return Vec::new();
        };
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let updated: Option<i64> = row.get(1)?;
            let created: Option<i64> = row.get(2)?;
            Ok((id, updated.or(created).unwrap_or(0)))
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.flatten()
            .map(|(id, updated)| SourceStat {
                tool: TOOL,
                key: format!("sqlite:{}#{id}", db_path.to_string_lossy()),
                path: Some(db_path.clone()),
                // 会话内容变化一定伴随 time_updated 变化，用它当指纹
                size: updated,
                mtime_ms: updated,
                incremental: false,
            })
            .collect()
    }

    fn parse(&self, source: &SourceStat, _from_offset: u64) -> Result<Option<ParseOutput>> {
        let db_path = source
            .path
            .clone()
            .ok_or_else(|| anyhow!("OpenCode 源缺少数据库路径"))?;
        let session_id = source
            .key
            .rsplit('#')
            .next()
            .ok_or_else(|| anyhow!("OpenCode 源标识异常: {}", source.key))?
            .to_string();

        let conn = open_readonly(&db_path)?;
        let meta = conn.query_row(
            "SELECT title, directory, time_created, time_updated FROM session WHERE id = ?1",
            [&session_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        );
        let (title, cwd, created_at, updated_at) = match meta {
            Ok(value) => value,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        // 消息按创建时间排序，正文来自 part 表
        let mut stmt = conn.prepare(
            "SELECT m.id, m.data, m.time_created FROM message m \
             WHERE m.session_id = ?1 ORDER BY m.time_created ASC",
        )?;
        let message_rows: Vec<(String, String, Option<i64>)> = stmt
            .query_map([&session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?
            .flatten()
            .collect();

        let mut part_stmt = conn.prepare(
            "SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created ASC, id ASC",
        )?;

        let mut messages = Vec::new();
        let mut derived_title = None;
        for (message_id, data, time_created) in message_rows {
            let payload: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let role = match payload.get("role").and_then(Value::as_str) {
                Some("user") => ROLE_USER,
                Some("assistant") => ROLE_ASSISTANT,
                _ => continue,
            };
            let ts = payload
                .get("time")
                .and_then(|time| time.get("created"))
                .and_then(parse_ts)
                .or_else(|| time_created.map(normalize_millis));

            let parts: Vec<Value> = part_stmt
                .query_map([&message_id], |row| row.get::<_, String>(0))?
                .flatten()
                .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
                .collect();

            let mut text_buffer = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                text_buffer.push(text.to_string());
                            }
                        }
                    }
                    Some("tool") => {
                        let name = part
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let input = part
                            .get("state")
                            .and_then(|state| state.get("input"))
                            .map(|value| value.to_string())
                            .unwrap_or_default();
                        messages.push(Message::tool_use(name, input, ts));
                        if let Some(output) = part
                            .get("state")
                            .and_then(|state| state.get("output"))
                            .and_then(Value::as_str)
                        {
                            if !output.trim().is_empty() {
                                messages.push(Message::tool_result(Some(name), output, ts));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !text_buffer.is_empty() {
                let text = text_buffer.join("\n");
                if role == ROLE_USER && derived_title.is_none() {
                    derived_title = first_line_summary(&text, 120);
                }
                messages.push(Message::text(role, text, ts));
            }
        }

        forward_fill_ts(&mut messages, created_at.map(normalize_millis));

        let title = title
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| first_line_summary(&value, 120))
            .or(derived_title);

        Ok(Some(ParseOutput {
            session: SessionPayload {
                tool: TOOL.to_string(),
                session_id,
                title,
                cwd: cwd.filter(|value| !value.trim().is_empty()),
                created_at: created_at.map(normalize_millis),
                updated_at: updated_at
                    .map(normalize_millis)
                    .or(created_at.map(normalize_millis)),
                source_path: source.key.clone(),
                messages,
            },
            new_offset: 0,
            full_replace: true,
        }))
    }
}

/// OpenCode 有的字段是秒、有的是毫秒，统一成毫秒
fn normalize_millis(value: i64) -> i64 {
    if value > 1_000_000_000_000 {
        value
    } else {
        value * 1000
    }
}

pub fn resume_command(bin: &str, session_id: &str) -> String {
    format!("{bin} -s {session_id}")
}
