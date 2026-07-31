#!/bin/sh
set -eu

REPO_URL="https://github.com/markho930903/codex-headroom-bridge.git"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "codex-headroom-bridge currently supports macOS only." >&2
    exit 1
fi

if command -v uv >/dev/null 2>&1; then
    UV_BIN="$(command -v uv)"
else
    curl -LsSf https://astral.sh/uv/install.sh | sh
    UV_BIN="$HOME/.local/bin/uv"
fi

if [ ! -x "$UV_BIN" ]; then
    echo "uv installation did not produce an executable at $UV_BIN" >&2
    exit 1
fi

"$UV_BIN" tool install --force --python 3.13 "headroom-ai[all]"
"$UV_BIN" tool install --force "git+$REPO_URL"

TOOL_BIN_DIR="$("$UV_BIN" tool dir --bin)"
HEADROOM_BIN="$TOOL_BIN_DIR/headroom"
BRIDGE_BIN="$TOOL_BIN_DIR/codex-headroom-bridge"

"$BRIDGE_BIN" install \
    --headroom-bin "$HEADROOM_BIN" \
    --bridge-bin "$BRIDGE_BIN"

echo "Installed. CC Switch remains unchanged until local routing takeover is enabled."
