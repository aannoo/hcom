# hcom script template — annotated reference

complete annotated template for writing hcom workflow scripts.

## file location and discovery

scripts live in `~/.hcom/scripts/` as `.sh` or `.py` files. hcom discovers them automatically:

```bash
cp my-script.sh ~/.hcom/scripts/my-script.sh
chmod +x ~/.hcom/scripts/my-script.sh
hcom run my-script "task description"
```

description: line 2 comment (after shebang) is shown in `hcom run` listing.

user scripts shadow bundled scripts (confess, debate, fatcow) with the same name.

## full template with commentary

```bash
#!/usr/bin/env bash
# brief description shown in hcom run list.
set -euo pipefail

# --- agent tracking ---
# every script must track launched agents for cleanup.
# without this, orphan headless agents run indefinitely.
LAUNCHED_NAMES=()
track_launch() {
  # hcom launch prints "Names: luna" or "Names: luna nemo kira"
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do
    # use kill (not stop) — kill also closes the terminal pane
    hcom kill "$name" --go 2>/dev/null || true
  done
}
# trap ensures cleanup runs on any error or signal
trap cleanup ERR INT TERM

# --- identity propagation ---
# hcom injects --name before script args. MUST parse and forward it.
# without this, the script's hcom commands can't identify the caller.
name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name_flag="$2"; shift 2 ;;
    -h|--help) echo "Usage: hcom run my-script [OPTIONS] TASK"; exit 0 ;;
    -*) shift ;;  # skip unknown flags
    *) task="$1"; shift ;;
  esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"
# default task if none provided
task="${task:-default task here}"

# --- unique thread for isolation ---
# without --thread, messages leak across concurrent workflows
thread="my-workflow-$(date +%s)"

# --- launch agents ---
launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "task: ${task}. when done: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"DONE: <result>\". then: hcom stop" 2>&1)
track_launch "$launch_out"
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
echo "worker: $worker"

# --- wait for completion signal ---
# use hcom events --wait, NEVER sleep
hcom events --wait 120 \
  --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'" \
  $name_arg >/dev/null 2>&1 && echo "PASS" || echo "TIMEOUT"

# --- cleanup ---
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do
  hcom kill "$name" --go 2>/dev/null || true
done
```

## key conventions

| convention | why |
|------------|-----|
| `--go` on every launch/kill | prevents script from hanging on confirmation prompt |
| `--headless` on every launch | runs agent in background (no terminal window needed) |
| `--tag X` on every launch | enables `@X-` prefix routing (more reliable than raw names) |
| `--thread` on every send/wait | isolates messages per workflow run |
| `--intent` on every send | tells recipient whether to respond |
| `trap cleanup ERR INT TERM` | ensures orphan agents are killed on script failure or signal |
| parse `--name` from args | hcom injects this — must forward to all hcom commands |
| capture name from `Names:` | agent names are random 4-letter words, never hardcode |

## python scripts

```python
#!/usr/bin/env python3
"""brief description shown in hcom run list."""
import subprocess, sys, time, json

def hcom(*args):
    result = subprocess.run(["hcom"] + list(args), capture_output=True, text=True)
    return result.stdout.strip()

def main():
    thread = f"py-{int(time.time())}"
    # launch agent
    out = hcom("1", "claude", "--tag", "worker", "--go", "--headless",
               "--hcom-prompt", f"do task. send DONE to @bigboss via thread {thread}. stop.")
    # parse name
    for line in out.split("\n"):
        if line.startswith("Names: "):
            name = line.replace("Names: ", "").strip()
            break
    # wait
    hcom("events", "--wait", "120", "--sql",
         f"type='message' AND msg_thread='{thread}' AND msg_text LIKE '%DONE%'")
    # cleanup
    hcom("kill", name, "--go")

if __name__ == "__main__":
    main()
```

## batch launches

```bash
# launch 3 agents at once
launch_out=$(hcom 3 claude --tag team --go --headless \
  --hcom-prompt "answer the question" 2>&1)
# "Names: luna nemo kira" (space-separated)
names=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //')
for n in $names; do
  LAUNCHED_NAMES+=("$n")
done

# with batch coordination
launch_out=$(hcom 3 claude --tag team --go --headless \
  --batch-id "batch-$(date +%s)" \
  --hcom-prompt "answer the question" 2>&1)
```
