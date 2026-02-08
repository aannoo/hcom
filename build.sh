#!/bin/bash
# Build hcom Rust binary, copy to bundled location, restart daemon
#
# Modes:
#   ./build.sh              — build + copy + restart
#   ./build.sh --post-build — copy + restart only (called by watch.sh)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$SCRIPT_DIR/src/native/target/release/hcom"
BUNDLED_DIR="$SCRIPT_DIR/src/hcom/bin"

get_platform_tag() {
    local system=$(uname -s | tr '[:upper:]' '[:lower:]')
    local machine=$(uname -m | tr '[:upper:]' '[:lower:]')
    [[ "$machine" == "amd64" ]] && machine="x86_64"
    [[ "$machine" == "aarch64" ]] && machine="arm64"
    echo "${system}-${machine}"
}

copy_to_bundled() {
    local tag=$(get_platform_tag)
    local dst="$BUNDLED_DIR/hcom-${tag}"
    mkdir -p "$BUNDLED_DIR"
    # Atomic copy: temp file + rename prevents partial binary issues
    cp "$BINARY" "$dst.tmp.$$"
    mv "$dst.tmp.$$" "$dst"
    chmod +x "$dst"
    echo "Copied to $dst"
}

restart_daemon() {
    if pgrep -f 'hcom\.daemon' >/dev/null 2>&1; then
        echo "Restarting daemon..."
        hcom daemon restart 2>/dev/null || true
    fi
}

# --- Post-build mode: just copy + restart (called by watch.sh after cargo build) ---

if [[ "$1" == "--post-build" ]]; then
    copy_to_bundled
    restart_daemon
    exit 0
fi

# --- Full build mode ---

# Version sync check: all 3 version sources must match
CARGO_VER=$(grep '^version' "$SCRIPT_DIR/src/native/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
PY_VER=$(grep '^__version__' "$SCRIPT_DIR/src/hcom/shared.py" | sed 's/.*"\(.*\)".*/\1/')
TOML_VER=$(grep '^version' "$SCRIPT_DIR/pyproject.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
MISMATCH=""
[[ "$CARGO_VER" != "$PY_VER" ]] && MISMATCH="Cargo.toml=$CARGO_VER shared.py=$PY_VER"
[[ "$CARGO_VER" != "$TOML_VER" ]] && MISMATCH="${MISMATCH:+$MISMATCH, }pyproject.toml=$TOML_VER"
if [[ -n "$MISMATCH" ]]; then
    echo "[build] ERROR: Version mismatch! $MISMATCH (expected $CARGO_VER)"
    echo "[build] Sync all version files before building."
    exit 1
fi

# Cargo has its own file lock in target/ — safe to run alongside watch.sh's cargo-watch
echo "Building hcom..."
cd "$SCRIPT_DIR/src/native" && cargo build --release
copy_to_bundled
restart_daemon

# Hint if hcom isn't on PATH (first-time setup)
if ! command -v hcom &>/dev/null; then
    echo ""
    echo "[build] hcom not found on PATH. To fix:"
    echo "  ln -sf $BINARY ~/.local/bin/hcom"
fi
