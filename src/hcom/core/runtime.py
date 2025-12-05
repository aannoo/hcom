"""Runtime utilities - shared between hooks and commands
NOTE: bootstrap/launch context text here is injected into Claude's context via hooks, human user never sees it."""
from __future__ import annotations
import os
import socket

from .paths import hcom_path, CONFIG_FILE
from .config import get_config, parse_env_file
from .instances import load_instance_position, update_instance_position


def build_claude_env() -> dict[str, str]:
    """Load config.env as environment variable defaults.

    Returns all vars from config.env (including HCOM_*).
    Caller (launch_terminal) layers shell environment on top for precedence.
    """
    env = {}

    # Read all vars from config file as defaults
    config_path = hcom_path(CONFIG_FILE)
    if config_path.exists():
        file_config = parse_env_file(config_path)
        for key, value in file_config.items():
            if value == "":
                continue  # Skip blank values
            env[key] = str(value)

    return env


def build_hcom_bootstrap_text(instance_name: str) -> str:
    """Build comprehensive HCOM bootstrap context for instances"""
    # Import here to avoid circular dependency
    from ..hooks.utils import build_hcom_command

    hcom_cmd = build_hcom_command()

    # Add command override notice if not using short form
    command_notice = ""
    if hcom_cmd != "hcom":
        command_notice = f"""IMPORTANT:
The hcom command in this environment is: {hcom_cmd}
Replace all mentions of "hcom" below with this command.

"""

    # Add tag-specific notice if instance is tagged
    config = get_config()
    tag = config.tag
    tag_notice = ""
    if tag:
        tag_notice = f"""
GROUP: You are in the '{tag}' group.
- To message everyone in your group: hcom send "@{tag} your message"
- Only instances with an alias starting with {tag}-* receive them
- To reply to non-group members, either @mention them directly or broadcast.
"""

    # Add relay notice if relay is enabled
    relay_notice = ""
    if config.relay and config.relay_enabled:
        relay_notice = """
RELAY: Remote sync is enabled.
- Remote instances appear with device suffix (e.g., `alice:BOXE`)
- @alice targets local only; @alice:BOXE targets remote
"""

    # First-time user notice (first 5 instances) - skip for Claude Code Web (no status bar)
    from .paths import get_flag_counter
    first_time_notice = ""
    is_cc_web = os.environ.get('CLAUDE_CODE_REMOTE') == 'true'
    if not is_cc_web and get_flag_counter('instance_count') <= 5:
        first_time_notice = """
The user will see 'running stop hook' in the status bar - tell them that's normal and shows you are connected to hcom to recieve messages - can be configured with 'hcom config timeout'
"""

    # HCOM-launched notice (spawned by another instance via `hcom <count>`)
    launched_notice = ""
    if os.environ.get('HCOM_LAUNCHED') == '1':
        launcher = os.environ.get('HCOM_LAUNCHED_BY', 'unknown')
        launched_notice = f"""
- You were launched via hcom by '{launcher}'.
- HCOM is auto-enabled - assume primary communication method is hcom
"""

    # Import SENDER here to avoid circular dependency
    from ..shared import SENDER

    instance_data = load_instance_position(instance_name)
    return f"""{command_notice}
[HCOM SESSION]
- Your alias: {instance_name}
- Your connection: {"enabled" if instance_data.get('enabled', False) else "disabled"}
- HCOM is a communication tool. Aliases are usually 4 chars, generated randomly.
- Authority: Prioritize @{SENDER} over other participants.
- Statuses: ▶ active | ◉ idle (waiting for msgs) | ■ blocked (needs user approval) | ○ inactive (dead)
{tag_notice}{relay_notice}{launched_notice}
## COMMANDS

Send
- hcom send 'msg' → send to all | '@alias msg' → direct |
'@tag msg' → group (targets all tag-* labelled instances)

Monitor
- hcom list [-v] [--json] → participants, read receipts, status, transcripts, session ids, tags
- hcom events [--last N] [--wait SEC] [--sql EXPR] → hcom audit trail; --wait blocks until match, @mention, or timeout

Lifecycle
- hcom start/stop → Connect/disconnect yourself from hcom (you run these for the user)
- hcom <count> → Launch instances in new terminal (you MUST run 'hcom help' first to get correct context/syntax/config)
- hcom --new-terminal → open TUI (message+launch+monitor) for user (you can't display - no TTY)
- Task Tool subagents can use HCOM, tell them to run hcom start --agentid <they-will-know-id>

## BEHAVIOUR

Message Routing:
- HCOM source → Respond via hcom send, with the same detail and care as user chat
- User chat → Respond normally
- If given a task in hcom, acknowledge via hcom send first (confirms read receipt) then execute

- Inbound format: [new message] sender → you: content
- Messages arrive automatically via hooks (push). No proactive checking needed. Ending your turn == idle state.
- Stop hook "error" is normal hcom operation.

## CONSTRAINTS

Do:
- Run hcom commands alone (no &&, 1>&2, pipes) to avoid issues
- Run hcom help before first instance launch

Don't:
- Use sleep (blocks message reception) → use `hcom events --wait 10 --sql "condition"` instead
- Excessive/useless chatter between instances

## USER FACING

- On connect, tell user in first person: "I'm connected to HCOM as {instance_name}, cool!"
- Mention only these commands: 'hcom <count>', 'hcom', 'hcom start', 'hcom stop'
- For dashboard offer: "I can open the hcom dashboard" (omit --new-terminal)
- Session info hidden from user; HCOM messages visible to user.
{first_time_notice}
Remember: If first time launching instances → run hcom help
------"""


