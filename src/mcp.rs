//! MCP stdio 服务端（JSON-RPC 2.0，换行分隔）。
//!
//! 手写协议而不引第三方 SDK：只需要 initialize / tools/list / tools/call 三个方法，
//! 依赖越少越好，也方便被 Claude Code、Codex、Kiro 三家 host 同时挂载。

use crate::index::Index;
use crate::ops;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "ai-session-hub";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn serve() -> Result<()> {
    let mut index = Index::open()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                write_message(
                    &mut stdout,
                    &error_response(Value::Null, -32700, &format!("JSON 解析失败: {error}")),
                )?;
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        // 通知（无 id）不需要响应
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let response = match method.as_str() {
            "initialize" => success(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    "instructions": "跨 AI 工具（Claude Code / Codex / Kiro / Gemini / OpenCode）的会话检索与交接。想回顾历史先用 session_search，读全文用 session_read，换工具继续聊用 session_handoff。",
                }),
            ),
            "ping" => success(id, json!({})),
            "tools/list" => success(id, json!({ "tools": tool_definitions() })),
            "resources/list" => success(id, json!({ "resources": [] })),
            "prompts/list" => success(id, json!({ "prompts": [] })),
            "tools/call" => match call_tool(&mut index, &params) {
                Ok(text) => success(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                ),
                Err(error) => success(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": format!("执行失败：{error}") }],
                        "isError": true,
                    }),
                ),
            },
            other => error_response(id, -32601, &format!("未实现的方法: {other}")),
        };
        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let text = serde_json::to_string(message)?;
    stdout.write_all(text.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn call_tool(index: &mut Index, params: &Value) -> Result<String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("缺少工具名"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        "session_search" => {
            let parsed: ops::SearchArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::search(index, &parsed)?))
        }
        "session_list" => {
            let parsed: ops::ListArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::list(index, &parsed)?))
        }
        "session_read" => {
            let parsed: ops::ReadArgs = serde_json::from_value(args)?;
            let (markdown, meta) = ops::read(index, &parsed)?;
            Ok(format!("{markdown}\n\n---\n{}", pretty(&meta)))
        }
        "session_status" => {
            let parsed: ops::UidArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::status(index, &parsed)?))
        }
        "session_active" => {
            let parsed: ops::ListArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::active(index, &parsed)?))
        }
        "session_digest" => {
            let parsed: ops::UidArgs = serde_json::from_value(args)?;
            let (markdown, _) = ops::digest(index, &parsed)?;
            Ok(markdown)
        }
        "session_handoff" => {
            let parsed: ops::HandoffArgs = serde_json::from_value(args)?;
            let result = ops::handoff(index, &parsed)?;
            Ok(format!(
                "交接包已生成，请在目标工程目录执行下面的命令：\n\n{}\n\n{}",
                result
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                pretty(&result)
            ))
        }
        "session_resume_cmd" => {
            let parsed: ops::ResumeArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::resume_cmd(index, &parsed)?))
        }
        "session_sync" => {
            let parsed: ops::SyncArgs = serde_json::from_value(args)?;
            Ok(pretty(&ops::sync(index, &parsed)?))
        }
        "session_stats" => Ok(pretty(&ops::stats(index)?)),
        other => Err(anyhow::anyhow!("未知工具: {other}")),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn tool_definitions() -> Vec<Value> {
    let tool_enum = json!(crate::tools::SUPPORTED_TOOLS);
    vec![
        json!({
            "name": "session_search",
            "description": "跨 AI 工具全文检索历史会话（Claude Code / Codex / Kiro / Gemini / OpenCode）。支持中文短语。想知道'我以前在哪聊过某件事'时用它。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索词，多个词之间空格分隔（AND）" },
                    "tool": { "type": "string", "enum": tool_enum, "description": "只搜某个工具" },
                    "cwd": { "type": "string", "description": "按工程目录过滤（子串匹配）" },
                    "since": { "type": "string", "description": "起始时间：7d / 36h / 90m / 2026-07-30 / 时间戳" },
                    "until": { "type": "string", "description": "结束时间，同上格式" },
                    "role": { "type": "string", "enum": ["user", "assistant", "tool"] },
                    "include_tool_results": { "type": "boolean", "description": "是否检索工具输出（默认 false，体积大噪声高）" },
                    "limit": { "type": "integer", "description": "返回条数上限，默认 20" },
                    "max_per_session": { "type": "integer", "description": "同一会话最多返回几条命中，默认 3" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "session_list",
            "description": "按工具 / 工程目录 / 时间列出历史会话，用于'我昨天在某个项目里都聊了什么'这类回溯。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "enum": tool_enum },
                    "cwd": { "type": "string" },
                    "since": { "type": "string", "description": "7d / 36h / 2026-07-30 / 时间戳" },
                    "title": { "type": "string", "description": "标题包含的关键字" },
                    "state": {
                        "type": "string",
                        "description": "按状态过滤，支持 running/awaiting_input/awaiting_approval/interrupted/idle/done/unfinished，也接受中文：进行中/待我回复/待确认/已被打断/空闲/已完成/未完成"
                    },
                    "limit": { "type": "integer", "description": "默认 20" }
                }
            }
        }),
        json!({
            "name": "session_status",
            "description": "判断某个会话现在是什么状态：进行中(running)/待我回复(awaiting_input)/待我确认工具执行(awaiting_approval)/已被打断(interrupted)/空闲可续聊(idle)/已完成(done)/已结束但有遗留(unfinished)，并给出判定依据与下一步建议。用户问'那个会话跑完了吗''还在跑吗''要不要我确认'时用它。",
            "inputSchema": {
                "type": "object",
                "properties": { "uid": { "type": "string" } },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_active",
            "description": "总览当前还开着的会话：busy=正在跑或等批准（别打扰），waiting_for_me=在等我回话，open_but_idle=进程还开着但空闲。用户问'现在哪些 AI 在干活''codex 忙不忙''有没有在等我确认的'时用它。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "enum": tool_enum },
                    "cwd": { "type": "string" },
                    "since": { "type": "string", "description": "回看范围，默认 2d" }
                }
            }
        }),
        json!({
            "name": "session_read",
            "description": "读取某个会话的正文（Markdown）。默认剔除工具输出并限制字符预算，避免打爆上下文。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": { "type": "string", "description": "会话标识，形如 kiro:<session_id>，也接受 session_id 前缀" },
                    "tail": { "type": "integer", "description": "只要最近 N 条消息" },
                    "from_seq": { "type": "integer", "description": "从第几条开始（配合分页）" },
                    "include_tool_results": { "type": "boolean", "description": "是否带上工具输出，默认 false" },
                    "only_text": { "type": "boolean", "description": "只要对话正文（丢掉工具调用与输出），回溯'聊了什么'时建议 true" },
                    "budget_chars": { "type": "integer", "description": "字符预算，默认 12000，超出时保留最近的部分" }
                },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_digest",
            "description": "生成某个会话的结构化摘要：用户诉求、关键结论、执行过的命令、涉及文件、可能未完成的事项。纯规则抽取，无模型幻觉。",
            "inputSchema": {
                "type": "object",
                "properties": { "uid": { "type": "string" } },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_handoff",
            "description": "跨工具交接：把某个会话的摘要 + 最近对话原文导出成交接包，并返回目标工具的启动命令（新会话第一句就带上下文）。同工具时额外给出原生续聊命令。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": { "type": "string" },
                    "to_tool": { "type": "string", "enum": tool_enum, "description": "要切换到的工具" },
                    "note": { "type": "string", "description": "接手后的目标 / 下一步指令" },
                    "tail": { "type": "integer", "description": "带上最近 N 条原文，默认 30" },
                    "include_tool_results": { "type": "boolean", "description": "是否带工具输出，默认 false" }
                },
                "required": ["uid", "to_tool"]
            }
        }),
        json!({
            "name": "session_resume_cmd",
            "description": "拿到某个会话在其原生工具里的恢复命令；给了 prompt 就返回'恢复并直接追问'的一条命令。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": { "type": "string" },
                    "prompt": { "type": "string", "description": "恢复后要发的第一句话" }
                },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_sync",
            "description": "刷新索引（默认增量，查询时会自动调用；full=true 时全量重建）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "full": { "type": "boolean" },
                    "tool": { "type": "string", "enum": tool_enum }
                }
            }
        }),
        json!({
            "name": "session_stats",
            "description": "查看索引概况：各工具的会话数与消息数、上次同步时间、索引文件位置。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_name_and_schema() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 10);
        for tool in tools {
            assert!(tool.get("name").and_then(Value::as_str).is_some());
            assert!(tool.get("description").and_then(Value::as_str).is_some());
            assert_eq!(
                tool.get("inputSchema")
                    .and_then(|schema| schema.get("type"))
                    .and_then(Value::as_str),
                Some("object")
            );
        }
    }

    #[test]
    fn unknown_method_returns_jsonrpc_error() {
        let response = error_response(json!(1), -32601, "未实现的方法: foo");
        assert_eq!(response["error"]["code"], json!(-32601));
        assert_eq!(response["id"], json!(1));
    }
}
