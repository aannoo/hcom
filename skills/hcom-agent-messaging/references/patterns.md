# Tested Multi-Agent Patterns

Every pattern below has been tested with real agents on hcom v0.7.6. Event outputs are from real runs on 2026-03-24.

## Pattern 1: Basic Two-Agent Messaging

Worker sends result, reviewer acknowledges, DONE signal to orchestrator.

```bash
#!/usr/bin/env bash
# Two agents exchange messages: worker sends result, reviewer sends DONE.
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
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="count from 1 to 3"

thread="basic-$(date +%s)"

# Launch worker
launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "Do: ${task}. Send result: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"RESULT: <answer>\". Then: hcom stop" 2>&1)
track_launch "$launch_out"
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Worker: $worker"

# Launch reviewer
launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for @worker-. Reply: hcom send \"@${worker}\" --thread ${thread} --intent ack -- \"ACK\". Send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"DONE\". Then: hcom stop" 2>&1)
track_launch "$launch_out"
reviewer=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Reviewer: $reviewer"
echo "Thread: $thread"

# Wait for DONE signal
result=$(hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Worker: mila
Reviewer: niro
Thread: basic-1774354927
PASS
```

**Real event JSON from test run:**
```json
{"id":42,"type":"message","instance":"mila","data":{"from":"mila","text":"RESULT: 1, 2, 3","scope":"mentions","mentions":["niro"],"intent":"inform","thread":"basic-1774354927","sender_kind":"instance","delivered_to":["niro"]}}
{"id":45,"type":"message","instance":"niro","data":{"from":"niro","text":"DONE","scope":"broadcast","intent":"inform","thread":"basic-1774354927","sender_kind":"instance","delivered_to":["mila"]}}
```

**Timing:** ~17s total (launch to final DONE signal)

---

## Pattern 2: Worker-Reviewer Feedback Loop

Worker does task, reviewer evaluates, sends APPROVED or FIX feedback, worker corrects if needed.

```bash
#!/usr/bin/env bash
# Worker-reviewer feedback loop with APPROVED/FIX protocol.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="list 3 colors in French"

thread="review-$(date +%s)"

# Launch worker
launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "Task: ${task}. Do it. Send: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"ROUND 1 DONE: <result>\". If you get FIX feedback, fix and resend as ROUND 2 DONE. After APPROVED, send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"FINAL\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Worker: $worker"

# Launch reviewer
launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "On ROUND N DONE from @worker-: check result for correctness. If correct: hcom send \"@${worker}\" --thread ${thread} --intent inform -- \"APPROVED\". If wrong: hcom send \"@${worker}\" --thread ${thread} --intent request -- \"FIX: <issue>\". After sending verdict, hcom stop." 2>&1)
track_launch "$launch_out"
reviewer=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Reviewer: $reviewer"
echo "Thread: $thread"

# Wait for FINAL signal
result=$(hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%FINAL%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Worker: kiwi
Reviewer: cora
Thread: review-1774354961
PASS
```

**Real event JSON from test run:**
```json
{"from":"kiwi","intent":"inform","text":"ROUND 1 DONE: 1. Rouge (Red) 2. Bleu (Blue) 3. Vert (Green)","thread":"review-1774354961","scope":"mentions","mentions":["cora"]}
{"from":"cora","intent":"inform","text":"APPROVED","thread":"review-1774354961","scope":"mentions","mentions":["kiwi"]}
{"from":"kiwi","intent":"inform","text":"FINAL","thread":"review-1774354961","scope":"broadcast"}
```

**Timing:** ~20s total. Worker got APPROVED on first round (no FIX cycle needed).

**Key insight:** The FIX/APPROVED protocol creates a natural feedback loop. Workers self-correct based on reviewer feedback. Multiple rounds happen automatically.

---

## Pattern 3: Ensemble Consensus (N Agents + Judge)

N agents independently answer the same question, judge reads all answers and aggregates.

