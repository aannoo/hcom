#!/usr/bin/env bash
# Wait for an agent to go idle, then inject text into another agent or launch a new one with it.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: hcom run onidle <watch-agent> <target> <text> [OPTIONS]

Waits until <watch-agent> is idle (listening). If it is already idle, fires
immediately. What fires depends on <target>:

  agent name (dero)   `hcom term inject dero "<text>" --enter`
  tool keyword (agy)  `hcom agy --hcom-prompt "<text>"`  (launches a new agent)

Tool keywords are whatever `hcom <kw> --help` documents as a launcher —
claude, codex, gemini, opencode, kilo, pi, omp, antigravity/agy, cursor-agent,
kimi, copilot. Tool keywords win over same-named agents.

Options:
  --timeout SEC   Give up if watch-agent never goes idle (default: 3600)
  --no-enter      Type the text but do not submit it (inject mode only)
  -q, --quiet     Only print the final result, no status feedback
  -h, --help      Show this help

Examples:
  hcom run onidle koda dero 'koda finished — take over the review'
  hcom run onidle koda agy 'review what koda just landed'
  hcom run onidle koda omp 'ping' --timeout 120

Exit codes: 0 fired, 1 error, 2 timed out.
EOF
}

name_flag=""
timeout=3600
enter="--enter"
quiet=0
positional=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --name) name_flag="${2:-}"; shift 2 ;;
    --timeout) timeout="${2:-}"; shift 2 ;;
    --no-enter) enter=""; shift ;;
    -q|--quiet) quiet=1; shift ;;
    --) shift; while [[ $# -gt 0 ]]; do positional+=("$1"); shift; done ;;
    -*) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    *) positional+=("$1"); shift ;;
  esac
done

if [[ ${#positional[@]} -lt 3 ]]; then
  usage >&2
  exit 1
fi

watch="${positional[0]}"
target="${positional[1]}"
text="${positional[*]:2}"

name_arg=()
[[ -n "$name_flag" ]] && name_arg=(--name "$name_flag")

if ! [[ "$timeout" =~ ^[0-9]+$ ]]; then
  echo "--timeout must be a whole number of seconds" >&2
  exit 1
fi

say() { (( quiet )) || echo "$@"; }

# Is $target a launch keyword? Ask hcom instead of hardcoding a tool list that
# would rot: every launcher's help opens with a `hcom [N] <kw>` usage line, and
# plain commands (send, list, ...) do not.
is_launch_keyword() {
  local out
  out="$(hcom "$1" --help 2>&1)" || return 1
  grep -qE '^  hcom \[N\] ' <<<"$out"
}

if is_launch_keyword "$target"; then
  mode="launch"
  if [[ -z "$enter" ]]; then
    echo "--no-enter is only meaningful when the target is an agent, not tool '$target'" >&2
    exit 1
  fi
else
  mode="inject"
fi

# Fail on a typo'd name now, not after an hour of waiting.
check=("$watch")
[[ "$mode" == "inject" ]] && check+=("$target")
for agent in "${check[@]}"; do
  if ! hcom list "$agent" status "${name_arg[@]}" >/dev/null 2>&1; then
    echo "Not an agent or tool keyword: $agent (see \`hcom list\`)" >&2
    exit 1
  fi
done

status_of() { hcom list "$1" status "${name_arg[@]}" 2>/dev/null || echo gone; }

# Per-agent detail line. status_detail is last in the template because it holds
# raw shell commands that themselves contain `|`.
# awk must not `exit` early here: closing the pipe early kills `hcom list` with
# SIGPIPE, which under `set -o pipefail` takes the whole script down.
info_of() {
  hcom list --format '{name}|{base_name}|{tool}|{status}|{status_context}|{status_age_seconds}|{status_detail}' \
    "${name_arg[@]}" 2>/dev/null |
    awk -F'|' -v want="$1" '($1==want || $2==want) && !seen {print; seen=1}'
}

icon_for() {
  case "$1" in
    active) printf '▶' ;;
    listening) printf '◉' ;;
    blocked) printf '■' ;;
    inactive|gone) printf '○' ;;
    *) printf '◦' ;;
  esac
}

