# ai-session-hub

[English](README.md) | 简体中文

跨 AI 工具的本地会话检索、上下文回溯与跨工具交接。项目产出一个名为 `ash` 的 Rust 二进制文件，既可以直接作为 CLI 使用，也可以通过 stdio 作为 MCP server 挂载到 AI 客户端。

> 重要：本项目会读取本机 AI 会话日志。会话内容可能包含密码、API Token、内部地址、源代码、SQL、个人信息或其他机密内容。它不是脱敏工具，也不会自动识别或删除敏感信息。请只在你信任的机器和 AI 客户端上使用。

## 功能

- 跨 Claude Code、Codex、Kiro CLI、Kiro IDE、Kimi Code CLI、pi、WorkBuddy、Gemini CLI、OpenCode 检索历史会话。
- 按工程目录、工具、时间、角色过滤，并支持中文短语检索。
- 读取会话正文，默认限制字符预算并剔除工具输出。
- 生成确定性摘要：用户诉求、助手要点、执行命令、文件路径和可能未完成事项。
- 在不同 AI 工具之间生成“摘要 + 最近对话”的 Markdown 交接包。
- 判断会话是否进行中、等待回复、等待工具审批、空闲、已完成或有遗留事项。
- 全程本地运行，不调用模型，不发起网络请求，不修改原始会话文件。

## 支持的会话来源

| 工具 | 默认读取位置 | 解析方式 |
| --- | --- | --- |
| Claude Code | `~/.claude/projects/**/*.jsonl` | JSONL 字节偏移增量 |
| Codex | `~/.codex/sessions/**/*.jsonl`、`~/.codex/archived_sessions/**/*.jsonl` | JSONL 字节偏移增量 |
| Kiro CLI | `~/.kiro/sessions/cli/*.jsonl` 及同名 `.json` | JSONL 字节偏移增量 |
| Kiro IDE | `<Kiro 用户目录>/globalStorage/kiro.kiroagent/workspace-sessions/<工作区 base64>/<sessionId>.json` | JSON 全量解析 |
| Kimi Code CLI | `~/.kimi-code/sessions/wd_*/session_*/agents/*/wire.jsonl` 及同目录 `state.json` | JSONL 字节偏移增量 |
| pi | `~/.pi/agent/sessions/**/<时间>_<uuid>.jsonl` | JSONL 字节偏移增量 |
| WorkBuddy | `~/.workbuddy/projects/**/*.jsonl`、`~/.codebuddy/projects/**/*.jsonl` | JSONL 字节偏移增量 |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*.json` | JSON 全量解析 |
| OpenCode | `~/.local/share/opencode/opencode.db` | SQLite 全量解析 |

Kimi Code CLI（命令 `kimi`）的一个会话是一个目录而不是一个文件：元信息在 `state.json`，正文在 `agents/<agent>/wire.jsonl`。wire 流里同一条消息会以多种事件重复出现，这里只认 `agent.message.appended`——它是唯一同时覆盖 user / assistant / tool 三种角色的事件；`context.append_message` 只有 user，还混入了注入给模型的上下文。助手的 `think` 思考块不入索引。会话 ID 自带 `session_` 前缀（如 `session_e1af002a-…`），恢复命令是 `kimi -r <sessionId>`；`kimi` 不接受位置参数形式的提示词，带首句续聊时会走 `kimi -r <id> -p '<提示词>'`。旧版 `kimi-cli` 的目录结构不同，未做支持，需要时用 `ASH_KIMI_DIR` 指向实际目录。

Kiro IDE 的用户目录在 macOS 是 `~/Library/Application Support/Kiro/User`，Linux 是 `~/.config/Kiro/User`，两处都会尝试。IDE 会话的工具标识是 `kiro-ide`，与 Kiro CLI（`kiro`）相互独立：会话 ID 不通用，且 IDE 会话无法用命令行恢复。

有两个限制来自 Kiro IDE 的存储本身，不是解析问题：agent 模式下助手回复常被写成 `On it.` 这类占位符，真实回复不落盘；消息级没有时间戳，全部回落到会话创建时间。用户提问、标题、工作区路径是完整的。

pi（`pi` CLI agent）的布局与 Claude Code 同构——按工程目录分目录、每会话一个 append-only JSONL——但更好读：首行直接带真实 `cwd`，不用反解编码后的目录名。助手的 `thinking` 块不入索引，工具结果只取 text 块，避免把 base64 图片塞进全文索引。中断时 pi 会写 `stopReason: aborted` 且消息体为空，这里补一条 `Operation aborted` 标记，会话才能被判成「已被打断」。恢复命令是 `pi --session <id>`，支持 UUID 前缀。

WorkBuddy（腾讯 CodeBuddy 的企业换皮）与 Claude Code 布局同构，需要扫两个根目录：桌面应用写 `~/.workbuddy`，独立的 `codebuddy` / `cbc` CLI 写 `~/.codebuddy`。标题取模型生成的 `ai-title` 事件。桌面端的用户提问整条被 `<system-reminder>` 包住，注入的 identity 文件动辄 14KB，因此只索引其中 `<user_query>` 的内容；CLI 端没有这层包裹，原样取用。`reasoning` 与 `file-history-snapshot` 行不入索引。活跃度来自 `<root>/sessions/<pid>.json` 心跳文件，里面同时有 pid 和 sessionId，比命令行匹配更可靠——WorkBuddy 的会话 ID 从不出现在命令行里。恢复命令是 `codebuddy --resume <id>`；注意该 CLI 装在 `WorkBuddy.app` 内部、默认不在 `PATH` 上，想让生成的命令直接可用需要自行软链 `WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy`。

源文件只读。索引和交接包写入独立的数据目录，不会回写上述会话来源。

## 安装

从源码构建。本项目不提供预编译二进制，也没有 npm / Homebrew 包。

前置条件：Rust stable 工具链（用 [rustup](https://rustup.rs) 安装），以及一个 C 编译器——`rusqlite` 会编译内置的 SQLite。支持 macOS 和 Linux；原生 Windows 不支持（会话路径依赖 `HOME`，状态判定依赖 `ps`），请在 WSL 中使用。

```bash
git clone https://github.com/VangelisHaha/ai-session-hub.git
cd ai-session-hub
cargo build --release

