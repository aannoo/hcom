#!/usr/bin/env bash
# Codex writes code, Claude reviews Codex's transcript. Cross-tool review pattern.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do
    hcom kill "$name" --go 2>/dev/null || true
  done
}

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
task="${task:-write /tmp/hello.py that prints hello world and run it}"
thread="codex-$(date +%s)"

trap cleanup ERR

launch_out=$(hcom 1 codex --tag coder --go --headless \
  --hcom-prompt "Do this: ${task}. When done, send output: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"CODE DONE: <output>\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
coder=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Coder (Codex): $coder"

launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for @coder- CODE DONE message. Read their transcript: hcom transcript @${coder} --last 5 --full. Send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"REVIEWED: <pass/fail>\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
reviewer=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Reviewer (Claude): $reviewer"
echo "Thread: $thread"
echo "Waiting..."

hcom events --wait 180 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%REVIEWED%'" $name_arg >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
hcom events --sql "msg_thread='${thread}'" --last 10 2>&1