fmt_age() {
  local s="${1:-}"
  [[ "$s" =~ ^[0-9]+$ ]] || return 0
  if (( s < 60 )); then printf '%ds' "$s"
  elif (( s < 3600 )); then printf '%dm' $(( s / 60 ))
  else printf '%dh' $(( s / 3600 )); fi
}

# Sets DESC ("koda [claude] ▶ active (tool:Bash) 3m — cargo test …") and
# DESC_KEY, the same minus the age — so the wait loop can tell "moved on to
# something new" apart from "same thing, seven seconds later".
DESC=""
DESC_KEY=""
describe() {
  local line name tool status context age detail
  line="$(info_of "$1")" || line=""
  if [[ -z "$line" ]]; then
    DESC="$1 (no details)"
    DESC_KEY="$DESC"
    return
  fi
  IFS='|' read -r name _ tool status context age _ <<<"$line"
  detail="$(cut -d'|' -f7- <<<"$line")"
  DESC_KEY="$name [$tool] $(icon_for "$status") $status"
  [[ -n "$context" ]] && DESC_KEY+=" ($context)"
  DESC="$DESC_KEY${age:+ $(fmt_age "$age")}"
  if [[ -n "$detail" ]]; then
    (( ${#detail} > 70 )) && detail="${detail:0:70}…"
    DESC+=" — $detail"
    DESC_KEY+=" — $detail"
  fi
}

if [[ "$mode" == "launch" ]]; then
  action="launch a new $target agent with the text as its prompt"
elif [[ -n "$enter" ]]; then
  action="type the text into $target and press enter"
else
  action="type the text into $target without pressing enter"
fi
say "plan    when $watch goes idle, $action; gives up after ${timeout}s"

describe "$watch"
say "watch   $DESC"
watch_seen="$DESC_KEY"
if [[ "$mode" == "launch" ]]; then
  say "target  $target — launch a new agent: hcom $target --hcom-prompt ..."
else
  describe "$target"
  say "target  $DESC  ← inject${enter:+ + enter}"
fi
say "text    ${text}"

started=$(date +%s)
deadline=$(( started + timeout ))
armed=0
chunk=10
(( quiet )) && chunk=60

while :; do
  status="$(status_of "$watch")"
  case "$status" in
    listening)
      break
      ;;
    gone|inactive)
      echo "$watch is $status — nothing to wait for" >&2
      exit 1
      ;;
  esac

  if (( ! armed )); then
    say "queued  waiting for $watch to go idle (timeout ${timeout}s)"
    armed=1
  fi

  # Only speak up when the watched agent actually moves on to something else,
  # not every time its status simply gets older.
  describe "$watch"
  if [[ "$DESC_KEY" != "$watch_seen" ]]; then
    say "  [+$(( $(date +%s) - started ))s] $DESC"
    watch_seen="$DESC_KEY"
  fi

  remaining=$(( deadline - $(date +%s) ))
  if (( remaining <= 0 )); then
    echo "Timed out after $(( $(date +%s) - started ))s waiting for $watch to go idle (last status: $status)" >&2
    exit 2
  fi
  # Idle detection is event-driven and instant either way; the chunk only
  # bounds how often the "still working on X" line can refresh.
  (( remaining > chunk )) && remaining=$chunk

  # Blocks until the next listening event for $watch (or the chunk expires).
  # The status re-check at the top of the loop is what actually decides —
  # --wait has a 10s lookback that can replay an already-stale idle event, so
  # debounce a matched wake to avoid spinning through that window.
  if hcom events --idle "$watch" --wait "$remaining" "${name_arg[@]}" >/dev/null 2>&1; then
    sleep 1
  fi
done

waited=$(( $(date +%s) - started ))
if [[ "$mode" == "launch" ]]; then
  say "fired   $watch idle after ${waited}s — launching $target"
  hcom "$target" --hcom-prompt "$text" "${name_arg[@]}"
else
  say "fired   $watch idle after ${waited}s — injecting into $target"
  if [[ -n "$enter" ]]; then
    hcom term inject "$target" "$text" --enter
  else
    hcom term inject "$target" "$text"
  fi
fi