# 把 `ash` 放进 PATH
mkdir -p "$HOME/.local/bin"
ln -sf "$PWD/target/release/ash" "$HOME/.local/bin/ash"

# 首次建全量索引，之后查询会自动增量刷新
ash sync --full
```

首次编译要几分钟，因为要编译内置的 SQLite；产物是一个自包含的二进制。

如果本机已有 Rust，又不想把仓库目录长期挂在 `PATH` 上，可以改用 `cargo install --path .`，`ash` 会被安装到 `~/.cargo/bin`。

如果找不到 `ash`，把 `export PATH="$HOME/.local/bin:$PATH"` 加进 shell 配置，或者直接用 `target/release/ash`。

升级时拉代码重编即可，软链会自动指向新二进制：

```bash
git pull && cargo build --release
```

## 快速开始

```bash
ash sync --full              # 建索引
ash search 计划表 --since 7d  # 跨工具搜索
ash active                   # 看现在有哪些会话在跑
```

随后按下文挂载成 MCP server，就可以直接问 AI：

- “我上周在哪聊过计划表草稿？”
- “那个 Codex 会话还在跑，还是在等我确认？”
- “总结一下那个会话，还有什么没做完？”
- “把那个会话交接给 Claude Code，上下文带过去。”


## CLI 用法

```bash
# 刷新索引；查询前也会自动增量刷新
ash sync
ash sync --full
ash sync --tool kiro

# 跨工具搜索。多个关键词按 AND 处理
ash search 计划表 草稿 --since 7d
ash search 额度 --tool kiro --cwd my-project

# 列出会话和实时状态
ash list --cwd my-project --limit 10
ash list --state 进行中
ash status kiro:<session-id>
ash active

# 读取、摘要和交接
ash read kiro:<session-id> --tail 20 --only-text --budget 8000
ash digest kiro:<session-id>
ash handoff kiro:<session-id> --to claude --note "继续补文档"
ash resume codex:<session-id> --prompt "接着上文继续"