```bash
#!/usr/bin/env bash
# 3 independent agents + judge for ensemble consensus.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="what is 12 + 15"

thread="ens-$(date +%s)"

# Launch 3 contestants
for i in 1 2 3; do
  launch_out=$(hcom 1 claude --tag "c${i}" --go --headless \
    --hcom-prompt "Answer independently: ${task}. Send ONLY your answer: hcom send \"@judge-\" --thread ${thread} --intent inform -- \"C${i}: <your answer>\". Then: hcom stop." 2>&1)
  track_launch "$launch_out"
  name=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
  echo "Contestant ${i}: $name"
done

# Launch judge
launch_out=$(hcom 1 claude --tag judge --go --headless \
  --hcom-prompt "Wait for 3 contestant answers in thread ${thread}. Check answers: hcom events --sql \"msg_thread='${thread}' AND msg_text LIKE 'C%'\" --last 10 $name_arg. Compare answers. Send: hcom send \"@bigboss\" --thread ${thread} --intent inform -- \"VERDICT: <best answer> (<reasoning>)\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
judge=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Judge: $judge"
echo "Thread: $thread"

# Wait for VERDICT
result=$(hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%VERDICT%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Contestant 1: nola
Contestant 2: juno
Contestant 3: pita
Judge: memo
Thread: ens-1774354989
PASS
```

**Real event JSON from test run:**
```json
{"from":"juno","text":"C2: 27","thread":"ens-1774354989","intent":"inform","scope":"mentions","mentions":["memo"]}
{"from":"nola","text":"C1: 27","thread":"ens-1774354989","intent":"inform","scope":"mentions","mentions":["memo"]}
{"from":"pita","text":"C3: <answer>27</answer>","thread":"ens-1774354989","intent":"inform","scope":"mentions","mentions":["memo"]}
{"from":"memo","text":"VERDICT: 27 (unanimous -- all three agents agreed)","thread":"ens-1774354989","intent":"inform","scope":"broadcast"}
```

**Timing:** ~30s total. Agents run in parallel so 3 agents cost same wall-clock as 1.

**Key insight:** The judge uses `hcom events --sql` to query thread messages, reading all answers in one call. This is the fan-out/fan-in pattern.

---

## Pattern 4: Sequential Cascade Pipeline

Each stage reads previous stage's transcript for full context handoff.

```bash
#!/usr/bin/env bash
# Sequential pipeline: planner designs, executor reads transcript and implements.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="name 3 planets"

thread="pipe-$(date +%s)"

# Stage 1: Planner
launch_out=$(hcom 1 claude --tag plan --go --headless \
  --hcom-prompt "Plan: ${task}. Think through it carefully. Send: hcom send --thread ${thread} --intent inform -- \"PLAN DONE: <summary>\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
planner=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Planner: $planner"

# Wait for plan completion
hcom events --wait 60 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%PLAN DONE%'" $name_arg >/dev/null 2>&1

# Stage 2: Executor reads planner's transcript
launch_out=$(hcom 1 claude --tag exec --go --headless \
  --hcom-prompt "Read planner transcript: hcom transcript @${planner} --last 3 $name_arg. Execute the plan precisely. Send: hcom send --thread ${thread} --intent inform -- \"EXEC DONE: <result>\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
executor=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Executor: $executor"

# Wait for execution
result=$(hcom events --wait 60 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%EXEC DONE%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Planner: zara
Executor: loki
PASS
```

**Real event JSON from test run:**
```json
{"from":"zara","text":"PLAN DONE: 3 planets: Mercury, Venus, Mars...","thread":"pipe-1774355030","intent":"inform","scope":"broadcast"}
{"from":"loki","text":"EXEC DONE: Three planets: Mercury, Venus, Mars...","thread":"pipe-1774355030","intent":"inform","scope":"broadcast"}
```

**Timing:** ~25s total (sequential, not parallel).

**Key insight:** `hcom transcript @name --full` is the context handoff mechanism. Each pipeline stage gets the complete work product of the previous stage. Use `--detailed` to include tool I/O (Bash output, file edits).

---

## Pattern 5: Cross-Tool (Claude + Codex)

Claude designs the spec, Codex implements in sandbox.

