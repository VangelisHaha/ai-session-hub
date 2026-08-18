//! 本地索引（SQLite + FTS5 trigram）。
//!
//! ## 为什么用 trigram
//! FTS5 默认的 unicode61 分词器不切中文，中文短语搜不到。trigram 按三字滑窗建索引，
//! 中文、英文子串都能命中，代价是索引体积略大——对本机这点数据量无所谓。
//!
//! ## 索引什么
//! 只把 `text` 与 `tool_use` 入 FTS：工具结果（编译输出、SQL 结果集）体积占源数据 90% 以上，
//! 检索价值低。需要时用 `ASH_INDEX_TOOL_RESULTS=1` 打开。

use crate::adapters::{all_adapters, SourceStat};
use crate::model::{SessionRow, KIND_TEXT, KIND_TOOL_RESULT, KIND_TOOL_USE};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const SCHEMA_VERSION: i64 = 2;

pub struct Index {
    conn: Connection,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub scanned_sources: usize,
    pub changed_sources: usize,
    pub new_messages: usize,
    pub sessions: usize,
    pub elapsed_ms: u128,
    pub per_tool: Vec<ToolSyncStat>,
}

#[derive(Debug, Serialize)]
pub struct ToolSyncStat {
    pub tool: String,
    pub sources: usize,
    pub changed: usize,
    pub new_messages: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub uid: String,
    pub tool: String,
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub role: String,
    pub kind: String,
    pub seq: i64,
    pub ts: Option<i64>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageRow {
    pub seq: i64,
    pub role: String,
    pub kind: String,
    pub tool_name: Option<String>,
    pub ts: Option<i64>,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub query: String,
    pub tool: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub role: Option<String>,
    pub include_tool_results: bool,
    pub limit: usize,
    pub max_per_session: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub tool: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<i64>,
    pub title_like: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    pub tail: Option<usize>,
    pub from_seq: Option<i64>,
    pub include_tool_results: bool,
    /// 只要对话正文（丢掉 tool_use / tool_result）——回溯"聊了什么"时最有用
    pub only_text: bool,
    pub budget_chars: Option<usize>,
}

impl Index {
    pub fn open() -> Result<Self> {
        let path = crate::paths::index_db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建索引目录失败: {}", parent.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("打开索引失败: {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        let index = Self { conn };
        index.migrate()?;
        Ok(index)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let index = Self { conn };
        index.migrate()?;
        Ok(index)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );",
        )?;
        // 索引是纯派生数据，schema 变化时直接重建（全量约半分钟），不写迁移脚本
        let current: Option<i64> = self
            .get_meta("schema_version")?
            .and_then(|value| value.parse().ok());
        if matches!(current, Some(version) if version != SCHEMA_VERSION) {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS messages_fts;
                 DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS sources;",
            )?;
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                uid TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                session_id TEXT NOT NULL,
                title TEXT,
                cwd TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                source_path TEXT,
                message_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE INDEX IF NOT EXISTS idx_sessions_tool ON sessions(tool);
             CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                session_uid TEXT NOT NULL,
                source_key TEXT NOT NULL DEFAULT '',
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                kind TEXT NOT NULL,
                tool_name TEXT,
                ts INTEGER,
                content TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_uid, seq);
             CREATE INDEX IF NOT EXISTS idx_messages_source ON messages(source_key);
             CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts DESC);
             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content,
                tokenize='trigram',
                content='messages',
                content_rowid='id'
             );
             CREATE TABLE IF NOT EXISTS sources (
                key TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                session_uid TEXT,
                size INTEGER NOT NULL DEFAULT 0,
                mtime_ms INTEGER NOT NULL DEFAULT 0,
                offset_bytes INTEGER NOT NULL DEFAULT 0
             );",
        )?;
        self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn last_sync_ms(&self) -> i64 {
        self.get_meta("last_sync_ms")
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    // ---------------------------------------------------------------- 同步

    /// 增量同步。`full=true` 时忽略偏移全量重建；`only_tool` 可限定单个工具。
    pub fn sync(&mut self, full: bool, only_tool: Option<&str>) -> Result<SyncReport> {
        let started = std::time::Instant::now();
        let mut report = SyncReport::default();
        let index_tool_results = std::env::var("ASH_INDEX_TOOL_RESULTS")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        for adapter in all_adapters() {
            if let Some(only) = only_tool {
                if adapter.tool() != only {
                    continue;
                }
            }
            let sources = adapter.list_sources();
            let mut stat = ToolSyncStat {
                tool: adapter.tool().to_string(),
                sources: sources.len(),
                changed: 0,
                new_messages: 0,
            };
            report.scanned_sources += sources.len();

            for source in sources {
                let previous = self.load_source(&source.key)?;
                let unchanged = previous
                    .as_ref()
                    .map(|prev| prev.size == source.size && prev.mtime_ms == source.mtime_ms)
                    .unwrap_or(false);
                if unchanged && !full {
                    continue;
                }

                // 文件被截断或不支持增量时回到全量
                let from_offset = match (&previous, full, source.incremental) {
                    (_, true, _) | (_, _, false) | (None, _, _) => 0,
                    (Some(prev), _, true) => {
                        if source.size < prev.size {
                            0
                        } else {
                            prev.offset_bytes
                        }
                    }
                };

                match adapter.parse(&source, from_offset) {
                    Ok(Some(output)) => {
                        let inserted =
                            self.apply_parse_output(&source, output, index_tool_results)?;
                        stat.changed += 1;
                        stat.new_messages += inserted;
                        report.changed_sources += 1;
                        report.new_messages += inserted;
                    }
                    Ok(None) => {
                        // 无法识别的源，仍记录指纹避免反复重试
                        self.save_source(&source, 0, None)?;
                    }
                    Err(error) => {
                        eprintln!("[warn] 解析失败 {}: {error}", source.key);
                        self.save_source(&source, from_offset, None)?;
                    }
                }
            }
            report.per_tool.push(stat);
        }

        report.sessions = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })? as usize;
        let now = chrono::Utc::now().timestamp_millis();
        self.set_meta("last_sync_ms", &now.to_string())?;
        report.elapsed_ms = started.elapsed().as_millis();
        Ok(report)
    }

    fn apply_parse_output(
        &mut self,
        source: &SourceStat,
        output: crate::adapters::ParseOutput,
        index_tool_results: bool,
    ) -> Result<usize> {
        let session = output.session;
        let uid = session.uid();
        let tx = self.conn.transaction()?;

        if output.full_replace {
            // 全量重解析：只摘掉**本数据源**的旧消息。
            // Claude 的子代理会话文件与主会话共用 sessionId，按会话删会把主会话内容抹掉。
            let mut stmt = tx.prepare(
                "SELECT id, content FROM messages WHERE session_uid = ?1 AND source_key = ?2",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map(params![&uid, &source.key], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .flatten()
                .collect();
            drop(stmt);
            for (id, content) in rows {
                tx.execute(
                    "INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', ?1, ?2)",
                    params![id, content],
                )
                .ok();
            }
            tx.execute(
                "DELETE FROM messages WHERE session_uid = ?1 AND source_key = ?2",
                params![&uid, &source.key],
            )?;
        }

        tx.execute(
            "INSERT INTO sessions(uid, tool, session_id, title, cwd, created_at, updated_at, source_path, message_count)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
             ON CONFLICT(uid) DO UPDATE SET
                title = COALESCE(excluded.title, sessions.title),
                cwd = COALESCE(excluded.cwd, sessions.cwd),
                created_at = COALESCE(sessions.created_at, excluded.created_at),
                updated_at = MAX(COALESCE(excluded.updated_at, 0), COALESCE(sessions.updated_at, 0)),
                source_path = COALESCE(excluded.source_path, sessions.source_path)",
            params![
                uid,
                session.tool,
                session.session_id,
                session.title,
                session.cwd,
                session.created_at,
                session.updated_at,
                session.source_path,
            ],
        )?;

        let mut seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), -1) FROM messages WHERE session_uid = ?1",
            [&uid],
            |row| row.get(0),
        )?;

        let mut inserted = 0usize;
        {
            let mut insert_msg = tx.prepare(
                "INSERT INTO messages(session_uid, source_key, seq, role, kind, tool_name, ts, content)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut insert_fts =
                tx.prepare("INSERT INTO messages_fts(rowid, content) VALUES(?1, ?2)")?;
            for message in session.messages {
                seq += 1;
                insert_msg.execute(params![
                    uid,
                    source.key,
                    seq,
                    message.role,
                    message.kind,
                    message.tool_name,
                    message.ts,
                    message.content,
                ])?;
                let rowid = tx.last_insert_rowid();
                let indexable = match message.kind.as_str() {
                    KIND_TEXT | KIND_TOOL_USE => true,
                    KIND_TOOL_RESULT => index_tool_results,
                    _ => false,
                };
                if indexable {
                    insert_fts.execute(params![rowid, message.content])?;
                }
                inserted += 1;
            }
        }

        tx.execute(
            "UPDATE sessions SET message_count = (SELECT COUNT(*) FROM messages WHERE session_uid = ?1) WHERE uid = ?1",
            [&uid],
        )?;
        tx.execute(
            "INSERT INTO sources(key, tool, session_uid, size, mtime_ms, offset_bytes)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET
                session_uid = excluded.session_uid,
                size = excluded.size,
                mtime_ms = excluded.mtime_ms,
                offset_bytes = excluded.offset_bytes",
            params![
                source.key,
                source.tool,
                uid,
                source.size,
                source.mtime_ms,
                output.new_offset as i64
            ],
        )?;
        tx.commit()?;
        Ok(inserted)
    }

    fn load_source(&self, key: &str) -> Result<Option<SourceRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT size, mtime_ms, offset_bytes FROM sources WHERE key = ?1",
                [key],
                |row| {
                    Ok(SourceRecord {
                        size: row.get(0)?,
                        mtime_ms: row.get(1)?,
                        offset_bytes: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?)
    }

    fn save_source(
        &self,
        source: &SourceStat,
        offset: u64,
        session_uid: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sources(key, tool, session_uid, size, mtime_ms, offset_bytes)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET
                size = excluded.size,
                mtime_ms = excluded.mtime_ms,
                offset_bytes = excluded.offset_bytes",
            params![
                source.key,
                source.tool,
                session_uid,
                source.size,
                source.mtime_ms,
                offset as i64
            ],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- 查询

    pub fn search(&self, options: &SearchOptions) -> Result<Vec<SearchHit>> {
        let limit = options.limit.max(1);
        let max_per_session = options.max_per_session.max(1);
        let terms: Vec<String> = options
            .query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // trigram 至少需要 3 个字符，短词退化为 LIKE
        let use_fts = terms.iter().all(|term| term.chars().count() >= 3);

        let mut sql = String::from(
            "WITH hit AS (
                SELECT m.id, m.session_uid, m.role, m.kind, m.seq, m.ts, ",
        );
        if use_fts {
            sql.push_str("snippet(messages_fts, 0, '《', '》', '…', 14) AS snippet\n");
            sql.push_str("FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid\n");
            sql.push_str("WHERE messages_fts MATCH ?1\n");
        } else {
            sql.push_str("substr(m.content, 1, 240) AS snippet\n");
            sql.push_str("FROM messages m\nWHERE 1=1\n");
        }

        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if use_fts {
            let match_expr = terms
                .iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            binds.push(Box::new(match_expr));
        } else {
            for term in &terms {
                sql.push_str(" AND m.content LIKE ?\n");
                binds.push(Box::new(format!("%{term}%")));
            }
        }

        if !options.include_tool_results {
            sql.push_str(" AND m.kind <> 'tool_result'\n");
        }
        if let Some(role) = &options.role {
            sql.push_str(" AND m.role = ?\n");
            binds.push(Box::new(role.clone()));
        }
        // 时间戳缺失表示"未知"，不是"1970 年"。COALESCE(ts,0) 会让这类消息
        // 在任何 since 过滤下都不可见；改为放行 null，由会话级时间兜底判断。
        if let Some(since) = options.since {
            sql.push_str(" AND (m.ts IS NULL OR m.ts >= ?)\n");
            binds.push(Box::new(since));
        }
        if let Some(until) = options.until {
            sql.push_str(" AND (m.ts IS NULL OR m.ts <= ?)\n");
            binds.push(Box::new(until));
        }
        sql.push_str(
            "),
            joined AS (
                SELECT hit.*, s.tool, s.session_id, s.title, s.cwd,
                       ROW_NUMBER() OVER (PARTITION BY hit.session_uid ORDER BY hit.ts DESC, hit.seq DESC) AS rn
                FROM hit JOIN sessions s ON s.uid = hit.session_uid
                WHERE 1=1\n",
        );
        if let Some(tool) = &options.tool {
            sql.push_str(" AND s.tool = ?\n");
            binds.push(Box::new(tool.clone()));
        }
        if let Some(cwd) = &options.cwd {
            sql.push_str(" AND COALESCE(s.cwd, '') LIKE ?\n");
            binds.push(Box::new(format!("%{cwd}%")));
        }
        // 消息时间为 null 时用所属会话的时间兜底，避免放行范围外的会话
        if let Some(since) = options.since {
            sql.push_str(
                " AND (hit.ts IS NOT NULL OR COALESCE(s.updated_at, s.created_at, 0) >= ?)\n",
            );
            binds.push(Box::new(since));
        }
        if let Some(until) = options.until {
            sql.push_str(
                " AND (hit.ts IS NOT NULL OR COALESCE(s.created_at, s.updated_at, 0) <= ?)\n",
            );
            binds.push(Box::new(until));
        }
        sql.push_str(
            ")
            SELECT session_uid, tool, session_id, title, cwd, role, kind, seq, ts, snippet
            FROM joined WHERE rn <= ?
            ORDER BY COALESCE(ts, 0) DESC, seq DESC
            LIMIT ?",
        );
        binds.push(Box::new(max_per_session as i64));
        binds.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|bind| bind.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(SearchHit {
                uid: row.get(0)?,
                tool: row.get(1)?,
                session_id: row.get(2)?,
                title: row.get(3)?,
                cwd: row.get(4)?,
                role: row.get(5)?,
                kind: row.get(6)?,
                seq: row.get(7)?,
                ts: row.get(8)?,
                snippet: row.get(9)?,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn list_sessions(&self, options: &ListOptions) -> Result<Vec<SessionRow>> {
        let mut sql = String::from(
            "SELECT uid, tool, session_id, title, cwd, created_at, updated_at, source_path, message_count
             FROM sessions WHERE 1=1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(tool) = &options.tool {
            sql.push_str(" AND tool = ?");
            binds.push(Box::new(tool.clone()));
        }
        if let Some(cwd) = &options.cwd {
            sql.push_str(" AND COALESCE(cwd, '') LIKE ?");
            binds.push(Box::new(format!("%{cwd}%")));
        }
        if let Some(since) = options.since {
            sql.push_str(" AND COALESCE(updated_at, created_at, 0) >= ?");
            binds.push(Box::new(since));
        }
        if let Some(title) = &options.title_like {
            sql.push_str(" AND COALESCE(title, '') LIKE ?");
            binds.push(Box::new(format!("%{title}%")));
        }
        sql.push_str(" ORDER BY COALESCE(updated_at, created_at, 0) DESC LIMIT ?");
        binds.push(Box::new(options.limit.max(1) as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|bind| bind.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            let tool: String = row.get(1)?;
            let session_id: String = row.get(2)?;
            Ok(SessionRow {
                uid: row.get(0)?,
                resume_command: Some(crate::tools::resume_command(&tool, &session_id)),
                tool,
                session_id,
                title: row.get(3)?,
                cwd: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                source_path: row.get(7)?,
                message_count: row.get(8)?,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn get_session(&self, uid: &str) -> Result<Option<SessionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT uid, tool, session_id, title, cwd, created_at, updated_at, source_path, message_count
                 FROM sessions WHERE uid = ?1",
                [uid],
                |row| {
                    let tool: String = row.get(1)?;
                    let session_id: String = row.get(2)?;
                    Ok(SessionRow {
                        uid: row.get(0)?,
                        resume_command: Some(crate::tools::resume_command(&tool, &session_id)),
                        tool,
                        session_id,
                        title: row.get(3)?,
                        cwd: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                        source_path: row.get(7)?,
                        message_count: row.get(8)?,
                    })
                },
            )
            .optional()?)
    }

    /// 会话 uid 支持模糊定位：完整 uid、`tool:前缀`、或裸 session_id 前缀。
    ///
    /// 前缀命中多个会话时返回全部候选，由上层报错提示补长前缀 ——
    /// 静默取"最近更新的那个"会让用户读到、甚至交接错的会话。
    pub fn resolve_uid_candidates(&self, input: &str, limit: usize) -> Result<Vec<String>> {
        if self.get_session(input)?.is_some() {
            return Ok(vec![input.to_string()]);
        }
        let pattern = format!("{input}%");
        let mut stmt = self.conn.prepare(
            "SELECT uid FROM sessions
             WHERE uid LIKE ?1 OR session_id LIKE ?1
             ORDER BY COALESCE(updated_at, 0) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit.max(1) as i64], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.flatten().collect())
    }

    /// 唯一命中时返回该 uid；无命中返回 None。多个候选由 `resolve_uid_candidates` 处理
    #[cfg(test)]
    pub fn resolve_uid(&self, input: &str) -> Result<Option<String>> {
        Ok(self.resolve_uid_candidates(input, 1)?.into_iter().next())
    }

    pub fn read_messages(&self, uid: &str, options: &ReadOptions) -> Result<Vec<MessageRow>> {
        let mut sql = String::from(
            "SELECT seq, role, kind, tool_name, ts, content FROM messages WHERE session_uid = ?1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(uid.to_string())];
        if options.only_text {
            sql.push_str(" AND kind = 'text'");
        } else if !options.include_tool_results {
            sql.push_str(" AND kind <> 'tool_result'");
        }
        if let Some(from_seq) = options.from_seq {
            sql.push_str(" AND seq >= ?");
            binds.push(Box::new(from_seq));
        }
        // 同一会话可能有多个数据源（如 Claude 主会话 + 子代理），按时间穿插才是真实顺序
        sql.push_str(" ORDER BY COALESCE(ts, 0) ASC, seq ASC");

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|bind| bind.as_ref()).collect();
        let mut rows: Vec<MessageRow> = stmt
            .query_map(params.as_slice(), |row| {
                Ok(MessageRow {
                    seq: row.get(0)?,
                    role: row.get(1)?,
                    kind: row.get(2)?,
                    tool_name: row.get(3)?,
                    ts: row.get(4)?,
                    content: row.get(5)?,
                })
            })?
            .flatten()
            .collect();

        if let Some(tail) = options.tail {
            if rows.len() > tail {
                rows = rows.split_off(rows.len() - tail);
            }
        }
        if let Some(budget) = options.budget_chars {
            rows = trim_to_budget(rows, budget);
        }
        Ok(rows)
    }

    /// 取会话最后一条消息的形态（含工具调用/结果），用于活跃度判定。
    ///
    /// 状态易失，所以不落库、每次实时读；`messages(session_uid, seq)` 上有索引，单会话是毫秒级。
    pub fn last_message(&self, uid: &str) -> Result<crate::liveness::LastMessage> {
        let row = self
            .conn
            .query_row(
                "SELECT role, kind, tool_name, substr(content, 1, 600), ts
                 FROM messages WHERE session_uid = ?1
                 ORDER BY seq DESC LIMIT 1",
                [uid],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;

        // 未完成信号只看最近几条助手正文，避免为了状态判定跑整会话摘要
        let mut stmt = self.conn.prepare(
            "SELECT content FROM messages
             WHERE session_uid = ?1 AND role = 'assistant' AND kind = 'text'
             ORDER BY seq DESC LIMIT 3",
        )?;
        let has_open_items = stmt
            .query_map([uid], |row| row.get::<_, String>(0))?
            .flatten()
            .any(|content| {
                crate::digest::OPEN_SIGNALS
                    .iter()
                    .any(|signal| content.contains(signal))
            });

        Ok(match row {
            Some((role, kind, tool_name, head, ts)) => crate::liveness::LastMessage {
                role,
                kind,
                tool_name,
                head,
                ts,
                has_open_items,
            },
            None => crate::liveness::LastMessage {
                has_open_items,
                ..Default::default()
            },
        })
    }

    pub fn stats(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.tool, COUNT(DISTINCT s.uid), COALESCE(SUM(s.message_count), 0)
             FROM sessions s GROUP BY s.tool ORDER BY s.tool",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.flatten().collect())
    }
}

struct SourceRecord {
    size: i64,
    mtime_ms: i64,
    offset_bytes: u64,
}

/// 从后往前保留消息，直到超出字符预算——回溯时最近的上下文最有价值。
fn trim_to_budget(rows: Vec<MessageRow>, budget: usize) -> Vec<MessageRow> {
    let mut used = 0usize;
    let mut kept: Vec<MessageRow> = Vec::new();
    for row in rows.into_iter().rev() {
        let cost = row.content.chars().count() + 24;
        if used + cost > budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(row);
    }
    kept.reverse();
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, SessionPayload};

    fn seed(index: &mut Index, tool: &str, id: &str, messages: Vec<Message>) {
        seed_with_source(
            index,
            tool,
            id,
            &format!("/tmp/{tool}-{id}.jsonl"),
            messages,
        );
    }

    fn seed_with_source(
        index: &mut Index,
        tool: &str,
        id: &str,
        source_key: &str,
        messages: Vec<Message>,
    ) {
        let source = SourceStat {
            tool: "claude",
            key: source_key.to_string(),
            path: None,
            size: 1,
            mtime_ms: 1,
            incremental: true,
        };
        let output = crate::adapters::ParseOutput {
            session: SessionPayload {
                tool: tool.to_string(),
                session_id: id.to_string(),
                title: Some(format!("会话 {id}")),
                cwd: Some("/Users/demo/project".to_string()),
                created_at: Some(1_700_000_000_000),
                updated_at: Some(1_700_000_100_000),
                source_path: source_key.to_string(),
                messages,
            },
            new_offset: 10,
            full_replace: true,
        };
        index.apply_parse_output(&source, output, false).unwrap();
    }

    #[test]
    fn chinese_phrase_is_searchable_via_trigram() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "kiro",
            "s1",
            vec![
                Message::text("user", "帮我把额度刷新一下", Some(1_700_000_001_000)),
                Message::text("assistant", "三个账号已经刷新完成", Some(1_700_000_002_000)),
            ],
        );
        let hits = index
            .search(&SearchOptions {
                query: "额度刷新".to_string(),
                limit: 10,
                max_per_session: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].uid, "kiro:s1");
        assert!(hits[0].snippet.contains('《'));
    }

    #[test]
    fn tool_results_are_excluded_by_default() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "codex",
            "s2",
            vec![
                Message::text("user", "跑一下测试", None),
                Message::tool_result(Some("shell"), "测试全部通过", None),
            ],
        );
        let default_read = index
            .read_messages("codex:s2", &ReadOptions::default())
            .unwrap();
        assert_eq!(default_read.len(), 1);
        let with_results = index
            .read_messages(
                "codex:s2",
                &ReadOptions {
                    include_tool_results: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(with_results.len(), 2);
    }

    #[test]
    fn incremental_append_keeps_existing_messages() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "claude",
            "s3",
            vec![Message::text("user", "第一问", None)],
        );
        // 第二次以增量方式追加
        let source = SourceStat {
            tool: "claude",
            key: "/tmp/claude-s3.jsonl".to_string(),
            path: None,
            size: 2,
            mtime_ms: 2,
            incremental: true,
        };
        let output = crate::adapters::ParseOutput {
            session: SessionPayload {
                tool: "claude".to_string(),
                session_id: "s3".to_string(),
                title: None,
                cwd: None,
                created_at: None,
                updated_at: Some(1_700_000_200_000),
                source_path: "/tmp/claude-s3.jsonl".to_string(),
                messages: vec![Message::text("assistant", "第一答", None)],
            },
            new_offset: 40,
            full_replace: false,
        };
        index.apply_parse_output(&source, output, false).unwrap();

        let rows = index
            .read_messages("claude:s3", &ReadOptions::default())
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 0);
        assert_eq!(rows[1].seq, 1);
        // 标题不会被 None 覆盖
        let session = index.get_session("claude:s3").unwrap().unwrap();
        assert_eq!(session.title.as_deref(), Some("会话 s3"));
        assert_eq!(session.message_count, 2);
        assert_eq!(
            session.resume_command.as_deref(),
            Some("claude --resume s3")
        );
    }

    #[test]
    fn budget_keeps_latest_messages() {
        let rows = vec![
            MessageRow {
                seq: 0,
                role: "user".into(),
                kind: "text".into(),
                tool_name: None,
                ts: None,
                content: "a".repeat(100),
            },
            MessageRow {
                seq: 1,
                role: "assistant".into(),
                kind: "text".into(),
                tool_name: None,
                ts: None,
                content: "b".repeat(100),
            },
        ];
        let kept = trim_to_budget(rows, 130);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].seq, 1);
    }

    #[test]
    fn short_query_falls_back_to_like() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "kiro",
            "s4",
            vec![Message::text("user", "ab 测试", None)],
        );
        let hits = index
            .search(&SearchOptions {
                query: "ab".to_string(),
                limit: 10,
                max_per_session: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn uid_can_be_resolved_by_prefix() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "kiro",
            "abcdef123456",
            vec![Message::text("user", "hi", None)],
        );
        assert_eq!(
            index.resolve_uid("abcdef12").unwrap().as_deref(),
            Some("kiro:abcdef123456")
        );
    }

    /// 前缀撞车时必须暴露全部候选，让上层报歧义而不是静默选一个
    #[test]
    fn colliding_prefixes_return_all_candidates() {
        let mut index = Index::memory().unwrap();
        seed(
            &mut index,
            "kiro",
            "abcdef111111",
            vec![Message::text("user", "一", None)],
        );
        seed(
            &mut index,
            "kiro",
            "abcdef222222",
            vec![Message::text("user", "二", None)],
        );
        let candidates = index.resolve_uid_candidates("abcdef", 5).unwrap();
        assert_eq!(candidates.len(), 2);
        // 完整 uid 仍然唯一命中
        assert_eq!(
            index
                .resolve_uid_candidates("kiro:abcdef111111", 5)
                .unwrap(),
            vec!["kiro:abcdef111111".to_string()]
        );
    }

    /// 时间戳为 null 的消息不应被 since 过滤静默吞掉，
    /// 但也不能因此放行范围外的会话
    #[test]
    fn null_timestamps_follow_their_session_time() {
        let mut index = Index::memory().unwrap();
        // seed 出来的会话 updated_at = 1_700_000_100_000
        seed(
            &mut index,
            "kiro",
            "s10",
            vec![Message::text("user", "关键词在这里", None)],
        );
        // since 早于会话时间：null 消息应当可见
        let hits = index
            .search(&SearchOptions {
                query: "关键词".to_string(),
                since: Some(1_600_000_000_000),
                limit: 10,
                max_per_session: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "会话在范围内，null 时间的消息应可见");

        // since 晚于会话时间：不该命中
        let hits = index
            .search(&SearchOptions {
                query: "关键词".to_string(),
                since: Some(1_900_000_000_000),
                limit: 10,
                max_per_session: 3,
                ..Default::default()
            })
            .unwrap();
        assert!(hits.is_empty(), "会话在范围外，不应因 null 时间被放行");
    }

    /// Claude 的子代理文件与主会话共用 sessionId，重解析不能互相抹掉
    #[test]
    fn sources_sharing_one_session_do_not_wipe_each_other() {
        let mut index = Index::memory().unwrap();
        seed_with_source(
            &mut index,
            "claude",
            "s9",
            "/tmp/main.jsonl",
            vec![Message::text("user", "主会话提问", Some(1_700_000_001_000))],
        );
        seed_with_source(
            &mut index,
            "claude",
            "s9",
            "/tmp/s9/subagents/agent-a.jsonl",
            vec![Message::text(
                "assistant",
                "子代理输出",
                Some(1_700_000_002_000),
            )],
        );
        // 再次全量重解析主会话文件，子代理消息必须还在
        seed_with_source(
            &mut index,
            "claude",
            "s9",
            "/tmp/main.jsonl",
            vec![Message::text("user", "主会话提问", Some(1_700_000_001_000))],
        );

        let rows = index
            .read_messages("claude:s9", &ReadOptions::default())
            .unwrap();
        assert_eq!(rows.len(), 2);
        // 按时间穿插排序
        assert_eq!(rows[0].content, "主会话提问");
        assert_eq!(rows[1].content, "子代理输出");
        assert_eq!(
            index
                .get_session("claude:s9")
                .unwrap()
                .unwrap()
                .message_count,
            2
        );
    }
}
