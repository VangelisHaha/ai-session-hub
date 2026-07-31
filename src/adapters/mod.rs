//! 各工具的会话解析适配器。
//!
//! ## 增量策略
//! Claude / Codex / Kiro 的会话文件都是 append-only 的 JSONL，因此按**字节偏移**增量解析：
//! 只从上次消费到的位置往后读，且只消费以换行结尾的完整行（尾部半行留到下次）。
//! Codex 本机会话目录已有 1.7G，全量重读代价太高，偏移增量是必须的。
//!
//! Gemini（单文件 JSON）与 OpenCode（SQLite）体量小且会整体重写，采用全量重解析。

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod kiro;
pub mod opencode;

use crate::model::SessionPayload;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 一个数据源（一个会话文件，或 SQLite 中的一个会话）
#[derive(Debug, Clone)]
pub struct SourceStat {
    pub tool: &'static str,
    /// 源标识：文件路径，或 `sqlite:<db path>#<session id>`
    pub key: String,
    pub path: Option<PathBuf>,
    /// 变更指纹：文件用大小，SQLite 用 max(time_updated)
    pub size: i64,
    pub mtime_ms: i64,
    /// 是否支持字节偏移增量
    pub incremental: bool,
}

/// 一次解析的产物
#[derive(Debug)]
pub struct ParseOutput {
    pub session: SessionPayload,
    /// 已消费到的字节偏移（不支持增量时为 0）
    pub new_offset: u64,
    /// true 表示本次是全量解析，上层需先清掉该会话的旧消息
    pub full_replace: bool,
}

pub trait Adapter: Send + Sync {
    fn tool(&self) -> &'static str;
    /// 列出当前所有数据源；源目录不存在时返回空
    fn list_sources(&self) -> Vec<SourceStat>;
    /// 从 `from_offset` 开始解析；`from_offset == 0` 视为全量
    fn parse(&self, source: &SourceStat, from_offset: u64) -> Result<Option<ParseOutput>>;
}

pub fn all_adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude::ClaudeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(kiro::KiroAdapter),
        Box::new(gemini::GeminiAdapter),
        Box::new(opencode::OpenCodeAdapter),
    ]
}

/// 从 `from_offset` 读取完整行，返回 (行内容, 新偏移)。
///
/// 只有以 `\n` 结尾的行才算消费完成：会话文件可能正在被写入，
/// 把半行当成完整 JSON 解析会静默丢消息。
pub fn read_new_lines(path: &Path, from_offset: u64) -> Result<(Vec<String>, u64)> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(from_offset))?;

    let mut lines = Vec::new();
    let mut offset = from_offset;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        if !buf.ends_with(b"\n") {
            // 尾部半行，本次不消费
            break;
        }
        offset += read as u64;
        let line = String::from_utf8_lossy(&buf).trim().to_string();
        if !line.is_empty() {
            lines.push(line);
        }
    }
    Ok((lines, offset))
}

/// 遍历目录下所有指定后缀的文件（递归）
pub fn collect_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => collect_files(&path, ext, out),
            Ok(file_type) if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some(ext) => {
                    out.push(path);
                }
            _ => {}
        }
    }
}

pub fn file_stat(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((size, mtime))
}

/// ISO8601 / 数字时间戳 → 毫秒
pub fn parse_ts(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => {
            let raw = number.as_f64()?;
            // 秒 / 毫秒 / 微秒都可能出现，按量级归一
            let millis = if raw > 1e15 {
                raw / 1000.0
            } else if raw > 1e12 {
                raw
            } else {
                raw * 1000.0
            };
            Some(millis as i64)
        }
        Value::String(text) => chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|dt| dt.timestamp_millis())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|dt| dt.and_utc().timestamp_millis())
            }),
        _ => None,
    }
}

/// 从文件名里提取 UUID 形态的会话 ID
pub fn uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let bytes: Vec<char> = stem.chars().collect();
    // 反向扫描 36 位的 8-4-4-4-12
    if bytes.len() >= 36 {
        for start in (0..=bytes.len() - 36).rev() {
            let candidate: String = bytes[start..start + 36].iter().collect();
            if is_uuid(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let widths = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(widths)
        .all(|(part, width)| part.len() == width && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 时间戳前向填充：Kiro 只在用户提问那一行写 timestamp，
/// 助手消息与工具结果的 meta 是 null。不补齐会导致 96% 的 Kiro 消息没有时间，
/// 时间过滤和按时间排序都会失真。
pub fn forward_fill_ts(messages: &mut [crate::model::Message], seed: Option<i64>) {
    let mut last = seed;
    for message in messages.iter_mut() {
        match message.ts {
            Some(ts) => last = Some(ts),
            None => message.ts = last,
        }
    }
}

/// 判断一段文本能否当标题：JSON 片段、纯符号、包裹块都不行
pub fn looks_like_title(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('<') {
        return false;
    }
    trimmed
        .chars()
        .take(40)
        .any(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 把 JSON 里可能嵌套的文本块拍平成纯文本
pub fn extract_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(extract_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get("text") {
                return text.clone();
            }
            if let Some(inner) = map.get("content") {
                return extract_text(inner);
            }
            if let Some(Value::String(text)) = map.get("data") {
                return text.clone();
            }
            String::new()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn only_complete_lines_are_consumed() {
        let dir = std::env::temp_dir().join(format!("ash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3").unwrap();
        drop(file);

        let (lines, offset) = read_new_lines(&path, 0).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(offset, 16);

        // 补齐第三行后再增量读，只拿到新行
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"}\n").unwrap();
        drop(file);
        let (lines, offset2) = read_new_lines(&path, offset).unwrap();
        assert_eq!(lines, vec!["{\"a\":3}".to_string()]);
        assert!(offset2 > offset);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uuid_is_extracted_from_codex_filename() {
        let path =
            Path::new("rollout-2026-07-31T12-00-00-b2daab07-0161-4897-9d0c-bf4f75c27b9b.jsonl");
        assert_eq!(
            uuid_from_filename(path).as_deref(),
            Some("b2daab07-0161-4897-9d0c-bf4f75c27b9b")
        );
    }

    #[test]
    fn timestamps_accept_iso_and_epoch() {
        assert_eq!(
            parse_ts(&Value::String("2026-07-31T09:14:08.089Z".to_string())),
            Some(1785489248089)
        );
        assert_eq!(
            parse_ts(&serde_json::json!(1780022615392i64)),
            Some(1780022615392)
        );
        assert_eq!(
            parse_ts(&serde_json::json!(1780022615i64)),
            Some(1780022615000)
        );
    }

    #[test]
    fn missing_timestamps_are_forward_filled() {
        let mut messages = vec![
            crate::model::Message::text("user", "问", Some(1000)),
            crate::model::Message::text("assistant", "答", None),
            crate::model::Message::tool_result(None, "输出", None),
            crate::model::Message::text("user", "再问", Some(5000)),
            crate::model::Message::text("assistant", "再答", None),
        ];
        forward_fill_ts(&mut messages, None);
        assert_eq!(
            messages.iter().map(|m| m.ts).collect::<Vec<_>>(),
            vec![Some(1000), Some(1000), Some(1000), Some(5000), Some(5000)]
        );
    }

    #[test]
    fn json_fragments_are_rejected_as_titles() {
        assert!(!looks_like_title("{\"a\":1}"));
        assert!(!looks_like_title("  <environment>"));
        assert!(!looks_like_title("---"));
        assert!(looks_like_title("帮我看下计划表"));
        assert!(looks_like_title("fix the bug"));
    }
}
