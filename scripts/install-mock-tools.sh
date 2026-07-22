#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${HCOM_MOCK_TOOLS_PREFIX:-$ROOT/target/mock-tools}"
CACHE="${HCOM_MOCK_TOOLS_NPM_CACHE:-$ROOT/target/npm-cache}"

mkdir -p "$PREFIX" "$CACHE"

if [[ "$#" -gt 0 ]]; then
  packages=("$@")
else
  packages=(
    "@openai/codex@0.145.0"
    "@anthropic-ai/claude-code@2.1.216"
  )
fi

claude_version=""
has_claude_native=0
codex_version=""
has_codex_native=0
for package in "${packages[@]}"; do
  case "$package" in
    @openai/codex-linux-* | @openai/codex-darwin-*)
      has_codex_native=1
      ;;
    @openai/codex@*)
      codex_version="${package##*@}"
      ;;
    @anthropic-ai/claude-code@*)
      claude_version="${package##*@}"
      ;;
    @anthropic-ai/claude-code)
      claude_version="2.1.216"
      ;;
    @anthropic-ai/claude-code-*)
      has_claude_native=1
      ;;
  esac
done

os="$(uname -s)"
arch="$(uname -m)"

if [[ -n "$codex_version" && "$has_codex_native" -eq 0 ]]; then
  case "$os:$arch" in
    Darwin:arm64) codex_platform="darwin-arm64" ;;
    Darwin:x86_64) codex_platform="darwin-x64" ;;
    Linux:x86_64) codex_platform="linux-x64" ;;
    Linux:aarch64 | Linux:arm64) codex_platform="linux-arm64" ;;
    *)
      printf 'Unsupported Codex mock-test platform: %s %s\n' "$os" "$arch" >&2
      exit 1
      ;;
  esac
  packages+=(
    "@openai/codex-$codex_platform@npm:@openai/codex@$codex_version-$codex_platform"
  )
fi

if [[ -n "$claude_version" && "$has_claude_native" -eq 0 ]]; then
  case "$os:$arch" in
    Darwin:arm64) claude_platform="darwin-arm64" ;;
    Darwin:x86_64) claude_platform="darwin-x64" ;;
    Linux:x86_64) claude_platform="linux-x64" ;;
    Linux:aarch64 | Linux:arm64) claude_platform="linux-arm64" ;;
    *)
      printf 'Unsupported Claude mock-test platform: %s %s\n' "$os" "$arch" >&2
      exit 1
      ;;
  esac
  packages+=("@anthropic-ai/claude-code-$claude_platform@$claude_version")
fi

npm_platform="$(node -p 'process.platform')"
npm_platform_args=()
if [[ "$npm_platform" == "android" ]]; then
  npm_platform_args+=(--force)
fi

npm install \
  --global \
  --prefix "$PREFIX" \
  --cache "$CACHE" \
  --no-audit \
  --no-fund \
  --fetch-retries 5 \
  --fetch-retry-mintimeout 20000 \
  --fetch-retry-maxtimeout 120000 \
  --fetch-timeout 600000 \
  "${npm_platform_args[@]}" \
  "${packages[@]}"

if [[ -n "$claude_version" ]]; then
  node "$PREFIX/lib/node_modules/@anthropic-ai/claude-code/install.cjs"
fi

if [[ "$npm_platform" == "android" && -n "$claude_version" ]]; then
  claude_native="$PREFIX/lib/node_modules/@anthropic-ai/claude-code-$claude_platform/claude"
  if [[ ! -x "$claude_native" ]]; then
    printf 'Claude native binary is missing: %s\n' "$claude_native" >&2
    exit 1
  fi
  claude_proot_distro="${HCOM_MOCK_TOOLS_CLAUDE_PROOT_DISTRO:-}"
  if [[ -z "$claude_proot_distro" ]]; then
    printf '%s\n' \
      'Android real-Claude tests require a glibc proot distro.' \
      'Set HCOM_MOCK_TOOLS_CLAUDE_PROOT_DISTRO to its proot-distro name.' >&2
    exit 1
  fi
  if [[ ! "$claude_proot_distro" =~ ^[A-Za-z0-9._-]+$ ]]; then
    printf 'Invalid proot distro name: %s\n' "$claude_proot_distro" >&2
    exit 1
  fi
  if ! command -v proot-distro >/dev/null; then
    printf 'proot-distro is required for Android real-Claude tests\n' >&2
    exit 1
  fi
  if ! proot-distro login "$claude_proot_distro" \
    --user "$(id -u):$(id -g)" \
    -- /bin/true >/dev/null 2>&1; then
    printf '%s\n' \
      "Cannot enter proot distro '$claude_proot_distro' as UID $(id -u)." \
      'Configure that user with a valid login shell inside the distro.' >&2
    exit 1
  fi
  rm -f "$PREFIX/bin/claude"
  {
    printf '#!%s\n' "$(command -v bash)"
    printf 'env_args=()\n'
    printf 'while IFS= read -r name; do\n'
    printf '  case "$name" in\n'
    printf '    HCOM_* | ANTHROPIC_* | CLAUDE_* | DISABLE_* | ENABLE_* | XDG_* | CODEX_HOME | DUMMY_KEY | PATH | TMPDIR | CI | LANG | LC_ALL | TERM | NO_COLOR | FORCE_COLOR)\n'
    printf '      env_args+=(--env "$name=${!name}") ;;\n'
    printf '  esac\n'
    printf 'done < <(compgen -e)\n'
    printf 'exec proot-distro login "%s" --user %s:%s --shared-tmp --work-dir "$PWD" "${env_args[@]}" -- /usr/bin/env "HOME=$HOME" "%s" "$@"\n' \
      "$claude_proot_distro" \
      "$(id -u)" \
      "$(id -g)" \
      "$claude_native"
  } >"$PREFIX/bin/claude"
  chmod +x "$PREFIX/bin/claude"
fi

if [[ "$npm_platform" == "android" && -n "$codex_version" ]]; then
  codex_entry="$PREFIX/lib/node_modules/@openai/codex/bin/codex.js"
  if [[ ! -f "$codex_entry" ]]; then
    printf 'Codex entry point is missing: %s\n' "$codex_entry" >&2
    exit 1
  fi
  rm -f "$PREFIX/bin/codex"
  printf '#!%s\nexec "%s" "%s" "$@"\n' \
    "$(command -v bash)" \
    "$(command -v node)" \
    "$codex_entry" \
    >"$PREFIX/bin/codex"
  chmod +x "$PREFIX/bin/codex"
fi

printf '%s\n' "$PREFIX/bin"
