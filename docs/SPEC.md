# opencode-memory-mcp (RememberMCP) — Architecture / Build Spec

An MCP server that gives any MCP client (including OpenCode itself) searchable,
read-only access to the user's past OpenCode conversations through a local RAG index.

This document is the build spec. Follow it as written. Where it says VERIFY, check
against the real database on the machine before coding the dependent part.

## 1. Goals and non-goals

Goals
- Answer "what did I do / decide / try before about X" from past OpenCode sessions.
- Hybrid retrieval (keyword + semantic) with filters by project, date range and role.
- Zero infrastructure: one process, local files only, runs over stdio.
- Never modify OpenCode's data. Never send conversation text off the machine.

Non-goals
- Writing to OpenCode's database or resuming sessions.
- Multi-user or remote hosting.
- Indexing other agents' histories (design the adapter interface so it can be added
  later, but ship only OpenCode).

## 2. Stack (decided)

| Concern | Choice |
|---|---|
| Language | Python 3.12, managed with `uv` (superseded: implemented in Rust — see README) |
| MCP | Official MCP SDK, stdio transport |
| Index store | One SQLite file, with `sqlite-vec` for vectors and FTS5 for BM25 |
| Embeddings | `fastembed`-equivalent local ONNX CPU embeddings, default model `BAAI/bge-small-en-v1.5` (384 dims). Pluggable behind an `Embedder` protocol |
| Tests | Offline unit/integration tests, no network |

stdout belongs to the MCP protocol. All logging goes to stderr.

## 3. Source data (OpenCode)

Primary source: SQLite at `$XDG_DATA_HOME/opencode/opencode.db`, default
`~/.local/share/opencode/opencode.db`. Allow an `OPENCODE_DB_PATH` override.
The database runs in WAL mode and is written by a live OpenCode process.

Tables the V1 adapter reads:
- `project`: workspace metadata.
- `session`: one row per conversation. Has an id like `ses_…`, a `directory`,
  a `title`, timestamps and an archived marker.
- `message`: one row per turn. `data` is a JSON blob whose `role` is `user` or `assistant`.
- `part`: one row per piece of a message. `data` is a JSON blob discriminated by `type`.
  Relevant types are text, tool call/result, file, reasoning, patch and snapshot.

Rules for reading it
- Open read-only: `sqlite3.connect("file:<path>?mode=ro", uri=True)` with
  `PRAGMA query_only=ON`. Never take write locks. Never copy the WAL by hand.
- Wrap reads in a short deferred transaction so a session is read consistently.
- Treat `data` JSON as untrusted and drifting. Older rows were copied from the
  previous JSON storage without normalisation, so fields such as `agent`,
  `parentID`, `model` can be missing. Use `.get()` everywhere, skip unparseable
  rows, and count skips in `index_status`.

Fallback sources (each is a separate adapter behind a `SourceAdapter` protocol)
- Legacy JSON tree at `<data-dir>/storage/`: `session/<projectID>/<sessionID>.json`,
  `message/<sessionID>/<messageID>.json`, `part/<messageID>/<partID>.json`.
  Some users have sessions here that never made it into SQLite, so index them too,
  deduplicating by session id with SQLite winning.
- OpenCode 2 (beta) writes `session_v2` and `session_message` tables in the same
  file and does not import V1 history. Ship the V1 adapter first. Add a V2 adapter
  in a later milestone, and write it from `PRAGMA table_info` output, not from guesses.

VERIFY before coding the adapters
- Run `.schema` and `PRAGMA table_info(...)` on the real DB. Confirm the actual
  timestamp and archived column names (sources disagree: `time_created` /
  `time_archived` vs `created_at` / `updated_at`).
- Dump 20 random `part.data` values per `type` to learn the real JSON shapes.
  Commit anonymised samples as test fixtures.
- Check whether the DB contains both V1 and V2 table families.

## 4. Components

```
src/ (Rust crate `remember-mcp`)
  config.rs          env + defaults
  sources/
    mod.rs           SourceAdapter protocol, SessionRecord/MessageRecord/PartRecord
    sqlite_v1.rs     reads session/message/part
    json_tree.rs     legacy storage/ reader
    sqlite_v2.rs     session_v2/session_message (V2-only sessions)
  chunking.rs        exchanges -> chunks
  redact.rs          secret scrubbing before anything is stored
  embed.rs           Embedder protocol + local ONNX impl
  index.rs           index.db schema, upserts, sync
  search.rs          hybrid retrieval + RRF
  server.rs          MCP tools and resources
  main.rs            entrypoint
tests/
  common/            synthetic opencode.db builder + JSON tree fixtures
```

