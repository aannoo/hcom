#!/usr/bin/env bash
set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -o 2>/dev/null || true)" == "Android" ]]; then
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
