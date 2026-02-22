#!/usr/bin/env python3
"""Background glue that watches transcript timeline and connects dots across agents.

Monitors all active agents via timeline, detects conflicts, surfaces relevant info,
and sends FYI messages to agents or @bigboss.

Usage:
    hcom run glue                    # start watching
    hcom run glue --active           # more frequent checks
    hcom run glue --focus "auth"     # prioritize auth-related work
    hcom run glue --once             # single scan then exit
    hcom stop glue                   # stop watching
"""

from __future__ import annotations

import argparse

from hcom.api import HcomError, instances, launch, session


# =============================================================================
# SYSTEM PROMPT - The core intelligence that makes glue valuable
# =============================================================================

OVERSEER_SYSTEM_PROMPT = """You are a background observer watching multiple AI coding agents work across different terminals.

## YOUR ROLE
You are the "connective tissue" that ties together parallel work happening across agents. \
You observe, investigate, and surface insights that individual agents miss because they're focused on their own tasks.

The human user is rapidly alt-tabbing between terminals, skimming responses, sending quick prompts. They miss things. You don't miss things.

## WHAT YOU WATCH

You receive event notifications based on subscription level. Check your startup prompt for which subscriptions are active.

**Always On (Default):**
- `status_context='prompt'` - User submitted a prompt in any terminal
- `msg_from='bigboss'` - Human sent a message via CLI/TUI

**With --watch:**
- `status_val='listening'` - Agent finished a turn (went listening)

**With --active:**
- `status_context='tool:Edit'` or `'tool:Write'` - File modifications (for conflict detection)

**Investigation Tools:**
- `hcom transcript timeline --last 10` - Recent exchanges across all agents
- `hcom transcript @NAME --detailed` - Full context of what agent saw and did
- `hcom events --sql "..." --last N` - Query event history

## WHAT YOU LOOK FOR

### High Priority (warn the agent directly)
- **Conflicts**: Agent A editing file X while Agent B also editing file X (or dependent files)
- **Contradictions**: Agent A says "elephants like pie" but Agent B's research says opposite
- **Stale assumptions**: Agent A using function X that Agent B just deleted/refactored
- **Duplicate work**: Two agents implementing the same helper/abstraction
- **Breaking changes**: Agent A deleted/changed something Agent B depends on

### Medium Priority (FYI to the agent)
- **Relevant research**: Agent A researched topic X that's now relevant to Agent B's task
- **Better approaches**: You know Agent A found a library/pattern that Agent B could use
- **Context sharing**: User told Agent A something that Agent B should know

### Low Priority (info to bigboss)
- **Progress updates**: "Agent A finished auth module, Agent B still on frontend"
- **Interesting observations**: "Agent A and B both used the same library independently"
- **Potential optimizations**: "Agent A could use Agent B's utility function"
- **Concerns**: "Agent A stuck on same error for 3 exchanges"

## HOW TO INVESTIGATE

When you notice something interesting:

1. **Quick check**: `hcom list -v` to see current status of all agents
2. **Events scan**: `hcom events --sql "instance='NAME'" --last 20` to see recent activity
3. **Transcript deep-dive**: `hcom transcript @NAME --detailed --last 5` for full context
4. **Timeline check**: `hcom transcript timeline --last 10` to see recent user interactions
5. **Code verification**: Read actual files if needed to verify claims

Investigation pattern:
1. See something interesting in timeline/events
2. Dig into specific transcript for details
3. Cross-reference with events or other transcripts
4. Verify claims if needed (read code, check research)
5. Only message if you find something actionable

## HOW TO COMMUNICATE

### Message Format
Keep messages SHORT and ACTIONABLE. The agent can ask for more.

**Warning** (potential problem):
```
hcom send "@agent [warn] brief description. Check: [specific pointer]"
```

**FYI** (useful info, no action required):
```
hcom send "@agent [fyi] brief info. More: [how to get details]"
```

**Info to human** (not agent-specific or general observation):
```
hcom send "@bigboss [glue] observation about X and Y"
```

### Example Messages

Good:
- "@luna [warn] @nova editing auth.py - you touched it 2 min ago. coordinate? (transcript @nova 5-7)"
- "@nova [fyi] @luna found rate-limiting library (limiters) - could use for your API work. ask @luna or see transcript 3"
- "@bigboss [glue] @luna and @nova both implementing cache logic - consider shared utility"

Bad (too long, too vague):
- "@luna I noticed you are working on the authentication module and I wanted to let you know..."

## WHAT TO IGNORE

- Style preferences, formatting choices
- Normal sequential work (agent reads files, makes edits - fine)
- Agents working on clearly separate things
- Minor inefficiencies that don't affect others
- "Could be cleaner" type observations

## CADENCE

- Check timeline every time you get an event notification
- Don't spam - batch observations if multiple things in quick succession
- If nothing noteworthy, stay silent (you're background)
- Quality over quantity - only message when genuinely useful

## KEY COMMANDS

```bash
hcom list -v                           # All agents with status
hcom events --last 30                  # Recent events
hcom events --sql "QUERY" --wait 60    # Wait for matching event
hcom transcript @NAME --last 5         # Agent's recent conversation
hcom transcript @NAME --detailed       # With tool I/O details
hcom transcript timeline --last 10     # Timeline across all agents
hcom send "@NAME [warn/fyi] message"   # Send to specific agent
hcom send "@bigboss [glue] info"   # Send to human
```

## CONSTRAINTS

- **Read-only**: Never modify files. You observe and communicate.
- **Non-intrusive**: Don't spam agents. Only message when genuinely useful.
- **Brief**: Short messages. Agent can ask for more via hcom.
- **Humble**: You might be wrong. Frame as FYI not commands.
- **Targeted**: Message the specific agent who needs the info.

Remember: You're the glue holding parallel work together. Surface what matters, skip what doesn't."""


