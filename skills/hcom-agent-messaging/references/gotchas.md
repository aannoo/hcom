# hcom Script Gotchas

Common issues and fixes discovered during real testing.

## Script Hangs Forever

**Cause:** Missing `--go` flag on `hcom 1 claude`, `hcom kill`, or any command that normally prompts for confirmation.

**Fix:** Always include `--go` on every hcom launch and kill command in scripts.

```bash
# WRONG - hangs waiting for user confirmation
hcom 1 claude --tag worker --headless --hcom-prompt "..."
hcom kill luna

# RIGHT
hcom 1 claude --tag worker --go --headless --hcom-prompt "..."
hcom kill luna --go
```

## Agent Not Receiving Messages

**Diagnosis steps:**
```bash
hcom list                    # Is agent alive? What status?
hcom events --last 5         # Was message actually sent?
hcom events --agent luna     # What has luna seen recently?
```

**Common causes and fixes:**

| Cause | How to detect | Fix |
|-------|---------------|-----|
| Agent already stopped | `hcom list` shows inactive/missing | Check timing; agent may finish before message arrives |
| Agent has not bound session yet | `hcom list` shows "launching" | Wait: `hcom events --wait 30 --idle "$name"` |
| Wrong @-mention syntax | Event shows empty `delivered_to` array | Use `@tag-` prefix, not raw 4-letter name |
| No matching thread | Agent sees no messages | Both sides must use exact same `--thread` value |
| Message scope mismatch | Event `scope` is "mentions" but agent not in `mentions` array | Verify @mention matches agent name or tag |
| Identity binding failed | Agent not in `instances` table | Check `HCOM_PROCESS_ID` env var propagation |

## Messages Leaking Between Workflows

**Cause:** No `--thread` isolation. Without threads, all messages in a broadcast scope reach all agents.

**Fix:** Every workflow must use a unique thread ID:
```bash
thread="my-workflow-$(date +%s)"
# All messages in this workflow use --thread
hcom send @worker- --thread "$thread" -- "task"
hcom events --wait 120 --sql "msg_thread='${thread}' AND msg_text LIKE '%DONE%'"
```

**Why timestamps work:** `$(date +%s)` gives epoch seconds. Even if two workflows start in the same second, different thread prefixes (e.g., "review-" vs "ensemble-") prevent collision.

## SQL LIKE Matching Behavior

`msg_text LIKE '%APPROVED%'` also matches `"approved": true` in JSON because SQLite LIKE is case-insensitive for ASCII characters. This is actually convenient for most use cases.

**Precision matching when needed:**
```bash
# Case-sensitive match (use GLOB instead of LIKE)
hcom events --sql "msg_text GLOB '*APPROVED*'"

# Match exact word boundary
hcom events --sql "msg_text LIKE '% APPROVED%' OR msg_text LIKE 'APPROVED%'"
```

## hcom events --wait Exit Code

Returns **0** on match, **1** on timeout, **2** on SQL error. Use exit code directly:

```bash
hcom events --wait 60 --sql "msg_thread='${thread}' AND msg_text LIKE '%DONE%'" $name_arg >/dev/null 2>&1
case $? in
  0) echo "MATCHED" ;;
  1) echo "TIMEOUT" ;;
  2) echo "SQL ERROR — check your query" ;;
esac
```

In scripts where you only care about success vs failure, `&& echo PASS || echo FAIL` is fine — exit codes 1 and 2 are both failures.

## Agent Name Capture

Names are random 4-letter CVCV words (luna, nemo, bali, kiwi, cora, etc.). Never hardcode them. Always parse from launch output:

```bash
launch_out=$(hcom 1 claude --tag worker --go --headless --hcom-prompt "..." 2>&1)
track_launch "$launch_out"
name=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')
# $name is now "luna" or "nemo" etc.
echo "Launched: $name"
```

**For batch launches (multiple agents):**
```bash
launch_out=$(hcom 3 claude --tag team --go --headless --hcom-prompt "..." 2>&1)
# Names: luna nemo bali (space-separated)
names=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //')
for n in $names; do
  LAUNCHED_NAMES+=("$n")
  echo "Launched: $n"
done
```

## Agent Cleanup on Error

Without cleanup, orphan headless agents run indefinitely consuming resources. Always use `trap cleanup ERR INT TERM` and track launched names. See `script-template.md` for the full pattern.

**Use `hcom kill` not `hcom stop`:** Kill sends SIGTERM and closes the terminal pane. Stop preserves the session for resume but leaves the pane open.

## Broadcast vs Mention Routing

**Broadcast (no @mentions):**
```bash
hcom send -- "everyone sees this"  # No @ prefix = broadcast
```

**Mention (targeted):**
```bash
hcom send @luna -- "only luna sees this"         # Direct mention
hcom send @worker- -- "all workers see this"     # Tag prefix
hcom send @luna @nova -- "luna and nova see this" # Multiple mentions
```

**Common mistake:** Forgetting `--` before the message text. Without `--`, the message text might be parsed as flags.

## Heartbeat and Stale Detection

Agents are marked stale (inactive) if their heartbeat is not updated within tool-dependent thresholds. After system sleep/wake, hcom gives a grace period where heartbeat checks are suspended to prevent mass stale detection after laptop lid close/open.

## Intent System Misuse

**Wrong:** Not using intents, causing agents to over-respond:
```bash
hcom send @worker- -- "FYI: I updated the config"  # No intent = ambiguous
```

**Right:**
```bash
hcom send @worker- --intent inform -- "FYI: I updated the config"   # Worker won't respond
hcom send @worker- --intent request -- "Review this code"           # Worker must respond
hcom send @worker- --intent ack -- "Got it, thanks"                 # Worker ignores
```

The bootstrap teaches agents: `request -> always respond`, `inform -> respond only if useful`, `ack -> don't respond`.

## Thread vs Reply-To vs Scope

| Mechanism | When to use | What it does |
|-----------|-------------|-------------|
| `--thread` | Group related messages | Creates namespace for conversation isolation |
| `--reply-to <id>` | Reference specific message | Links message to an event ID |
| `@mentions` | Target specific agents | Controls delivery scope |
| Broadcast (no @) | Everyone needs to see | Delivers to all active/listening agents |

## TTY/PTY Issues

**Agent shows as "blocked" in hcom list:**
- Cause: Approval prompt detected (OSC9 sequence) or output unstable
- Fix: Agent needs user to approve a tool call, or the PTY delivery detected instability

**Agent terminal pane does not close on kill:**
- Cause: Terminal preset close command failed or pane ID was not captured
- Fix: Manually close the terminal tab/pane. Check `hcom config terminal` for preset.
