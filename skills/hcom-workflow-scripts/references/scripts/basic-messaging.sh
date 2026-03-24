#!/usr/bin/env bash
# Two agents exchange messages: worker sends result, reviewer acknowledges.
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
task="${task:-count from 1 to 5}"
thread="basic-$(date +%s)"

trap cleanup ERR

launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "Do this: ${task}. Send result to @reviewer- via: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"RESULT: <your answer>\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Worker: $worker"

launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for a message from @worker-. When received, reply: hcom send \"@${worker}\" --thread ${thread} --intent ack -- \"ACK: received\". Then send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"DONE\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
reviewer=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Reviewer: $reviewer"
echo "Thread: $thread"
echo "Waiting..."

hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'" $name_arg >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
hcom events --sql "msg_thread='${thread}'" --last 10 2>&1
