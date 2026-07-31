# ai-session-hub

跨 AI 工具的会话检索、上下文回溯与**跨工具交接**。一个本地二进制 `ash`，既是 CLI，也是 MCP server。

支持的会话来源（只读，绝不改动源文件）：

| 工具 | 会话位置 | 增量方式 |
| --- | --- | --- |
| Claude Code | `~/.claude/projects/**/*.jsonl`（含 `subagents/`） | 字节偏移增量 |
| Codex | `~/.codex/sessions/**/*.jsonl` + `archived_sessions` | 字节偏移增量 |
| Kiro CLI | `~/.kiro/sessions/cli/*.jsonl` + 同名 `.json` | 字节偏移增量 |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*.json` | 全量重解析 |
| OpenCode | `~/.local/share/opencode/opencode.db` | 全量重解析 |

## 解决什么问题

三个工具各写各的会话，互相看不到。于是：

- **回溯**：想不起某个方案是在 Codex 还是 Kiro 里聊的 → `session_search` 一次搜全部。
- **读上下文**：把某个历史会话的正文按字符预算取回来，自动剥掉 `SYSTEM_CONTEXT` 之类的注入包裹。
- **换工具继续聊**：跨工具无法迁移原生会话（历史格式、tool_use ID 体系、模型都不通），所以走
  「摘要 + 最近原文」的交接包，再给出目标工具的启动命令，第一句话就带上下文。同工具则直接给
  原生 `--resume` 命令（无损）。
- **知道谁在忙**：某个会话是进行中、在等我确认、还是已经结束？哪些会话我漏回了？
  `session_status` / `session_active` 实时判定，避免去打扰正在干活的终端。

## 安装

```bash
cargo build --release
ln -sf "$PWD/target/release/ash" ~/.local/bin/ash   # 或任意 PATH 目录
ash sync --full                                     # 首次建索引
```

## CLI

```bash
ash sync [--full] [--tool kiro]          # 刷新索引（查询时会自动增量刷新）
ash search 计划表 草稿 --since 7d         # 跨工具检索，多个词是 AND
ash search 额度 --tool kiro --cwd nikou   # 按工具 / 工程目录过滤
ash list --cwd nikou-cc-switch --limit 5  # 列会话（含 resume 命令）
ash read kiro:b2daab07 --tail 20 --only-text --budget 8000
ash digest kiro:b2daab07                  # 结构化摘要
ash handoff kiro:b2daab07 --to claude --note "继续补文档"
ash resume codex:019fb745 --prompt "接着上文继续"
ash status kiro:b2daab07                  # 这个会话现在什么状态
ash active                                # 谁在忙 / 谁在等我回话
ash list --state 进行中                    # 按状态过滤（中英文都认）
ash stats
```

`uid` 形如 `kiro:<session_id>`，也接受 `session_id` 前缀（`kiro:b2daab07` 即可）。

## 作为 MCP 挂到三家

```bash
# Kiro
kiro-cli mcp add --name ai-session-hub --command ~/.local/bin/ash --args mcp

# Claude Code
claude mcp add ai-session-hub -- ~/.local/bin/ash mcp

# Codex：写入 ~/.codex/config.toml
[mcp_servers.ai_session_hub]
type = "stdio"
command = "/Users/<you>/.local/bin/ash"
args = ["mcp"]
```

暴露 10 个工具：`session_search`、`session_list`、`session_status`、`session_active`、
`session_read`、`session_digest`、`session_handoff`、`session_resume_cmd`、`session_sync`、
`session_stats`。

## 会话状态

| state | 含义 | 判定依据 |
| --- | --- | --- |
| `running` 进行中 | AI 正在跑 | 90 秒内有活动，或提问/工具调用刚发起 |
| `awaiting_approval` 待我确认工具执行 | 卡在工具审批 | 工具调用超过 90 秒没结果且进程还在 |
| `awaiting_input` 待我回复 | AI 在等我回话 | 最后一句是提问/请确认，且会话仍开着 |
| `interrupted` 已被打断 | 我打断过 | 最后一条带中断标记 |
| `idle` 空闲可续聊 | 进程还开着但没在等我 | 有持有进程、最后是 AI 交付 |
| `done` 已完成 | 已收尾 | 进程已退出且无遗留信号 |
| `unfinished` 已结束但有遗留 | 没聊完 | 进程已退出，但有待办信号／提问没人回／工具没走完 |

状态**不入库**（易失，落库必然读到陈旧值），每次实时算：一次 `ps` 快照 + 读 Kiro 锁目录 + 单会话
最后一条消息。判定信号按可靠性分三层：

1. **Kiro 的 `.lock`**（含 `pid` / `started_at`）→ 能精确知道会话被哪个进程持有；
2. **进程命令行**（`--resume-id` / `resume <id>`）→ 显式恢复的会话能对上号；
3. **对话形态 + 活动时间** → Codex / Claude 写完日志就关 fd（`lsof` 抓不到持有者），只能推断。

活动时间取「最后消息时间」与「会话文件 mtime」的较新者：Kiro 的助手消息和工具结果都不带
timestamp，只看消息时间会把正在连续跑工具的会话误判成卡住。

## 设计要点

- **中文检索用 FTS5 trigram**：默认的 unicode61 分词器不切中文，中文短语根本搜不到；trigram
  按三字滑窗建索引，中英文子串都能命中。查询词短于 3 字时自动退化为 `LIKE`。
- **只索引有检索价值的部分**：工具结果（编译输出、SQL 结果集）占源数据九成以上体积，默认不进
  FTS，内容也按 1200 字符截断。需要时 `ASH_INDEX_TOOL_RESULTS=1`。
- **偏移增量而非全量重读**：本机 Codex 会话目录已有 1.7G，全量重读一次要 35 秒；按字节偏移只
  读新增部分，日常同步是毫秒级。只消费以换行结尾的完整行，避免把正在写入的半行当成 JSON。
- **同一会话可以有多个数据源**：Claude 的子代理文件与主会话共用 `sessionId`，所以重解析时只
  清理「本数据源」的消息，否则主会话内容会被子代理文件抹掉。
- **时间戳前向填充**：Kiro 只在用户提问那一行写 timestamp，助手消息与工具结果都是 null，不补
  齐会导致九成消息没有时间，时间过滤与排序全部失真。
- **摘要不调模型**：只做确定性抽取（提问清单、命令、文件、待办信号），把「理解」留给调用方的
  AI。没有幻觉风险，也不产生额外 token 成本。

## 数据与隐私

- 索引：`~/.ai-session-hub/index.db`（SQLite，可随时删掉重建）
- 交接包：`~/.ai-session-hub/handoff/*.md`
- 全程本地，不发起任何网络请求；会话原文只读不写。
- 会话里可能包含 token、内部域名、SQL，交接包属于敏感文件，别随手外发。

环境变量（测试或非默认安装位置用）：`AI_SESSION_HUB_HOME`、`ASH_CLAUDE_DIR`、`ASH_CODEX_DIR`、
`ASH_KIRO_DIR`、`ASH_GEMINI_DIR`、`ASH_OPENCODE_DB`、`ASH_INDEX_TOOL_RESULTS`。

## 开发

```bash
cargo test          # 31 个单测
cargo clippy
cargo fmt
```
