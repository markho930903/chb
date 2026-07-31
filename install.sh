#!/bin/sh
set -eu

REPO="markho930903/codex-headroom-bridge"
VERSION="${CODEX_HEADROOM_BRIDGE_VERSION:-latest}"
INSTALL_TMP=""
NEW_BIN=""
NEW_LINK=""

cleanup() {
    if [ -n "$NEW_BIN" ]; then
        rm -f "$NEW_BIN"
    fi
    if [ -n "$NEW_LINK" ]; then
        rm -f "$NEW_LINK"
    fi
    if [ -n "$INSTALL_TMP" ]; then
        rm -rf "$INSTALL_TMP"
    fi
}
trap cleanup EXIT HUP INT TERM

if [ "$(uname -s)" != "Darwin" ]; then
    echo "codex-headroom-bridge currently supports macOS only." >&2
    exit 1
fi

case "$(uname -m)" in
    arm64) TARGET="aarch64-apple-darwin" ;;
    x86_64) TARGET="x86_64-apple-darwin" ;;
    *)
        echo "Unsupported macOS architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

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

TOOL_BIN_DIR="$("$UV_BIN" tool dir --bin)"
HEADROOM_BIN="$TOOL_BIN_DIR/headroom"
BRIDGE_BIN="$TOOL_BIN_DIR/chb"
LEGACY_BIN="$TOOL_BIN_DIR/codex-headroom-bridge"
ASSET="chb-$TARGET"
INSTALL_TMP="$(mktemp -d "${TMPDIR:-/tmp}/codex-headroom-bridge.XXXXXX")"

if [ "$VERSION" = "latest" ]; then
    RELEASE_URL="https://github.com/$REPO/releases/latest/download"
else
    RELEASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi

curl -fsSL "$RELEASE_URL/$ASSET" -o "$INSTALL_TMP/$ASSET"
curl -fsSL "$RELEASE_URL/$ASSET.sha256" -o "$INSTALL_TMP/$ASSET.sha256"
(
    cd "$INSTALL_TMP"
    shasum -a 256 -c "$ASSET.sha256"
)
chmod 755 "$INSTALL_TMP/$ASSET"
"$INSTALL_TMP/$ASSET" --version >/dev/null

"$UV_BIN" tool install --force --python 3.13 "headroom-ai[all]"

# Remove the legacy Python package registration before claiming its command name.
"$UV_BIN" tool uninstall codex-headroom-bridge >/dev/null 2>&1 || true
mkdir -p "$TOOL_BIN_DIR"
NEW_BIN="$TOOL_BIN_DIR/.chb.new.$$"
install -m 755 "$INSTALL_TMP/$ASSET" "$NEW_BIN"
mv -f "$NEW_BIN" "$BRIDGE_BIN"
NEW_BIN=""
NEW_LINK="$TOOL_BIN_DIR/.codex-headroom-bridge.link.$$"
ln -s "chb" "$NEW_LINK"
mv -f "$NEW_LINK" "$LEGACY_BIN"
NEW_LINK=""

"$BRIDGE_BIN" install \
    --headroom-bin "$HEADROOM_BIN" \
    --bridge-bin "$BRIDGE_BIN"

echo "Installed. CC Switch remains unchanged until local routing takeover is enabled."