# =============================================================================
# LAUNCH PROMPT - Instructions for the specific session
# =============================================================================


def build_glue_prompt(watch: bool, active: bool, once: bool, focus: str | None) -> str:
    """Build the launch prompt for glue.

    3 subscription levels:
    - Default: prompts + bigboss (just human input)
    - --watch: + listening (agent turn boundaries)
    - --active: + edit/write (file changes for conflict detection)
    """

    # Base subscriptions - always on:
    # User prompts + human CLI messages
    prompt_sub = "hcom events sub \"type='status' AND status_context='prompt'\""
    bigboss_sub = "hcom events sub \"type='message' AND msg_from='bigboss'\""

    # Watch level: agent finished turn
    listening_sub = "hcom events sub \"type='status' AND status_val='listening'\""

    # Active level: file edits for conflict detection
    edit_sub = "hcom events sub \"type='status' AND status_context IN ('tool:Edit', 'tool:Write')\""

    if active:
        # Full monitoring (implies watch)
        mode_desc = "prompts + bigboss + listening + file edits"
        subscriptions = f"""   - {prompt_sub}
   - {bigboss_sub}
   - {listening_sub}
   - {edit_sub}"""
    elif watch:
        # Watch turn boundaries
        mode_desc = "prompts + bigboss + listening"
        subscriptions = f"""   - {prompt_sub}
   - {bigboss_sub}
   - {listening_sub}"""
    else:
        # Default: just human input
        mode_desc = "prompts + bigboss"
        subscriptions = f"""   - {prompt_sub}
   - {bigboss_sub}"""

    parts = [
        f'''Starting glue mode.

## INITIALIZATION

1. Understand the project:
   - Read README.md, ARCHITECTURE.md, or similar overview files if they exist
   - Get a mental model of what this codebase does and its structure

2. Connect to hcom:
   - hcom list -v (see active agents)
   - hcom transcript timeline --detailed --last 20 (recent activity)

3. Set up subscriptions ({mode_desc}):
{subscriptions}

4. Announce ready:
   - hcom send "@bigboss [glue] Online, watching all agents"'''
    ]

    if focus:
        parts.append(f"""
## FOCUS AREA
Pay special attention to work related to: {focus}
Prioritize observations and cross-references involving this area.""")

    if once:
        parts.append("""
## SINGLE PASS MODE
This is a one-time scan. After initial analysis:
1. Report any observations to @bigboss
2. Send any relevant FYIs to agents
3. Exit""")
    else:
        parts.append("""
## WATCH LOOP
After setup, wait for event notifications and investigate as they come:
1. On [event] notification: check timeline, investigate if interesting
2. Cross-reference transcripts when you see overlap
3. Message agents or @bigboss when you find something useful
4. Quality over quantity - stay silent unless you have something useful""")

    return "\n\n".join(parts)


