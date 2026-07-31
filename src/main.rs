//! ai-session-hub：跨 AI 工具的会话检索 / 回溯 / 交接。
//!
//! 两种用法：
//! - `ash mcp`：作为 MCP server 挂到 Claude Code / Codex / Kiro，让 AI 自己检索历史；
//! - `ash <子命令>`：命令行直接用（sync / search / list / read / digest / handoff / resume / stats）。

mod adapters;
mod digest;
mod handoff;
mod index;
mod mcp;
mod model;
mod ops;
mod paths;
mod tools;

use anyhow::{anyhow, Result};
use index::Index;
use std::collections::HashMap;

const USAGE: &str = r#"ai-session-hub (ash) — 跨 AI 工具会话检索与交接

用法：
  ash mcp                                以 MCP server 模式运行（stdio）
  ash sync [--full] [--tool <t>]         刷新索引
  ash search <关键词...> [选项]           跨工具检索
  ash list [选项]                        列出会话
  ash read <uid> [选项]                  读会话正文
  ash digest <uid>                       结构化摘要
  ash handoff <uid> --to <tool> [--note <文本>] [--tail N]
                                         生成跨工具交接包
  ash resume <uid> [--prompt <文本>]     取原生恢复命令
  ash stats                              索引概况

通用选项：
  --tool <claude|codex|kiro|gemini|opencode>
  --cwd <路径片段>      --since <7d|36h|2026-07-30>   --until <同上>
  --role <user|assistant|tool>            --limit <N>
  --tail <N>            --from-seq <N>    --budget <字符数>
  --with-tool-results   把工具输出也算进来（默认剔除）
  --only-text           只要对话正文，丢掉工具调用与输出
  --json                以 JSON 输出（read / digest 默认输出 Markdown）
"#;

fn main() {
    if let Err(error) = run() {
        eprintln!("错误：{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{USAGE}");
        return Ok(());
    }

    let command = args[0].clone();
    let (positional, flags) = parse_args(&args[1..]);

    match command.as_str() {
        "mcp" => mcp::serve(),
        "sync" => {
            let mut index = Index::open()?;
            let report = ops::sync(
                &mut index,
                &ops::SyncArgs {
                    full: flags.contains_key("full"),
                    tool: flags.get("tool").cloned().flatten(),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "search" => {
            if positional.is_empty() {
                return Err(anyhow!("search 需要关键词"));
            }
            let mut index = Index::open()?;
            let result = ops::search(
                &mut index,
                &ops::SearchArgs {
                    query: positional.join(" "),
                    tool: flags.get("tool").cloned().flatten(),
                    cwd: flags.get("cwd").cloned().flatten(),
                    since: flags.get("since").cloned().flatten(),
                    until: flags.get("until").cloned().flatten(),
                    role: flags.get("role").cloned().flatten(),
                    include_tool_results: flags.contains_key("with-tool-results"),
                    limit: number(&flags, "limit"),
                    max_per_session: number(&flags, "max-per-session"),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        "list" => {
            let mut index = Index::open()?;
            let result = ops::list(
                &mut index,
                &ops::ListArgs {
                    tool: flags.get("tool").cloned().flatten(),
                    cwd: flags.get("cwd").cloned().flatten(),
                    since: flags.get("since").cloned().flatten(),
                    title: flags.get("title").cloned().flatten(),
                    limit: number(&flags, "limit"),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        "read" => {
            let uid = positional
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("read 需要会话 uid"))?;
            let mut index = Index::open()?;
            let (markdown, meta) = ops::read(
                &mut index,
                &ops::ReadArgs {
                    uid,
                    tail: number(&flags, "tail"),
                    from_seq: number(&flags, "from-seq").map(|value| value as i64),
                    include_tool_results: flags.contains_key("with-tool-results"),
                    only_text: flags.contains_key("only-text"),
                    budget_chars: number(&flags, "budget"),
                },
            )?;
            if flags.contains_key("json") {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                println!("{markdown}");
            }
            Ok(())
        }
        "digest" => {
            let uid = positional
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("digest 需要会话 uid"))?;
            let mut index = Index::open()?;
            let (markdown, value) = ops::digest(&mut index, &ops::UidArgs { uid })?;
            if flags.contains_key("json") {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{markdown}");
            }
            Ok(())
        }
        "handoff" => {
            let uid = positional
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("handoff 需要会话 uid"))?;
            let to_tool = flags
                .get("to")
                .cloned()
                .flatten()
                .ok_or_else(|| anyhow!("handoff 需要 --to <tool>"))?;
            let mut index = Index::open()?;
            let result = ops::handoff(
                &mut index,
                &ops::HandoffArgs {
                    uid,
                    to_tool,
                    note: flags.get("note").cloned().flatten(),
                    tail: number(&flags, "tail"),
                    include_tool_results: flags.contains_key("with-tool-results"),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        "resume" => {
            let uid = positional
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("resume 需要会话 uid"))?;
            let mut index = Index::open()?;
            let result = ops::resume_cmd(
                &mut index,
                &ops::ResumeArgs {
                    uid,
                    prompt: flags.get("prompt").cloned().flatten(),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        "stats" => {
            let index = Index::open()?;
            println!("{}", serde_json::to_string_pretty(&ops::stats(&index)?)?);
            Ok(())
        }
        other => Err(anyhow!("未知子命令 {other}\n\n{USAGE}")),
    }
}

type Flags = HashMap<String, Option<String>>;

/// 极简参数解析：`--key value` / `--key`（布尔）/ 其余为位置参数
fn parse_args(args: &[String]) -> (Vec<String>, Flags) {
    let mut positional = Vec::new();
    let mut flags: Flags = HashMap::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if let Some(key) = arg.strip_prefix("--") {
            let next = args.get(idx + 1);
            match next {
                Some(value) if !value.starts_with("--") => {
                    flags.insert(key.to_string(), Some(value.clone()));
                    idx += 2;
                }
                _ => {
                    flags.insert(key.to_string(), None);
                    idx += 1;
                }
            }
        } else {
            positional.push(arg.clone());
            idx += 1;
        }
    }
    (positional, flags)
}

fn number(flags: &Flags, key: &str) -> Option<usize> {
    flags.get(key)?.as_ref()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_positionals_are_separated() {
        let args: Vec<String> = ["kiro:abc", "--to", "claude", "--json", "--tail", "10"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let (positional, flags) = parse_args(&args);
        assert_eq!(positional, vec!["kiro:abc".to_string()]);
        assert_eq!(
            flags.get("to").cloned().flatten().as_deref(),
            Some("claude")
        );
        assert!(flags.contains_key("json"));
        assert_eq!(number(&flags, "tail"), Some(10));
    }
}