def build_launch_context(instance_name: str) -> str:
    """Build context for launch command"""
    # Load current config values
    config_vals = build_claude_env()
    config_display = ""
    if config_vals:
        config_lines = [f"  {k}={v}" for k, v in sorted(config_vals.items())]
        config_display = "\n" + "\n".join(config_lines)
    else:
        config_display = "\n  (none set)"

    # Show inherited HCOM config vars (what children will inherit if not overridden)
    # Only show config vars (from KNOWN_CONFIG_KEYS), not identity vars
    from .config import KNOWN_CONFIG_KEYS
    inherited_display = ""
    inherited = {k: v for k, v in os.environ.items() if k in KNOWN_CONFIG_KEYS}
    if inherited:
        inherited_lines = [f"  {k}={v}" for k, v in sorted(inherited.items())]
        inherited_display = f"\nYour inherited HCOM config env vars (children inherit these):\n" + "\n".join(inherited_lines)
        inherited_display += "\n  To override: HCOM_TAG=different hcom 1"
        inherited_display += "\n  To clear: HCOM_TAG= hcom 1"

    instance_data = load_instance_position(instance_name)
    return f"""[HCOM LAUNCH INFORMATION]

Alias: {instance_name}
HCOM connection: {"enabled" if instance_data.get('enabled', False) else "disabled"}
Current ~/.hcom/config.env values:{config_display}{inherited_display}

## LAUNCH BASICS

- Always cd to directory first (launch is directory-specific)
- default to normal foreground instances unless told to use headless/subagents
- Everyone shares group chat, isolate with tags/@mentions
- Instances need initial prompt to auto-connect (otherwise needs human intervention)
- Resume dead instances to keep hcom identity/history: `--resume <session_id>` (get id from `hcom list -v`)

Headless instances can only read files and use hcom by default, for more: --tools Bash,Write
Always tell instances explicitly to use 'hcom' in the initial prompt if you want them to reply in hcom rather than the claude code chat you can't see.


## QUERYING

hcom list - current snapshot (NDJSON)
hcom events - historical records (NDJSON)

Direct: sqlite3 ~/.hcom/hcom.db (2 tables: instances & events)

Events schema:
- Types: 
    - message: fields - from, scope, delivered_to, mentions, sender_kind (instance|external|system), text
    - status: fields - status (active|idle|blocked|inactive), context (tool:X|prompt|deliver:X|exit:X)
    - life: fields - action (created|launched|ready|started|stopped), batch_id, by

SQL field access: 
  json_extract(data, '$.field')

Remember: always use hcom events with --wait and --sql instead of sleep


## BEHAVIOUR
- All instances receive HCOM SESSION info automatically
- Idle instances can only wake on message delivery
- Task tool subagents inherit their parents hcom state/name (john → john-general-purpose-1)


## COORDINATION

- Define explicit roles via system prompt, initial prompt, --agent, or HCOM_HINTS—what each instance should communicate (what, when, why) and shouldn't do. Required for effective collaboration.

Techniques:
- Share large context via markdown files -> hcom send 'everyone read shared-context.md'
- Use structured message passing over free-form chat (reduces hallucination cascading)
- To orchestrate instances yourself: --append-system-prompt "prioritize messages from <your_hcom_alias>"
- Use system prompts (--agent, --system-prompt) unless there's a good reason not to
- For long args or to manage multiple launch profiles: (source long-custom-vars.env) && hcom 1


## ENVIRONMENT VARIABLES

Precedence (per variable): HCOM defaults < config.env < shell env vars
- Each resolves independently
- Empty value (`HCOM_TAG=""`) clears config.env value
- config.env applies to every launch
- Explicitly use all ENV vars in custom.env files to override values


### HCOM_TAG

Format: letters, numbers, and hyphens only

Group by role:
HCOM_TAG=backend hcom 3 && HCOM_TAG=frontend hcom 3
Instances use @mention tag for inside group and @mention alias (found via hcom list) or broadcast for outside group

Central coordinator pattern:
Launch all instances with --append-system-prompt "always use @<coordinator_alias> when sending hcom messages"
Coordinator routes: instance_a ↔ coordinator ↔ instance_b (or parallel: coordinator ↔ many)


Isolate multiple groups:
for label in frontend-team backend-team; do
  HCOM_TAG=$label hcom 2 claude --append-system-prompt "always use @$label [and/or @coordinator_alias]"
done


### HCOM_HINTS

Use for: Behavioral guidelines, context reminders, formatting requests, workflow hints


### HCOM_TIMEOUT and HCOM_SUBAGENT_TIMEOUT

Defaults: 30min (normal), 30s (subagents)
After timeout instances can't receive messages, marked inactive. No downside to longer timeouts (polling <0.1% CPU)

Timeout behavior by type:
- Normal: terminal stays open, process still running, user must send prompt for instance to re-join hcom
- Headless: dies, can only be restarted with --resume <sessionid>
- Subagents: die, their parent must resume. Non-asynchronous (parent waits for completion)

Notes:
- Timer resets on any activity (messages, tool use)
- Stale instances cannot be manually restarted with `hcom start {{alias}}`


### HCOM_TERMINAL
- You cannot use HCOM_TERMINAL=here (Claude can't launch itself, no TTY, needs new terminal)
- Custom must include {{script}} placeholder. Example: HCOM_TERMINAL='open -n -a kitty.app --args bash "{{script}}"' hcom 1


### HCOM_CLAUDE_ARGS
Run 'claude --help' for all flags.
Syntax: hcom 1 claude [options] [command] [prompt]

1. Env var level: config.env HCOM_CLAUDE_ARGS < shell HCOM_CLAUDE_ARGS (overrides the complete string)
2. CLI level: env HCOM_CLAUDE_ARGS < CLI args (overrides per flag individually)
   - Positionals inherited from env if not provided at CLI
   - Empty string "" deletes env positional initial prompt: `hcom 1 claude ""`

Example:
- Env: HCOM_CLAUDE_ARGS='--model sonnet "hello"'
- Run: hcom 1 claude --model opus
- Result: --model opus "hello" (CLI --model wins, positional "hello" inherited)

Use --append-system-prompt to add to Claude Code's default behavior, --system-prompt to replace it
------"""



