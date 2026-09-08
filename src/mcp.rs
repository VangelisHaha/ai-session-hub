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
                    "instructions": "Local history across AI coding tools (Claude Code / Codex / Kiro / Gemini / OpenCode). Use these tools whenever the user refers to earlier/other conversations, past work, previous chats, prior sessions, \"what did I do\", \"where did I discuss\", \"resume\", \"continue that\", \"is it still running\", or wants to move context to another AI tool. Search history -> session_search; list/browse -> session_list; read transcript -> session_read; summarize -> session_digest; live status -> session_status / session_active; move to another tool -> session_handoff / session_resume_cmd.\n跨 AI 工具的本地会话检索与交接。中文短指令路由：'会话搜索/搜会话/找下会话/以前聊过' -> session_search；'最近会话/会话列表/看下会话/昨天的会话' -> session_list；'活跃会话/哪些在跑/在等我确认' -> session_active；'跑完了吗/还在跑吗' -> session_status；'读会话/聊天记录/当时说了什么' -> session_read；'会话摘要/总结一下/复盘' -> session_digest；'交接/换工具继续/带上下文过去' -> session_handoff；'续聊/接着聊/恢复会话' -> session_resume_cmd；'刷新索引/重建索引' -> session_sync；'索引概况/有多少会话' -> session_stats。只要用户提到'会话''之前''以前''上次''历史'并指向过去的对话，就先查这套工具，不要凭记忆回答。",
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
            "description": "Full-text search across past AI coding sessions (Claude Code / Codex / Kiro / Gemini / OpenCode). USE THIS whenever the user asks about earlier conversations, previous chats, past work, prior context, or anything they \"talked about before\" / \"did last week\" / \"discussed somewhere\" — e.g. \"where did I discuss X\", \"did I already fix Y\", \"find that conversation about Z\", \"search my history\", \"what did we decide about ...\". Also the right tool when the user cannot remember which AI tool or project a discussion happened in. Supports Chinese phrases.\n跨 AI 工具全文检索历史会话。想知道'我以前在哪聊过某件事'时用它。中文触发词：会话搜索、搜索会话、搜会话、找会话、找下会话、查会话、查下会话、搜历史、搜下历史、历史会话、以前聊过、之前聊过、之前讨论过、上次聊的、我在哪聊过、之前是怎么弄的。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search terms; multiple terms are AND-ed, space separated. 检索词，多个词之间空格分隔（AND）" },
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
            "description": "List / browse past AI sessions by tool, project directory, title, state or time range. USE THIS for \"what was I working on yesterday\", \"list my recent sessions\", \"show my chats in this repo\", \"which sessions are unfinished\", \"my codex sessions last week\", or any request to enumerate history rather than keyword-search it.\n按工具 / 工程目录 / 时间列出历史会话，用于'我昨天在某个项目里都聊了什么'这类回溯。中文触发词：最近会话、最近的会话、会话列表、列一下会话、列出会话、看下会话、看看会话、我的会话、有哪些会话、昨天的会话、今天的会话、本周会话、这个项目的会话、没做完的会话、未完成会话。",
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
            "description": "Check the live state of one session: running / awaiting_input / awaiting_approval / interrupted / idle / done / unfinished, with the evidence behind the verdict and a suggested next step. USE THIS for \"is that session done?\", \"is it still running?\", \"did it finish?\", \"is it stuck?\", \"is it waiting on me to approve something?\".\n判断某个会话现在是什么状态：进行中/待我回复/待我确认工具执行/已被打断/空闲可续聊/已完成/已结束但有遗留。中文触发词：会话状态、跑完了吗、还在跑吗、结束了吗、好了没、卡住了吗、是不是在等我、要不要我确认、这个会话怎么了。",
            "inputSchema": {
                "type": "object",
                "properties": { "uid": { "type": "string" } },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_active",
            "description": "Overview of all currently open AI sessions, bucketed into busy (working or waiting for approval — don't interrupt), waiting_for_me (needs my reply), and open_but_idle. USE THIS for \"which AI agents are working right now\", \"is codex busy\", \"anything waiting on me\", \"what's still open\", \"do I have any pending approvals\".\n总览当前还开着的会话，用户问'现在哪些 AI 在干活''有没有在等我确认的'时用它。中文触发词：活跃会话、当前会话、在跑的会话、开着的会话、哪些在跑、谁在干活、忙不忙、有没有等我确认的、有没有待处理的、还有什么没关。",
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
            "description": "Read the transcript of one session as Markdown. USE THIS after session_search / session_list to actually see what was said, or for \"show me that conversation\", \"open that session\", \"what exactly did it say\", \"read the last N messages\". Tool output is stripped and a character budget is applied by default so it won't blow up the context.\n读取某个会话的正文（Markdown），默认剔除工具输出并限制字符预算。中文触发词：读会话、看会话内容、打开会话、会话正文、聊天记录、当时说了什么、原文、最后几条消息、翻一下那个会话。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uid": { "type": "string", "description": "Session UID like kiro:<session_id>; a unique session_id prefix also works. Get it from session_search / session_list." },
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
            "description": "Structured summary of one session: what the user wanted, key conclusions, commands run, files touched, and likely unfinished items. USE THIS for \"summarize that session\", \"recap what happened\", \"tl;dr of that chat\", \"what was left unfinished\", \"catch me up on that\". Deterministic rule-based extraction, no model, no hallucination.\n生成某个会话的结构化摘要：用户诉求、关键结论、执行过的命令、涉及文件、可能未完成的事项。中文触发词：会话摘要、总结会话、总结一下那个会话、复盘、回顾一下、干了什么、结论是什么、还有什么没做完、遗留问题。",
            "inputSchema": {
                "type": "object",
                "properties": { "uid": { "type": "string" } },
                "required": ["uid"]
            }
        }),
        json!({
            "name": "session_handoff",
            "description": "Hand off a session to another AI tool: exports summary + recent transcript into a handoff package and returns the launch command so the new session starts with full context. USE THIS for \"continue this in claude/codex/kiro\", \"move this over to another tool\", \"switch tools but keep context\", \"transfer this conversation\", \"port the context\". Same-tool handoff also returns the native resume command.\n跨工具交接：导出摘要 + 最近对话原文，并返回目标工具的启动命令。中文触发词：会话交接、交接、转到、换个工具继续、拿到 claude 去弄、换 codex 继续聊、把上下文带过去、迁移上下文、导出上下文。",
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
            "description": "Get the native resume command for a session in its own tool. USE THIS for \"how do I resume that session\", \"reopen that chat\", \"continue where I left off\", \"give me the command to pick that back up\". If a prompt is supplied, returns a single command that resumes and immediately asks that follow-up.\n拿到某个会话在其原生工具里的恢复命令；给了 prompt 就返回'恢复并直接追问'的一条命令。中文触发词：恢复会话、续聊、接着聊、怎么继续、重新打开、回到那个会话、恢复命令。",
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
            "description": "Refresh the session index (incremental by default; full=true rebuilds from scratch). Queries auto-refresh, so only call this for \"reindex\", \"rebuild the index\", \"resync sessions\", or when a session you just had is missing from search results.\n刷新索引（默认增量，查询时会自动调用；full=true 时全量重建）。中文触发词：刷新索引、重建索引、同步会话、重新索引、索引更新、搜不到刚才的会话。",
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
            "description": "Index overview: session and message counts per tool, last sync time, index file location. USE THIS for \"index stats\", \"how many sessions do I have\", \"is the index healthy\", \"where is the index stored\".\n查看索引概况：各工具的会话数与消息数、上次同步时间、索引文件位置。中文触发词：索引概况、索引统计、会话统计、有多少会话、索引在哪、索引多大、索引健康吗。",
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