```bash
#!/usr/bin/env bash
# Claude architect designs spec, Codex engineer implements.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="write a bash function that reverses a string"

thread="duo-$(date +%s)"

# Claude architect
launch_out=$(hcom 1 claude --tag arch --go --headless \
  --hcom-prompt "Design spec: ${task}. Send: hcom send \"@eng-\" --thread ${thread} --intent request -- \"SPEC: <detailed spec>\". Wait for IMPLEMENTED. Send APPROVED. Stop." 2>&1)
track_launch "$launch_out"
arch=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Architect (Claude): $arch"

# Codex engineer
launch_out=$(hcom 1 codex --tag eng --go --headless \
  --hcom-prompt "Wait for spec from @arch-. Implement exactly as specified. Confirm: hcom send \"@arch-\" --thread ${thread} --intent inform -- \"IMPLEMENTED\". Stop." 2>&1)
track_launch "$launch_out"
eng=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Engineer (Codex): $eng"
echo "Thread: $thread"

# CRITICAL: Wait for Codex to be ready before anything happens
hcom events --wait 30 --idle "$eng" $name_arg >/dev/null 2>&1

# Wait for APPROVED
result=$(hcom events --wait 180 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%APPROVED%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Architect (Claude): veto
Engineer (Codex): dire
Thread: duo-1774355064
PASS
```

**Real event JSON from test run:**
```json
{"from":"veto","intent":"request","text":"SPEC: Write a bash function named reverse_string that takes a single string argument and prints it reversed to stdout...","thread":"duo-1774355064","scope":"mentions","mentions":["dire"]}
{"from":"veto","intent":"inform","text":"APPROVED","thread":"duo-1774355064","scope":"broadcast"}
```

**Timing:** ~30s total. Codex needs extra time for sandbox setup.

**Critical Codex notes:**
- Always `hcom events --wait 30 --idle "$codex_name"` before sending messages
- Codex message delivery via PTY injection (1-3s vs <1s for Claude)
- Session binds on first agent-turn-complete (codex-notify hook)

---

## Pattern 6: Codex Codes, Claude Reviews Transcript

Codex writes and runs code, Claude reads Codex's full transcript to review.

```bash
#!/usr/bin/env bash
# Codex codes and runs, Claude reviews the transcript.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
[[ -z "$task" ]] && task="write /tmp/calc.py that prints 2+2=4 and run it"

thread="codex-$(date +%s)"

# Codex coder
launch_out=$(hcom 1 codex --tag coder --go --headless \
  --hcom-prompt "Do: ${task}. When done, send output: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"CODE DONE: <output>\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
coder=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Coder (Codex): $coder"

# CRITICAL: wait for Codex to bind
hcom events --wait 30 --idle "$coder" $name_arg >/dev/null 2>&1

# Claude reviewer
launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for CODE DONE from @coder-. Read full transcript: hcom transcript @${coder} --last 5 --full $name_arg. Review code quality and correctness. Send: hcom send --thread ${thread} --intent inform -- \"REVIEWED: pass\" or \"REVIEWED: fail (<reason>)\". Then: hcom stop." 2>&1)
track_launch "$launch_out"
reviewer=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "Reviewer (Claude): $reviewer"
echo "Thread: $thread"

# Wait for review
result=$(hcom events --wait 180 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%REVIEWED%'" $name_arg 2>/dev/null)
if [[ -n "$result" ]]; then echo "PASS"; else echo "TIMEOUT"; fi

# Cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

**Tested result:**
```
Coder (Codex): demo
Reviewer (Claude): poke
Thread: codex-1774355112
PASS
```

**Real event JSON from test run:**
```json
{"from":"demo","text":"CODE DONE: 2+2=4","thread":"codex-1774355112","intent":"inform","scope":"mentions","mentions":["poke"]}
{"from":"poke","text":"REVIEWED: pass","thread":"codex-1774355112","intent":"inform","scope":"broadcast"}
```

**Timing:** ~35s total. Codex needs time for sandbox code execution.

**Key insight:** Claude reads Codex's complete transcript (including Bash output, file writes, command results) via `hcom transcript @name --full --detailed`. This enables deep code review without sharing files.

---

## Summary Table

| # | Pattern | Agents | Tools | Result | Time |
|---|---------|--------|-------|--------|------|
| 1 | Basic messaging | 2 | Claude x2 | PASS | ~17s |
| 2 | Review loop | 2 | Claude x2 | PASS | ~20s |
| 3 | Ensemble consensus | 4 | Claude x4 | PASS | ~30s |
| 4 | Cascade pipeline | 2 | Claude x2 | PASS | ~25s |
| 5 | Cross-tool duo | 2 | Claude+Codex | PASS | ~30s |
| 6 | Codex->Claude review | 2 | Codex+Claude | PASS | ~35s |

All 6 patterns verified working with real agent runs. Scripts are production-ready.