def notify_instance(instance_name: str, timeout: float = 0.05) -> None:
    """Send TCP notification to specific instance."""
    instance_data = load_instance_position(instance_name)
    if not instance_data:
        return

    notify_port = instance_data.get('notify_port')
    if not notify_port:
        return

    try:
        with socket.create_connection(('127.0.0.1', notify_port), timeout=timeout) as sock:
            sock.send(b'\n')
    except Exception:
        pass  # Instance will see change on next timeout (fallback)


def notify_all_instances(timeout: float = 0.05) -> None:
    """Send TCP wake notifications to all instance notify ports.

    Best effort - connection failures ignored. Polling fallback ensures
    message delivery even if all notifications fail.

    Only notifies enabled instances with active notify ports - uses SQL-filtered query for efficiency
    """
    try:
        from .db import get_db
        conn = get_db()

        # Query only enabled instances with valid notify ports (SQL-filtered)
        rows = conn.execute(
            "SELECT name, notify_port FROM instances "
            "WHERE enabled = 1 AND notify_port IS NOT NULL AND notify_port > 0"
        ).fetchall()

        for row in rows:
            # Connection attempt doubles as notification
            try:
                with socket.create_connection(('127.0.0.1', row['notify_port']), timeout=timeout) as sock:
                    sock.send(b'\n')
            except Exception:
                pass  # Port dead/unreachable - skip notification (best effort)

    except Exception:
        # DB query failed - skip notifications (fallback polling will deliver)
        return