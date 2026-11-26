# AGENTS.md — Install & operate RememberMCP

Instructions for AI agents. Follow them end to end; every command is safe,
read-only with respect to OpenCode's data, and verified by `--check`.

## What this is

RememberMCP is an MCP server (`remember-mcp` binary) that indexes **past
OpenCode conversations** from the local OpenCode database into a disposable
SQLite index and serves read-only search over stdio.

## Install (from a clone of this repo)

```bash
cargo build --release                 # Rust 1.89+, no system deps (SQLite bundled)
./target/release/remember-mcp --check # MUST exit 0 → sources reachable, prints JSON
```

`--check` prints JSON with `opencode_db_exists`, adapter session counts, and
index paths. If `opencode_db_exists` is false, ask the user for
`OPENCODE_DB_PATH` instead of guessing.

Optional: `cargo install --path . --locked` puts `remember-mcp` on `PATH`
(`~/.cargo/bin`), or copy `target/release/remember-mcp` to `~/.local/bin/`.

## Register with OpenCode

Edit `~/.config/opencode/opencode.json` (global) or `opencode.json` (project)
and add exactly one entry (merge into the existing `"mcp"` object; do not
overwrite other servers):

```json
{
  "mcp": {
    "history": {
      "type": "local",
      "command": ["/home/USER/.local/bin/remember-mcp"],
      "enabled": true
    }
  }
}
```

Use the absolute path of the binary you built. After editing, restart OpenCode
(or tell the user to) — OpenCode spawns the server as a child process and kills
it on exit; there is no daemon to manage.

## Verify

1. `remember-mcp --check` → `ok: true`.
2. Confirm the MCP entry parses: `python3 -m json.tool ~/.config/opencode/opencode.json`.
3. If a running client is available, call the `index_status` tool; expect
   `sessions`/`chunks` counts (first call may report `building` — that is
   normal; the initial index builds in the background).

## Operate

- Rebuild after code changes: `cargo build --release`.
- Rebuild the index from scratch: call the `reindex` tool with `{"full": true}`,
  or delete `$XDG_DATA_HOME/opencode-memory-mcp/index.db*` and restart.
- First search/reindex downloads the embedding model (~130 MB) once into
  `~/.cache/remember-mcp/fastembed`; offline afterwards. To skip embeddings
  entirely, set `EMBED_MODEL=none` (FTS-only mode).

## Hard rules

- NEVER write to, migrate, checkpoint, or copy OpenCode's `opencode.db` /
  `-wal` / `-shm`. The server opens it read-only; you must too.
- NEVER modify or delete `~/.config/opencode/opencode.json` entries other than
  the `history` MCP entry described above.
- All `remember-mcp` logs go to stderr; stdout is the MCP protocol — never
  pipe anything else into the server's stdin.