# =============================================================================
# MAIN
# =============================================================================


def main():
    parser = argparse.ArgumentParser(
        description="""Launch background glue that watches the transcript timeline.

Glue is the "connective tissue" for parallel agent work. It:
- Monitors user prompts and bigboss messages across all terminals
- Watches events for file edits and status changes
- Cross-references work between agents
- Surfaces conflicts, contradictions, duplicates, and opportunities
- Sends targeted FYI/warning messages to agents or @bigboss

WHAT IT WATCHES FOR:
  - File collisions (multiple agents editing same files)
  - Complementary work (one agent's research relevant to another)
  - Contradictions (conflicting claims between agents)
  - Stale assumptions (using code another agent changed)
  - Breaking changes (deleted/changed dependencies)
  - Redundant efforts (multiple agents solving same problem)

WHAT IT IGNORES:
  - Style differences, formatting
  - Normal sequential work
  - Agents working on clearly separate things

MESSAGE TYPES:
  - [warn] @agent: Potential problem, check X
  - [fyi] @agent: Useful info, no action needed
  - [glue] @bigboss: General observation

SUBSCRIPTION LEVELS (cumulative):
  Default:  prompts + bigboss (human input only)
  --watch:  + listening (agent turn boundaries)
  --active: + file edits (conflict detection)""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Default - watch human input only:
  hcom run glue

  # Watch agent turn boundaries too:
  hcom run glue --watch

  # Full monitoring with file edit detection:
  hcom run glue --active

  # Focus on auth-related work:
  hcom run glue --focus "authentication"

  # Single scan then exit:
  hcom run glue --once

  # Stop glue:
  hcom stop @glue
""",
    )
    parser.add_argument("--name", help="Your identity (optional)")
    parser.add_argument(
        "--watch",
        "-w",
        action="store_true",
        help="Also watch agent turn boundaries (listening status)",
    )
    parser.add_argument(
        "--active",
        "-a",
        action="store_true",
        help="Full monitoring: also watch file edits (implies --watch)",
    )
    parser.add_argument(
        "--focus",
        "-f",
        help="Focus area to prioritize (e.g., 'auth', 'API', 'database')",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Single scan then exit (no continuous watching)",
    )
    parser.add_argument(
        "--tool",
        choices=["claude", "gemini", "codex", "opencode"],
        default="claude",
        help="AI tool to use for glue (default: claude)",
    )
    parser.add_argument(
        "--interactive",
        "-i",
        action="store_true",
        help="Launch in interactive terminal instead of background",
    )
    args = parser.parse_args()

    # --active implies --watch
    watch = args.watch or args.active

    # Get caller session (optional - allows running without being in hcom)
    caller_dir = None
    try:
        s = session(name=args.name) if args.name else session()
        caller_dir = s.info.get("directory")
    except HcomError:
        # Not in an hcom session - that's fine for launching glue
        pass

    # Check what instances exist
    try:
        active_instances = instances()
    except HcomError:
        active_instances = []

    result = launch(
        1,
        tool=args.tool,
        tag="glue",
        background=not args.interactive,
        system_prompt=OVERSEER_SYSTEM_PROMPT,
        prompt=build_glue_prompt(watch, args.active, args.once, args.focus),
        cwd=caller_dir,
    )

    print(f"Overseer launched ({args.tool}, batch: {result['batch_id']})")
    print()
    if active_instances:
        print("Monitoring:")
        for inst in active_instances[:5]:
            print(f"  - @{inst['name']}")
        if len(active_instances) > 5:
            print(f"  ... and {len(active_instances) - 5} more")
        print()
    if args.focus:
        print(f"Focus: {args.focus}")
    mode_parts = []
    if args.active:
        mode_parts.append("active (+ file edits)")
    elif watch:
        mode_parts.append("watch (+ listening)")
    else:
        mode_parts.append("default (prompts + bigboss)")
    if args.once:
        mode_parts.append("single scan")
    print(f"Mode: {', '.join(mode_parts)}")
    print()
    print("Messages go to:")
    print("  - Agents: [warn] / [fyi] for agent-specific insights")
    print("  - @bigboss: [glue] for general observations")
    print()
    print("Stop: hcom stop @glue")


if __name__ == "__main__":
    main()
