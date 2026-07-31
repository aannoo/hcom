#!/usr/bin/env bash
# `cargo check` against the MSRV, mirroring the `msrv` job in
# .github/workflows/ci.yml. The version comes from Cargo.toml's rust-version so
# this cannot drift from the manifest it enforces.
#
# Skips loudly when that toolchain is absent: CI is the authority, but a silent
# skip would let `just ci` claim it verified the MSRV when it did not.
set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

msrv="$(sed -n 's/^rust-version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$msrv" ]]; then
  echo "msrv: no rust-version found in Cargo.toml" >&2
  exit 1
fi

if ! command -v rustup > /dev/null 2>&1; then
  echo "msrv: SKIPPED (no rustup on PATH; the ubuntu CI job checks $msrv)"
  exit 0
fi

# Entries are listed as e.g. `1.88.0-aarch64-unknown-linux-gnu`, so a prefix
# match is what answers "is the MSRV series installed".
if ! rustup toolchain list | grep -q "^$msrv"; then
  echo "msrv: SKIPPED ($msrv is not installed; run: rustup toolchain install $msrv)"
  echo "msrv: the ubuntu CI job gates this on every push"
  exit 0
fi

exec cargo "+$msrv" check --locked --all-targets