# 查看索引概况
ash stats
```

会话 UID 形如 `kiro:<session-id>`。查询时也可以使用完整 UID、工具前缀或唯一的会话 ID 前缀。

## MCP 配置

`ash mcp` 使用 stdio 通信，不监听端口。将它挂载到 AI 客户端后，客户端可以调用以下 10 个工具：

`session_search`、`session_list`、`session_status`、`session_active`、`session_read`、`session_digest`、`session_handoff`、`session_resume_cmd`、`session_sync`、`session_stats`。

下文示例假设 `ash` 已经放进 `PATH`。如果你只是在仓库里编译、没有做软链，把 `ash` 换成 `which ash` 的绝对路径或 `$PWD/target/release/ash`。

### Kiro CLI

```bash
kiro-cli mcp add --name ai-session-hub --command "$HOME/.local/bin/ash" --args mcp
```

### Claude Code

```bash
claude mcp add ai-session-hub -- "$HOME/.local/bin/ash" mcp
```

### Codex

在 `~/.codex/config.toml` 中加入：

```toml
[mcp_servers.ai_session_hub]
type = "stdio"
command = "/absolute/path/to/ash"
args = ["mcp"]
```

### Kimi Code CLI

写入 `~/.kimi-code/mcp.json`（项目级为 `<cwd>/.kimi-code/mcp.json`）：

```json
{
  "mcpServers": {
    "ai-session-hub": {
      "command": "/absolute/path/to/ash",
      "args": ["mcp"]
    }
  }
}
```

MCP 服务在会话启动时加载，改完后新建会话（`/new`）或重启 `kimi` 才生效。

### 其他 MCP 客户端

大多数客户端的 `mcp.json` 都接受这种写法：

```json
{
  "mcpServers": {
    "ai-session-hub": {
      "command": "/absolute/path/to/ash",
      "args": ["mcp"]
    }
  }
}
```

这里建议直接写绝对路径：GUI 客户端通常不继承 shell 的 `PATH`。用 `which ash` 拿到路径即可。

MCP 客户端一旦获得访问权限，就可以读取索引中允许返回的会话内容。因此请把 MCP 客户端视为本地敏感数据的同等信任边界。

## 会话状态

| 状态 | 含义 |
| --- | --- |
| `running` | 最近有活动，或刚发起用户提问/工具调用 |
| `awaiting_approval` | 工具调用长时间没有结果，进程仍在，可能在等待审批 |
| `awaiting_input` | AI 在等待用户回复或确认 |
| `interrupted` | 最后一条消息带有中断标记 |
| `idle` | 进程仍在，但没有待处理的提问或工具调用 |
| `done` | 进程已结束，未发现遗留事项 |
| `unfinished` | 进程已结束，但留下待办、未回答问题或未完成工具调用 |

状态不写入 SQLite，而是根据进程快照、Kiro 锁文件、最后消息和文件修改时间实时计算，所以它是近似判断，不是任务系统的最终状态。

## 数据与隐私

默认数据目录为 `~/.ai-session-hub`：

| 数据 | 内容 |
| --- | --- |
| `index.db` | 会话元数据、工程目录、源文件路径、会话 ID、截断后的消息内容和全文索引 |
| `handoff/*.md` | 摘要、最近对话原文、工程路径、续聊命令以及用户提供的交接备注 |
| WAL 文件 | SQLite 运行期间可能出现的 `index.db-wal`、`index.db-shm` |

请注意：

- 默认不联网，但“本地”不等于“公开安全”；其他本机用户、备份软件、终端录屏或拥有 MCP 权限的客户端仍可能看到数据。
- 不会自动脱敏。消息中的 Token、Cookie、密码、SQL、内部域名、个人信息和源代码可能进入索引或交接包。
- 工具结果默认不加入全文检索，但仍可能以截断形式写入索引；`ASH_INDEX_TOOL_RESULTS=1` 会进一步把工具结果加入全文检索。
- `session_read`、`session_digest` 和 `session_handoff` 都可能返回或导出敏感内容；交接包不要提交 Git、上传网盘或粘贴到公共 Issue。
- 当前版本没有加密、访问控制、自动过期清理或显式的 `0600` 文件权限设置。不要把数据目录放在共享目录、网络盘或公共 CI 工作区。
- 若会话里出现过真实凭据，应立即按凭据管理流程轮换；删除索引不能撤销已经被复制、备份或发送出去的内容。

如需清理本地数据，请先停止正在使用 `ash` 的 MCP 客户端，再删除 `~/.ai-session-hub` 下的 `index.db`、WAL 文件和 `handoff` 目录。源会话文件不会被本项目删除。

## 环境变量

用于测试、隔离索引或自定义 AI 工具安装位置：

| 变量 | 作用 |
| --- | --- |
| `AI_SESSION_HUB_HOME` | 覆盖索引和交接包的数据目录 |
| `ASH_CLAUDE_DIR` | 覆盖 Claude 会话目录 |
| `ASH_CODEX_DIR` | 覆盖 Codex 会话目录；设置后只使用该目录 |
| `ASH_KIRO_DIR` | 覆盖 Kiro CLI 会话目录 |
| `ASH_KIRO_IDE_DIR` | 覆盖 Kiro IDE 的 `workspace-sessions` 目录 |
| `ASH_KIMI_DIR` | 覆盖 Kimi Code CLI 的 `sessions` 目录 |
| `ASH_PI_DIR` | 覆盖 pi 的 `sessions` 目录 |
| `ASH_WORKBUDDY_DIR` | 覆盖 WorkBuddy 的 `projects` 目录；设置后只使用该目录 |
| `ASH_GEMINI_DIR` | 覆盖 Gemini 临时目录 |
| `ASH_OPENCODE_DB` | 覆盖 OpenCode SQLite 数据库路径 |
| `ASH_INDEX_TOOL_RESULTS` | 设为 `1` 后将工具结果加入全文检索 |

建议测试时使用独立的数据目录和会话目录，避免读到真实会话、也避免污染正式索引。本仓库不附带任何会话样例，下面的会话目录需要你自己准备：

```bash
AI_SESSION_HUB_HOME="$(mktemp -d)" \
ASH_CLAUDE_DIR="$(mktemp -d)" \
ash sync --full
```

## 设计说明

- 中文检索使用 SQLite FTS5 `trigram`；少于 3 个字符的查询退化为 `LIKE`。
- Claude、Codex、Kiro CLI、Kimi Code CLI、pi、WorkBuddy 使用字节偏移增量读取，只消费以换行结尾的完整 JSONL 行。
- Kiro IDE、Gemini 和 OpenCode 使用全量解析，因为其源文件或数据库可能整体更新。
- 会话内容统一裁剪：普通文本最多 24,000 字符，工具调用最多 600 字符，工具结果最多 1,200 字符。
- 摘要是规则化抽取，不调用模型；摘要结果仍应由使用方自行核实，不能当作事实审计结论。
- 跨工具交接不伪造目标工具的原生会话格式，而是生成显式 Markdown 交接包并返回启动命令。

## 开源与安全边界

本仓库只包含源码、锁文件、文档和测试，不应包含任何真实会话、索引库、交接包、凭据或业务数据。提交前建议至少执行：

```bash
git status --short
git ls-files | sort
git grep -n -I -i -E \
  'api[_-]?key|secret|password|passwd|token|private[_-]?key|authorization|bearer|cookie|jdbc:|mongodb(\+srv)?://'
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

如果本机装了 [ripgrep](https://github.com/BurntSushi/ripgrep)，等价的扫描命令是：

```bash
rg -n -i --hidden \
  --glob '!target/**' --glob '!.git/**' \
  'api[_-]?key|secret|password|passwd|token|private[_-]?key|authorization|bearer|cookie|jdbc:|mongodb(\+srv)?://' .
```

如果凭据曾经进入 Git 历史，仅删除工作树文件是不够的：应先轮换凭据，再清理历史，并在推送公开仓库前重新检查所有 refs、标签和附件。

## 开发

```bash
cargo test
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
```

## 商标与免责声明

本项目是独立的第三方工具，与 Anthropic、OpenAI、Amazon、Google、Moonshot AI、腾讯、OpenCode 及其他任何厂商均无隶属、赞助或背书关系。Claude、Claude Code、Codex、Kiro、Kimi、Kimi Code、pi、WorkBuddy、CodeBuddy、Gemini、OpenCode 等名称与标识归各自权利人所有，本文中仅用于说明本工具所兼容的会话来源。

本项目只读取运行它的机器上本地已有的会话文件，不修改、不上传、不再分发任何第三方软件的代码或数据。使用者需自行确认对所读取数据拥有相应权限，并遵守所用 AI 工具的服务条款以及所在组织的数据管理规定。

## 许可证

本项目采用 [MIT License](LICENSE)。第三方 Rust 依赖的许可证以各依赖自身发布的许可证文本为准。

`rusqlite` 通过 `bundled` 特性静态链接 SQLite；SQLite 本身属于公有领域（public domain）。若分发编译后的二进制，建议随包附带一份第三方许可证清单，例如用 [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) 或 [`cargo-license`](https://github.com/onur/cargo-license) 生成。
