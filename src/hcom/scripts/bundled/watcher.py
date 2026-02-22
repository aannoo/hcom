#!/usr/bin/env python3
"""Background code reviewer that subscribes to activity, sends review back via hcom

Launches a reviewer instance that:
1. Subscribes to target's idle events (or edit events with --active)
2. Reads their transcript when triggered
3. Sends feedback via hcom if it spots issues

Usage:
    hcom run watcher --target luna               # watch luna
    hcom run watcher --target luna --active      # review every ~10 edits
    hcom run watcher --target luna --once        # single review then exit
"""

from __future__ import annotations

import argparse
import sys

from hcom.api import HcomError, instances, launch

REVIEWER_SYSTEM_PROMPT = """You are a background code reviewer watching another Claude instance work.

Your job: catch real problems early. Not nitpick, not backseat drive - just flag things that will actually cause issues.

When triggered to review, check the target's recent work and decide:
- LGTM: Nothing wrong. Send brief confirmation to @bigboss so they know you're working.
- FLAG: Real issue found. Send alert to the target with specifics.

What counts as a real issue:
- Bugs, logic errors, off-by-one mistakes
- Security problems (injection, exposed secrets, unsafe operations)
- Incomplete work (started something, didn't finish)
- Wrong assumptions (misread requirements, wrong file, etc)
- Failed operations they might have missed

What to ignore:
- Style preferences, formatting
- "Could be cleaner" type observations
- Missing tests/docs unless explicitly requested
- Alternate approaches that aren't clearly better

When you flag something:
- Be specific: file, line, what's wrong
- Be brief: one issue per message, no essays
- Be actionable: what should they check/fix

You have full repo access. Read files to verify before flagging.
Do not modify any files. You are read-only."""


def build_watcher_prompt(target: str, once: bool, active: bool) -> str:
    once_flag = " --once" if once else ""

    if active:
        # Active mode: trigger every 10 tool events without going listening (modulo)
        sql = (
            f"type='status' AND instance='{target}' AND status_context LIKE 'tool:%' "
            f"AND (SELECT COUNT(*) FROM events_v e2 WHERE e2.instance='{target}' "
            f"AND e2.status_context LIKE 'tool:%' AND e2.id > COALESCE("
            f"(SELECT MAX(id) FROM events_v e3 WHERE e3.instance='{target}' AND e3.status_val='listening'), 0)) % 10 = 0"
        )
        subscription = f'''hcom events sub "{sql}"{once_flag}'''
        trigger_desc = "every 10 tool uses (if no listening)"
    else:
        # Default: listening-only
        subscription = (
            f'''hcom events sub "type='status' AND instance='{target}' AND status_val='listening'"{once_flag}'''
        )
        trigger_desc = "each [event] notification (they went listening)"

    return f'''You are watching @{target}.

1. First, explore the codebase to understand the architecture and patterns
2. Subscribe: {subscription}
3. Review on {trigger_desc}
4. To review: hcom transcript @{target} --last 5 --detailed
5. After reviewing:
   - Issue found: hcom send "@{target} [spotted] <brief description>"
   - All good: hcom send "@bigboss [lgtm] @{target} - <one line summary of what they did>"

Start by exploring the repo, then set up your subscription and confirm: "Watching @{target}"'''


def main():
    parser = argparse.ArgumentParser(
        description="""Launch a background reviewer that watches another instance's work.

The reviewer subscribes to status events and periodically checks the target's
transcript for issues. It's read-only and runs in background.

WHAT IT CHECKS:
  - Bugs, logic errors, off-by-one mistakes
  - Security problems (injection, exposed secrets)
  - Incomplete work (started but not finished)
  - Wrong assumptions (misread requirements)
  - Failed operations that might have been missed

WHAT IT IGNORES:
  - Style preferences, formatting
  - "Could be cleaner" observations
  - Missing tests/docs unless requested
  - Alternative approaches that aren't clearly better

OUTPUT:
  - Issues → sent to target: "@target [spotted] <description>"
  - All good → sent to @bigboss: "[lgtm] @target - <summary>"

MODES:
  Default: Reviews when target goes idle (listening status)
  --active: Reviews every ~10 tool uses (catches issues faster)
  --once: Single review then exits (for spot checks)""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Watch luna, review when she goes idle:
  hcom run watcher --target luna

  # Watch nova actively (every ~10 edits):
  hcom run watcher --target nova --active

  # One-time review of current work:
  hcom run watcher --target luna --once

  # Stop the watcher:
  hcom stop reviewer
""",
    )
    parser.add_argument("--target", required=True, help="Instance to watch and review")
    parser.add_argument("--name", help="Your identity")
    parser.add_argument("--once", action="store_true", help="Do one review then exit (spot check mode)")
    parser.add_argument(
        "--active",
        "-a",
        action="store_true",
        help="Review every ~10 tool uses instead of only when idle",
    )
    parser.add_argument(
        "--tool",
        choices=["claude", "gemini", "codex", "opencode"],
        default="claude",
        help="AI tool to use for reviewer (default: claude)",
    )
    parser.add_argument(
        "--interactive",
        "-i",
        action="store_true",
        help="Launch in interactive terminal instead of background",
    )
    args = parser.parse_args()

    # Validate target exists
    try:
        info = instances(name=args.target)
    except HcomError:
        print(f"Error: instance '{args.target}' not found", file=sys.stderr)
        sys.exit(1)

    target = info["name"]
    target_dir = info.get("directory")

    result = launch(
        1,
        tool=args.tool,
        tag="reviewer",
        background=not args.interactive,
        system_prompt=REVIEWER_SYSTEM_PROMPT,
        prompt=build_watcher_prompt(target, args.once, args.active),
        cwd=target_dir,
    )

    print(f"Reviewer launched ({args.tool}, batch: {result['batch_id']})")
    print(f"Watching: @{target}")
    mode_parts = []
    if args.active:
        mode_parts.append("active (every ~10 edits)")
    else:
        mode_parts.append("listening")
    if args.once:
        mode_parts.append("single review")
    print(f"Mode: {', '.join(mode_parts)}")
    print()
    print("Issues -> @target, LGTM -> @bigboss")
    print("Stop: hcom stop reviewer")


if __name__ == "__main__":
    main()
