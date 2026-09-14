# ai-session-hub

English | [简体中文](README.zh-CN.md)

Local session search, context recall, and cross-tool handoff across AI coding tools. The project builds a single Rust binary named `ash`, which works both as a CLI and as an MCP server over stdio.

> Important: this project reads AI session logs from your machine. Session content may contain passwords, API tokens, internal addresses, source code, SQL, personal data, or other confidential material. It is not a redaction tool and it does not detect or remove sensitive information. Only use it on machines and AI clients you trust.

## Features

- Search history across Claude Code, Codex, Kiro CLI, Gemini CLI, and OpenCode.
- Filter by project directory, tool, time range, and role, with Chinese phrase search supported.
- Read session transcripts, with a character budget and tool output stripped by default.
- Produce deterministic digests: user intent, assistant takeaways, commands executed, file paths, and likely unfinished items.
- Generate a "digest + recent transcript" Markdown handoff package to move work between AI tools.
- Tell whether a session is running, awaiting input, awaiting tool approval, idle, done, or has leftovers.
- Runs entirely locally: no model calls, no network requests, no writes to the original session files.

## Supported session sources

| Tool | Default location | Parsing strategy |
| --- | --- | --- |
| Claude Code | `~/.claude/projects/**/*.jsonl` | Incremental JSONL by byte offset |
| Codex | `~/.codex/sessions/**/*.jsonl`, `~/.codex/archived_sessions/**/*.jsonl` | Incremental JSONL by byte offset |
| Kiro CLI | `~/.kiro/sessions/cli/*.jsonl` plus same-named `.json` | Incremental JSONL by byte offset |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*.json` | Full JSON parse |
| OpenCode | `~/.local/share/opencode/opencode.db` | Full SQLite parse |

Source files are read-only. The index and handoff packages are written to a separate data directory and never back into the sources above.

## Installation

Build from source. There are no prebuilt binaries and no npm/Homebrew package.

Requirements: the Rust stable toolchain (install via [rustup](https://rustup.rs)), plus a C compiler, because `rusqlite` compiles a bundled SQLite. macOS and Linux are supported; native Windows is not (session paths rely on `HOME` and liveness relies on `ps`), so use WSL there.

```bash
git clone https://github.com/VangelisHaha/ai-session-hub.git
cd ai-session-hub
cargo build --release

# Put `ash` on your PATH
mkdir -p "$HOME/.local/bin"
ln -sf "$PWD/target/release/ash" "$HOME/.local/bin/ash"

# Build the index once; later queries refresh incrementally
ash sync --full
```

The first build takes a few minutes because of the bundled SQLite; the result is a single self-contained binary.

If you already have Rust and would rather not keep the checkout on your `PATH`, `cargo install --path .` puts `ash` in `~/.cargo/bin` instead.

If `ash` is not found, either add `export PATH="$HOME/.local/bin:$PATH"` to your shell rc, or call `target/release/ash` directly.

To upgrade, pull and rebuild; the symlink then points at the new binary:

```bash
git pull && cargo build --release
```

## Quick start

```bash
ash sync --full                    # build the index
ash search "some topic" --since 7d # search across all tools
ash active                         # what is running right now
```

Then mount it as an MCP server (see below) and ask your AI things like:

- "Where did I discuss the WBS draft last week?"
- "Is that Codex session still running, or is it waiting on me?"
- "Summarize that session and tell me what's unfinished."
- "Hand that session off to Claude Code and keep the context."


## CLI usage

```bash
# Refresh the index; queries also trigger an incremental refresh automatically
ash sync
ash sync --full
ash sync --tool kiro

# Cross-tool search. Multiple terms are AND-ed
ash search 计划表 草稿 --since 7d
ash search quota --tool kiro --cwd my-project

# List sessions and live state
ash list --cwd my-project --limit 10
ash list --state running
ash status kiro:<session-id>
ash active

# Read, digest, and hand off
ash read kiro:<session-id> --tail 20 --only-text --budget 8000
ash digest kiro:<session-id>
ash handoff kiro:<session-id> --to claude --note "finish the docs"
ash resume codex:<session-id> --prompt "pick up where we left off"

# Index overview
ash stats
```

A session UID looks like `kiro:<session-id>`. Queries also accept a full UID, a tool prefix, or a unique session ID prefix.

## MCP configuration

`ash mcp` communicates over stdio and does not listen on a port. Once mounted in an AI client, the client can call these 10 tools:

`session_search`, `session_list`, `session_status`, `session_active`, `session_read`, `session_digest`, `session_handoff`, `session_resume_cmd`, `session_sync`, `session_stats`.

A globally installed `ash` is used in the examples below. If you built into the checkout without a symlink, replace `ash` with the absolute path from `which ash` or `$PWD/target/release/ash`.

### Kiro CLI

```bash
kiro-cli mcp add --name ai-session-hub --command "$HOME/.local/bin/ash" --args mcp
```

### Claude Code

```bash
claude mcp add ai-session-hub -- "$HOME/.local/bin/ash" mcp
```

### Codex

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.ai_session_hub]
type = "stdio"
command = "/absolute/path/to/ash"
args = ["mcp"]
```

### Any other MCP client

Most clients accept this shape in their `mcp.json`:

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

Prefer an absolute path here: GUI clients often do not inherit your shell `PATH`. Run `which ash` to get it.

Any MCP client with access can read whatever session content the index is allowed to return. Treat MCP clients as the same trust boundary as your local sensitive data.

## Session states

