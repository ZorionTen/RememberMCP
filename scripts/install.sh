#!/usr/bin/env bash
# RememberMCP installer: build, install to ~/.local/bin, verify, print config.
# Safe to re-run. Never touches OpenCode's database.
set -euo pipefail

BIN_NAME="remember-mcp"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${REMEMBER_MCP_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_HINT=0

for arg in "$@"; do
  case "$arg" in
    --check) CHECK_ONLY=1 ;;
    --config-hint) CONFIG_HINT=1 ;;
    -h|--help)
      echo "Usage: $0 [--check] [--config-hint]"
      echo "  --check        verify an existing install only (no build)"
      echo "  --config-hint  print the OpenCode MCP config snippet and exit"
      exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

if [ "${CONFIG_HINT:-0}" = "1" ] || [ "${CONFIG_HINT:-}" = "1" ]; then :; fi

print_config() {
  local path="$1"
  cat <<EOF

Add this to ~/.config/opencode/opencode.json (merge into the "mcp" object):

  "mcp": {
    "history": {
      "type": "local",
      "command": ["$path"],
      "enabled": true
    }
  }

Claude Desktop:  "mcpServers": { "history": { "command": "$path" } }
Claude Code:     claude mcp add history -- $path

OpenCode spawns the server as a child process: it starts when OpenCode starts
and exits when OpenCode exits. No daemon to manage.
EOF
}

if [ "${CHECK_ONLY:-0}" = "1" ]; then
  TARGET="$INSTALL_DIR/$BIN_NAME"
  [ -x "$TARGET" ] || TARGET="$REPO_DIR/target/release/$BIN_NAME"
  [ -x "$TARGET" ] || { echo "error: $BIN_NAME not found — run $0 without --check first" >&2; exit 1; }
  echo "verifying $TARGET"
  exec "$TARGET" --check
fi

echo "==> building $BIN_NAME (release)"
cd "$REPO_DIR"
command -v cargo >/dev/null || { echo "error: Rust/cargo not found — install from https://rustup.rs" >&2; exit 1; }
cargo build --release

echo "==> verifying build"
./target/release/$BIN_NAME --check || {
  echo "warning: --check failed (OpenCode database not found?)" >&2
}

echo "==> installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
install -m 755 "target/release/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
echo "installed: $INSTALL_DIR/$BIN_NAME"

print_config "$INSTALL_DIR/$BIN_NAME"
