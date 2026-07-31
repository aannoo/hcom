#!/usr/bin/env bash
set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -o 2>/dev/null || true)" == "Android" ]]; then
  # TypeScript 7 ships the compiler as a per-platform native (Go) binary and
  # publishes no `@typescript/typescript-android-*` package, so `tsc` cannot
  # resolve an executable here at all. Substituting the linux-arm64 build does
  # not help either: Android's seccomp filter kills it with SIGSYS on startup.
  # The typecheck gate therefore only runs on the ubuntu CI job (see the
  # `typecheck` job in .github/workflows/ci.yml); skipping loudly here keeps a
  # local `just ci` honest about what it did and did not verify.
  if [[ "${HCOM_TYPECHECK_FORCE:-}" != "1" ]]; then
    echo "typecheck: SKIPPED on Android (TypeScript 7 has no android native build)"
    echo "typecheck: plugin types are gated by the ubuntu CI job; set HCOM_TYPECHECK_FORCE=1 to attempt it anyway"
    exit 0
  fi

  # Stage the plugin sources under a dedicated child dir so the rm -rf below
  # can never target a caller-supplied path directly (e.g. HCOM_TYPECHECK_ROOT
  # pointed at the repo would otherwise wipe the whole src/ tree).
  project_root="${HCOM_TYPECHECK_ROOT:-$HOME/.hcom/.cache}/hcom-typecheck-stage"
  if [[ "$project_root" == "$repo_root" ]]; then
    echo "typecheck: refusing to stage into the repo root ($repo_root)" >&2
    exit 1
  fi

  rm -rf "$project_root/src"
  mkdir -p "$project_root/src"
  cp "$repo_root/package.json" "$repo_root/package-lock.json" "$repo_root/tsconfig.json" \
    "$project_root/"
  cp -R "$repo_root/src/omp_plugin" "$repo_root/src/opencode_plugin" \
    "$repo_root/src/pi_plugin" "$project_root/src/"
else
  project_root="$repo_root"
fi

cd "$project_root"
if [[ "${CI:-}" == "true" ]]; then
  npm ci --ignore-scripts
else
  # CI enforces the pinned Node 22 runtime. Local typechecking can also run on a
  # newer Node even when the user's global npm config enables engine-strict.
  npm install --ignore-scripts --prefer-offline --engine-strict=false
fi
npm run typecheck
