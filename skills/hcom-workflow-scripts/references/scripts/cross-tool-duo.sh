#!/usr/bin/env bash
# Claude designs spec, Codex implements it. Cross-tool collaboration.
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
task="${task:-write a python function that adds two numbers}"
thread="duo-$(date +%s)"

trap cleanup ERR

launch_out=$(hcom 1 claude --tag arch --go --headless \
  --hcom-prompt "Design a 2-3 sentence spec for: ${task}. Send to @eng-: hcom send \"@eng-\" --thread ${thread} --intent request -- \"SPEC: <spec>\". Wait for confirmation. Then send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"APPROVED\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
arch=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Architect (Claude): $arch"

launch_out=$(hcom 1 codex --tag eng --go --headless \
  --hcom-prompt "Wait for spec from @arch-. Implement it. Then confirm: hcom send \"@${arch}\" --thread ${thread} --intent inform -- \"IMPLEMENTED\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
eng=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Engineer (Codex): $eng"
echo "Thread: $thread"
echo "Waiting..."

hcom events --wait 180 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%APPROVED%'" $name_arg >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
hcom events --sql "msg_thread='${thread}'" --last 10 2>&1