Data flow:

```
opencode.db (read-only)
  -> SourceAdapter -> normalised Session/Message/Part
  -> chunking (exchange-based) -> redact -> embed
  -> index.db (chunks + FTS5 + vec0)
  <- search (FTS top-K + vector top-K -> RRF) <- MCP tools
```

## 5. Index store

Location: `$XDG_DATA_HOME/opencode-memory-mcp/index.db` (override with
`OPENCODE_MEMORY_INDEX`). This is a separate file from OpenCode's DB. It is
disposable and fully rebuildable.

Tables
- `sessions_idx(session_id PK, project_dir, title, source, time_created,
  time_updated, archived, message_count)`
- `chunks(id INTEGER PK, session_id, first_message_id, last_message_id, ts,
  roles, text, token_est, content_hash)`
- `chunks_fts`: FTS5 external-content table over `chunks.text`, tokenizer
  `porter unicode61`.
- `chunks_vec`: `vec0(embedding float[384])`, rowid equals `chunks.id`.
- `sync_state(key PK, value)`: holds watermarks, `embed_model`, `embed_dim`,
  `chunker_version`, `schema_version`.

If `embed_model`, `embed_dim` or `chunker_version` differs from the stored value
at startup, drop and rebuild chunks automatically.

## 6. Chunking

- Unit: an exchange, meaning one user message plus the assistant text that
  follows it until the next user message.
- Target 300 to 600 tokens (estimate as chars/4). Split longer exchanges on
  paragraph boundaries with about 80 tokens of overlap. Never split inside a
  fenced code block unless it alone exceeds the limit.
- Included parts: text, plus tool calls rendered as
  `tool: <name> <first 300 chars of input>` and tool results truncated to the
  first 500 chars.
- Excluded by default: reasoning parts, snapshots, patches, file attachments
  with binary content, and tool results over 20 KB (keep only the tool name and
  a size note). Config flag `INCLUDE_REASONING=1` turns reasoning on.
- Embedding text is prefixed with `Project: <basename> | Session: <title> | <date>`
  so queries mentioning a project or title match. The stored `text` column holds
  the same string.
- `content_hash` is a hash of the chunk text. Unchanged hashes are never re-embedded.

## 7. Redaction

Run before storing or embedding. Replace matches with `[REDACTED:<kind>]`.
- Cloud and API keys (AWS access keys, `sk-…`, `ghp_…`, Slack, Stripe, Google
  API key patterns).
- `Authorization: Bearer …` headers, JWTs, private key blocks, and `KEY=value`
  lines where the key name contains `SECRET`, `TOKEN`, `PASSWORD` or `KEY`.
- Config `EXTRA_REDACT_REGEX` for user patterns.
- Unit-test every pattern with positive and negative cases.

## 8. Sync

No daemon. Sync runs at server start and lazily before a search if the last sync
was more than `SYNC_TTL_SECONDS` ago (default 30). Searches never block on a
full rebuild: if the index is empty, return `status: indexing` with progress.

Algorithm
1. Read the list of `(session_id, time_updated)` from the source. This is a
   cheap query.
2. Compare with `sessions_idx`. New or newer sessions are re-chunked, and their
   old chunks are deleted and replaced in one transaction.
3. Sessions in the index that no longer exist in the source are removed.
4. Embed in batches of 64 and commit per session, so a crash loses at most one
   session.
5. Use a single-writer lock file on `index.db` so two server instances don't
   both sync.

## 9. Retrieval

1. Apply filters (`project`, `since`, `until`, `role`, `session_id`) as SQL
   conditions on candidate ids.
2. Run FTS5 `bm25` for the top 50 and vector KNN for the top 50.
3. Merge with Reciprocal Rank Fusion, `score = Σ 1/(60 + rank)`.
4. Collapse chunks from the same session that are adjacent or overlapping.
5. Return the top `limit` results (default 8, max 25). Each result has
   `chunk_id`, `session_id`, `session_title`, `project`, `timestamp`, `score`
   and a `snippet` capped at about 600 chars with the query terms located if
   possible.

Retrieval must work with FTS only if the embedder is unavailable. Report the
degraded mode in the response.

## 10. MCP interface

Tools (all read-only; set `readOnlyHint: true`)