| State | Meaning |
| --- | --- |
| `running` | Recent activity, or a user question / tool call was just issued |
| `awaiting_approval` | A tool call has had no result for a while and the process is alive, so it may be waiting for approval |
| `awaiting_input` | The AI is waiting for a user reply or confirmation |
| `interrupted` | The last message carries an interruption marker |
| `idle` | The process is alive, but there is no pending question or tool call |
| `done` | The process has exited and no leftovers were found |
| `unfinished` | The process has exited but left TODOs, unanswered questions, or incomplete tool calls |

State is not persisted in SQLite. It is computed on the fly from a process snapshot, Kiro lock files, the last message, and file modification times, so it is an approximation rather than the authoritative status from a task system.

## Data and privacy

The default data directory is `~/.ai-session-hub`:

| Data | Content |
| --- | --- |
| `index.db` | Session metadata, project directories, source file paths, session IDs, truncated message content, and the full-text index |
| `handoff/*.md` | Digests, recent raw transcript, project paths, resume commands, and any handoff note you provide |
| WAL files | `index.db-wal` and `index.db-shm` may appear while SQLite is running |

Please note:

- There is no network access by default, but "local" is not the same as "safe to publish": other users on the machine, backup software, terminal recordings, or clients with MCP access can still see the data.
- Nothing is redacted automatically. Tokens, cookies, passwords, SQL, internal domains, personal data, and source code in messages can end up in the index or a handoff package.
- Tool results are excluded from full-text search by default but may still be stored in truncated form; `ASH_INDEX_TOOL_RESULTS=1` additionally adds tool results to full-text search.
- `session_read`, `session_digest`, and `session_handoff` can all return or export sensitive content. Do not commit handoff packages to Git, upload them to cloud storage, or paste them into public issues.
- This version has no encryption, no access control, no automatic expiry cleanup, and no explicit `0600` file permissions. Do not place the data directory on a shared folder, network drive, or public CI workspace.
- If real credentials ever appeared in a session, rotate them through your credential management process immediately. Deleting the index cannot undo content that has already been copied, backed up, or sent elsewhere.

To clean up local data, first stop any MCP client using `ash`, then delete `index.db`, the WAL files, and the `handoff` directory under `~/.ai-session-hub`. This project never deletes the original session files.

## Environment variables

For testing, isolating the index, or pointing at custom AI tool locations:

| Variable | Effect |
| --- | --- |
| `AI_SESSION_HUB_HOME` | Override the data directory for the index and handoff packages |
| `ASH_CLAUDE_DIR` | Override the Claude session directory |
| `ASH_CODEX_DIR` | Override the Codex session directory; when set, only that directory is used |
| `ASH_KIRO_DIR` | Override the Kiro session directory |
| `ASH_GEMINI_DIR` | Override the Gemini temp directory |
| `ASH_OPENCODE_DB` | Override the OpenCode SQLite database path |
| `ASH_INDEX_TOOL_RESULTS` | Set to `1` to add tool results to full-text search |

When testing, use a dedicated data directory and session directories to avoid reading real sessions and to avoid polluting your primary index. This repository ships no session samples, so you need to prepare the session directories below yourself:

```bash
AI_SESSION_HUB_HOME="$(mktemp -d)" \
ASH_CLAUDE_DIR="$(mktemp -d)" \
ash sync --full
```

## Design notes

- Chinese search uses SQLite FTS5 `trigram`; queries shorter than 3 characters fall back to `LIKE`.
- Claude, Codex, and Kiro are read incrementally by byte offset, consuming only complete newline-terminated JSONL lines.
- Gemini and OpenCode are parsed in full, because their source files or database can be rewritten wholesale.
- Session content is truncated uniformly: up to 24,000 characters for plain text, 600 for tool calls, and 1,200 for tool results.
- Digests are rule-based extraction with no model calls. Consumers should still verify them; they are not an authoritative audit result.
- Cross-tool handoff does not fabricate the target tool's native session format. It produces an explicit Markdown handoff package and returns the launch command.

## Open source and security boundary

This repository should contain only source code, lock files, documentation, and tests — never real sessions, index databases, handoff packages, credentials, or business data. Before committing, run at least:

```bash
git status --short
git ls-files | sort
git grep -n -I -i -E \
  'api[_-]?key|secret|password|passwd|token|private[_-]?key|authorization|bearer|cookie|jdbc:|mongodb(\+srv)?://'
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

If you prefer [ripgrep](https://github.com/BurntSushi/ripgrep), the equivalent scan is:

```bash
rg -n -i --hidden \
  --glob '!target/**' --glob '!.git/**' \
  'api[_-]?key|secret|password|passwd|token|private[_-]?key|authorization|bearer|cookie|jdbc:|mongodb(\+srv)?://' .
```

If credentials once entered Git history, deleting the working-tree file is not enough: rotate the credentials first, then rewrite history, and re-check all refs, tags, and attachments before pushing to a public repository.

## Development

```bash
cargo test
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
```

## Trademarks and disclaimer

This project is an independent third-party tool with no affiliation, sponsorship, or endorsement from Anthropic, OpenAI, Amazon, Google, OpenCode, or any other vendor. Claude, Claude Code, Codex, Kiro, Gemini, OpenCode, and other names and marks belong to their respective owners and are used here only to describe the session sources this tool is compatible with.

This project only reads session files that already exist locally on the machine running it. It does not modify, upload, or redistribute any third-party software's code or data. Users are responsible for confirming they have the rights to the data being read, and for complying with the terms of service of the AI tools involved as well as their organization's data governance rules.

## License

Released under the [MIT License](LICENSE). Third-party Rust dependencies are governed by the license texts published with each dependency.

`rusqlite` statically links SQLite through its `bundled` feature; SQLite itself is in the public domain. If you distribute compiled binaries, consider shipping a third-party license inventory generated with a tool such as [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) or [`cargo-license`](https://github.com/onur/cargo-license).
