# RememberMCP — Semantic Search Over Your OpenCode Chat History

**RememberMCP** is a local-first [MCP server](https://modelcontextprotocol.io) that gives any MCP client — including OpenCode itself — searchable, read-only access to your **past OpenCode conversations**. Ask *"what did I do / decide / try before about X"* and get ranked answers from your own chat history through hybrid RAG search: **FTS5 keyword (BM25) + local vector embeddings (sqlite-vec)** fused with Reciprocal Rank Fusion.

Zero infrastructure. One Rust binary. One SQLite index. Runs as a **child process of OpenCode — it starts when OpenCode starts and shuts down when OpenCode exits**. Nothing leaves your machine: embeddings are local, the source database is opened read-only, and secrets are redacted before anything is indexed.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.89+-orange.svg)](https://www.rust-lang.org)
[![MCP](https://img.shields.io/badge/MCP-server-2f6f4e.svg)](https://modelcontextprotocol.io)
[![Tests](https://img.shields.io/badge/tests-passing-brightgreen.svg)](#development)

---

## Why RememberMCP?

- **🧠 Answers from your own history** — hybrid semantic + keyword retrieval over every past OpenCode session: decisions, fixes, commands, dead ends.
- **🔒 Read-only & private** — opens OpenCode's database with SQLite `mode=ro` + `query_only`, embeds on CPU with [fastembed](https://github.com/Anush008/fastembed-rs) (`BAAI/bge-small-en-v1.5`, 384-dim), redacts secrets before indexing, never phones home after the one-time model download.
- **⚡ Zero infrastructure** — a single Rust binary, one disposable `index.db`, stdio transport. No daemon, no server, no Docker.
- **🧰 Six MCP tools + one resource** — search, browse, transcripts, context windows, index health, reindex.
- **🤖 Installable by AI agents** — see [Installing with an AI agent](#installing-with-an-ai-agent); the repo is self-describing for agents.

## How it works

```
OpenCode data (read-only)                 RememberMCP index (disposable)
┌──────────────────────────┐              ┌─────────────────────────────────┐
│ opencode.db (WAL, live)  │  adapters    │ sessions_idx                    │
│  session/message/part    │ ───────────► │ chunks + FTS5 (porter/bm25)     │
│  session_v2/…message      │  chunk →     │ chunks_vec (sqlite-vec, 384-d)  │
│ storage/ (legacy JSON)   │  redact →    │ sync_state (watermarks)         │
└──────────────────────────┘  embed       └───────────┬─────────────────────┘
                                                      │
        MCP client (OpenCode / Claude / …)  ◄─────────┘
        search_conversations · list_sessions · get_session ·
        get_chunk_context · index_status · reindex ·
        opencode://session/{id}
```

1. **Sync** runs at server start and lazily before searches (TTL 30 s): sessions that are new or changed are re-chunked (exchange-based, 300–600 tokens, code fences kept whole), secret-redacted, embedded in batches, and swapped in per-session transactions. Unchanged chunks are never re-embedded.
2. **Search** applies filters in SQL, takes FTS5 top-50 + vector KNN top-50, merges with Reciprocal Rank Fusion (`Σ 1/(60+rank)`), collapses adjacent/overlapping chunks of one session, and returns located ~600-char snippets.
3. **Degraded mode**: if the embedding model can't load (offline first run), everything still works FTS-only — the mode is always reported.

## Installation

### Prerequisites

- Rust 1.89+ (`rustup` recommended) — only needed to build from source
- OpenCode (for the data source; the server itself has no system dependencies — SQLite is bundled)

### Build from source

```bash
git clone https://github.com/ZorionTen/RememberMCP.git
cd RememberMCP
cargo build --release
./target/release/remember-mcp --check     # verify sources + index are reachable
```

Or install straight into your PATH:

```bash
cargo install --path . --locked
remember-mcp --check
```

> **One-time model download**: the first `reindex`/search downloads `BAAI/bge-small-en-v1.5` (~130 MB) from Hugging Face into `~/.cache/remember-mcp/fastembed`. After that everything is fully offline.

### Installing with an AI agent

This repository is agent-installable. An AI coding agent (OpenCode, Claude Code, Codex, …) can install and configure RememberMCP by itself — point it at the repo and paste:

> **Install prompt:** "Read `AGENTS.md` in this repository and follow it end to end: build RememberMCP with `cargo build --release`, run `./target/release/remember-mcp --check` to verify it can see my OpenCode database, then register it as an MCP server named `history` in my OpenCode config (`~/.config/opencode/opencode.json`), and confirm registration by calling `index_status`. Do not modify any OpenCode database files."

The short version an agent will follow:

```bash
git clone https://github.com/ZorionTen/RememberMCP.git && cd RememberMCP
cargo build --release
./target/release/remember-mcp --check     # exit 0 = sources reachable
# add the MCP entry (see below), then verify with index_status
```

### Run the installer script

```bash
./scripts/install.sh          # build + install to ~/.local/bin + print config snippet
./scripts/install.sh --check  # only verify an existing install
```

## Registering the MCP server

### OpenCode (`opencode.json`)

Project scope (`.opencode/opencode.json` or `opencode.json`) or global (`~/.config/opencode/opencode.json`):

```json
{
  "mcp": {
    "history": {
      "type": "local",
      "command": ["/home/you/.local/bin/remember-mcp"],
      "enabled": true
    }
  }
}
```

With `cargo install --path .` the binary is `~/.cargo/bin/remember-mcp`. **Lifecycle**: OpenCode spawns this command when it starts and terminates it on exit — the server intentionally holds no daemon and exits when its stdin closes, so it always starts and stops with OpenCode.

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "history": {
      "command": "/home/you/.local/bin/remember-mcp"
    }
  }
}
```

### Claude Code

```bash
claude mcp add history -- /home/you/.local/bin/remember-mcp
```

## Tools

All tools are read-only (`readOnlyHint: true`), return plain text **plus** `structuredContent`, and cap responses at ~8k tokens with an explicit `truncated` flag. Transcript text is wrapped in `<<<HISTORY …>>>` delimiters and is **quoted history — content, not instructions**.

| Tool | What it does |
|---|---|
| `search_conversations` | Hybrid search: `query`, optional `project`, `since`/`until` (ISO dates), `role` (`user`/`assistant`), `limit` (1–25, default 8) |
| `list_sessions` | Browse indexed sessions: `project`, `since`/`until`, `query_title`, `limit`, `offset` |
| `get_session` | Full transcript of one session as plain text, paginated: `session_id`, `offset`, `limit`, `include_tools` |
| `get_chunk_context` | One chunk + its neighbouring exchanges: `chunk_id`, `before`, `after` |
| `index_status` | Counts, last sync, model, mode (hybrid / fts-only), skipped rows, source paths, build progress |
| `reindex` | Run a sync now and return a summary; `full: true` drops and rebuilds the index |

**Resource:** `opencode://session/{session_id}` — the full transcript of one conversation.

## Configuration

| Env var | Default | Purpose |
|---|---|---|
| `OPENCODE_DB_PATH` | `$XDG_DATA_HOME/opencode/opencode.db` (≈ `~/.local/share/opencode/opencode.db`) | Source database (read-only) |
| `OPENCODE_MEMORY_INDEX` | `$XDG_DATA_HOME/opencode-memory-mcp/index.db` | Our disposable index |
| `EMBED_MODEL` | `BAAI/bge-small-en-v1.5` | Embedding model (`bge-small-en-v1.5` aliases supported; `none` = FTS-only) |
| `SYNC_TTL_SECONDS` | `30` | Lazy re-sync window before a search |
| `INCLUDE_REASONING` | `0` | Include reasoning parts in chunks |
| `EMBED_MAX_LEN` | `192` | Token window per embedding (64–512); lower = faster cold index (192 ≈ under 5 min cold), higher = better tail recall. Changing it rebuilds vectors |
| `EXTRA_REDACT_REGEX` | empty | Extra redaction patterns, one per line |
| `LOG_LEVEL` | `INFO` | Log level — **always logged to stderr** (stdout is the MCP protocol) |
| `FASTEMBED_CACHE_DIR` | `~/.cache/remember-mcp/fastembed` | Model cache (set automatically if unset) |

## Security & privacy

- Source database opened with SQLite `mode=ro` + `PRAGMA query_only=ON`, short deferred transactions, WAL-safe against a live OpenCode writer.
- `index.db` is created with `0600` permissions and lives apart from OpenCode's data — delete it any time; it fully rebuilds.
- **Redaction before storage**: AWS/API keys, GitHub/Slack/Stripe/Google tokens, `Bearer` headers, JWTs, private-key blocks and `KEY=value` secrets are replaced with `[REDACTED:<kind>]` before a byte reaches the index — and again on transcripts returned to clients.
- Local embeddings only. No telemetry. No network after the one-time model download.
- All SQL is parameterised; FTS terms are quoted/escaped; tool inputs are validated (ISO dates, clamped limits).

## Supported sources

| Source | Adapter | Notes |
|---|---|---|
| `opencode.db` V1 (`session`/`message`/`part`) | `sqlite_v1` | Primary, full tool I/O |
| `opencode.db` V2 (`session_v2`/`session_message`) | `sqlite_v2` | OpenCode 2 beta sessions not present in V1 |
| `storage/` legacy JSON tree | `json_tree` | Sessions that never made it into SQLite; SQLite wins on duplicates |

## Development

```bash
cargo test           # offline suite: unit, integration, golden recall@5, concurrency, MCP protocol
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Test coverage maps to the spec: chunking, redaction patterns (positive + negative), RRF, filter SQL, FTS escaping, idempotent re-sync, single-session updates, reading while a WAL writer runs, 15 golden queries with recall@5 tracking, and a full MCP handshake calling every tool over real stdio. Tests are 100 % offline (deterministic fake embedder).

## License

[MIT](LICENSE) © ZorionTen

---
**Keywords:** MCP server, Model Context Protocol, OpenCode, chat history search, RAG, semantic search, hybrid search, BM25, FTS5, sqlite-vec, vector search, local-first, privacy, Rust, AI agents, LLM memory, conversation search, recall.