| Tool | Params | Returns |
|---|---|---|
| `search_conversations` | `query` (str), `project?`, `since?`, `until?` (ISO dates), `role?` (`user`/`assistant`), `limit?` | Ranked snippets with ids and metadata |
| `list_sessions` | `project?`, `since?`, `until?`, `query_title?`, `limit?`, `offset?` | Sessions with title, project, dates, message count |
| `get_session` | `session_id`, `offset?`, `limit?` (messages), `include_tools?` | Ordered messages as plain text, paginated |
| `get_chunk_context` | `chunk_id`, `before?`, `after?` | The chunk plus neighbouring exchanges from the same session |
| `index_status` | none | Counts, last sync, model, mode (hybrid/fts-only), skipped-row count, source paths |
| `reindex` | `full?` (bool) | Starts a sync and returns a summary |

Resource: `opencode://session/{session_id}` returns the full transcript as text.

Output rules
- Return plain text blocks plus `structuredContent` with the same data.
- Hard cap each response at about 8k tokens. Truncate with an explicit
  `truncated: true` and a hint on how to page.
- Tool descriptions must say what the data is ("past OpenCode conversations on
  this machine") and when to use each tool, because the calling model chooses
  tools from them.

## 11. Registering in OpenCode

In `opencode.json` (project) or `~/.config/opencode/opencode.json` (global):

```json
{
  "mcp": {
    "history": {
      "type": "local",
      "command": ["/path/to/remember-mcp"],
      "enabled": true
    }
  }
}
```

Document the equivalent snippet for Claude Desktop and Claude Code in the README.

## 12. Configuration

| Env var | Default |
|---|---|
| `OPENCODE_DB_PATH` | `~/.local/share/opencode/opencode.db` (honour `XDG_DATA_HOME`) |
| `OPENCODE_MEMORY_INDEX` | `~/.local/share/opencode-memory-mcp/index.db` |
| `EMBED_MODEL` | `BAAI/bge-small-en-v1.5` |
| `SYNC_TTL_SECONDS` | `30` |
| `INCLUDE_REASONING` | `0` |
| `EXTRA_REDACT_REGEX` | empty |
| `LOG_LEVEL` | `INFO` (stderr) |

## 13. Security and privacy

- Read-only on the source DB, enforced by the URI mode and `query_only`.
- Local embeddings only. No telemetry, no network calls after the model download.
- Redaction happens before any text reaches the index (section 7).
- File permissions on `index.db` are `0600`.
- Tool inputs are validated (ISO dates, positive limits). All SQL is
  parameterised. Never interpolate user input into FTS `MATCH` without escaping:
  quote each term.
- Transcript text returned to the client is untrusted content from past
  sessions. Wrap it in a clear delimiter in tool output and mention in tool
  descriptions that it is quoted history, not instructions.

## 14. Testing

- Fixture builder that creates a synthetic `opencode.db` with the real V1 schema
  plus the legacy JSON tree, including: missing fields, huge tool outputs, a
  code-fenced message, a session with only tool parts, and an archived session.
- Unit: chunker, redaction patterns, RRF, filter SQL, FTS query escaping.
- Integration: build the index from the fixture, then assert
  `search_conversations` finds known facts; assert re-sync is idempotent and
  that editing a session updates only that session.
- Concurrency: read the source while a second connection writes to it in WAL
  mode. No `database is locked` errors.
- Golden queries: 15 queries with expected session ids, run in CI, tracking
  recall@5.
- Protocol: spawn the server over stdio and call every tool through the MCP client.

## 15. Build order

1. Schema introspection script plus fixtures (section 3, VERIFY items).
2. `sqlite_v1` adapter and the normalised records.
3. Chunker and redaction, with tests.
4. Index with FTS5 only, then `search_conversations` working end to end in
   FTS-only mode.
5. Embeddings plus sqlite-vec and RRF.
6. Remaining tools (`list_sessions`, `get_session`, `get_chunk_context`,
   `index_status`, `reindex`) and the resource.
7. JSON tree adapter and dedup.
8. README, registration snippets, golden-query evals.
9. Later: V2 adapter, optional reranker, optional MCP elicitation for ambiguous
   project filters.

## 16. Acceptance criteria

- Cold index of about 700 sessions completes in under 5 minutes on a laptop CPU.
  An incremental sync of one changed session completes in under 2 seconds.
- A typical search returns in under 300 ms after warm-up.
- The server starts and answers `index_status` while the first index is still
  building.
- OpenCode keeps working normally while the server runs, with no lock errors on
  either side.
- Deleting `index.db` and restarting reproduces the same results.
