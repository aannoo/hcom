#!/bin/bash
# Build hcom Rust binary, copy to bundled location
#
# Modes:
#   ./build.sh              — build + copy
#   ./build.sh --post-build — copy only (called by watch.sh)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NATIVE_DIR="$SCRIPT_DIR"
BUNDLED_DIR="$SCRIPT_DIR/bin"

# Resolve cargo target dir (respects .cargo/config.toml override for Android/noexec filesystems)
_cargo_target_dir() {
    local config="$NATIVE_DIR/.cargo/config.toml"
    if [[ -f "$config" ]]; then
        local dir=$(grep '^target-dir' "$config" | sed 's/.*= *"\(.*\)"/\1/')
        [[ -n "$dir" ]] && echo "$dir" && return
    fi
    echo "$NATIVE_DIR/target"
}
# Set after parsing args — profile depends on --release flag
BINARY=""

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
    ln -sf "hcom-${tag}" "$BUNDLED_DIR/hcom"
    echo "Copied to $dst"
}

# --- Post-build mode: just copy (called by watch.sh after cargo build) ---

if [[ "$1" == "--post-build" ]]; then
    # Post-build: use most recently modified binary
    local_target="$(_cargo_target_dir)"
    rel="$local_target/release/hcom"
    dbg="$local_target/debug/hcom"
    if [[ -f "$rel" && -f "$dbg" ]]; then
        # Pick whichever was modified more recently
        if [[ "$dbg" -nt "$rel" ]]; then
            BINARY="$dbg"
        else
            BINARY="$rel"
        fi
    elif [[ -f "$rel" ]]; then
        BINARY="$rel"
    elif [[ -f "$dbg" ]]; then
        BINARY="$dbg"
    else
        echo "[build] ERROR: no binary found" && exit 1
    fi
    copy_to_bundled
    exit 0
fi

# --- Full build mode ---

RELEASE_MODE=false
[[ "$1" == "--release" ]] && RELEASE_MODE=true

# Ensure cargo is on PATH (zshenv may not be sourced in all environments)
if ! command -v cargo &>/dev/null && [[ -f "$HOME/.cargo/env" ]]; then
    . "$HOME/.cargo/env"
fi
# On Termux, rustup shims may shadow pkg-installed cargo. Prefer working cargo.
if command -v cargo &>/dev/null && ! cargo --version &>/dev/null 2>&1; then
    TERMUX_CARGO="${PREFIX:-/data/data/com.termux/files/usr}/bin/cargo"
    if [[ -x "$TERMUX_CARGO" ]]; then
        export PATH="$(dirname "$TERMUX_CARGO"):$PATH"
    fi
fi

# Cargo has its own file lock in target/ — safe to run alongside watch.sh's cargo-watch
BUILD_FLAG=""
if [[ "$RELEASE_MODE" == true ]]; then
    BUILD_FLAG="--release"
    BINARY="$(_cargo_target_dir)/release/hcom"
    echo "Building hcom (release)..."
else
    BINARY="$(_cargo_target_dir)/debug/hcom"
    echo "Building hcom (debug)..."
fi
cd "$SCRIPT_DIR" && cargo build $BUILD_FLAG

echo "Running tests..."
cargo test || { echo "[build] ERROR: cargo test failed"; exit 1; }

copy_to_bundled

# Deploy to PATH — on noexec filesystems (Android shared storage), copy directly
hcom_path=$(command -v hcom 2>/dev/null || true)
if [[ -n "$hcom_path" ]] && [[ ! -L "$hcom_path" || "$(readlink "$hcom_path" 2>/dev/null)" == */bin/hcom* ]]; then
    # Atomic replace: temp file + mv so running processes keep the old inode
    cp "$BINARY" "$hcom_path.tmp.$$" && mv "$hcom_path.tmp.$$" "$hcom_path" && echo "Updated $hcom_path"
elif [[ -z "$hcom_path" ]]; then
    mkdir -p "$HOME/.local/bin"
    cp "$BINARY" "$HOME/.local/bin/hcom.tmp.$$" && mv "$HOME/.local/bin/hcom.tmp.$$" "$HOME/.local/bin/hcom" && echo "Installed to ~/.local/bin/hcom"
fi
