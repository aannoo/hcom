#!/usr/bin/env bash
# Sequential pipeline: planner designs, executor reads planner's transcript and implements.
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
task="${task:-list 5 prime numbers}"
thread="casc-$(date +%s)"

trap cleanup ERR

# Stage 1: Planner
launch_out=$(hcom 1 claude --tag plan --go --headless \
  --hcom-prompt "Plan how to: ${task}. Write 2 sentences. Send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"PLAN DONE: <plan>\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
planner=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Planner: $planner"

echo "Waiting for plan..."
hcom events --wait 60 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%PLAN DONE%'" $name_arg >/dev/null 2>&1 || { echo "FAIL (planner timeout)"; exit 1; }

# Stage 2: Executor reads planner's transcript
launch_out=$(hcom 1 claude --tag exec --go --headless \
  --hcom-prompt "Read planner's work: hcom transcript @${planner} --last 3. Execute the plan for: ${task}. Send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"EXEC DONE: <result>\". Then stop: hcom stop" 2>&1)
track_launch "$launch_out"
executor=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Executor: $executor"

echo "Waiting for execution..."
hcom events --wait 60 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%EXEC DONE%'" $name_arg >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
hcom events --sql "msg_thread='${thread}'" --last 10 2>&1
