#!/usr/bin/env python3
"""Fat cow - a fat agent that deeply reads a module and answers questions on demand.

Launches a headless Claude that gorges on a codebase module, memorizing files
with line references. Sits in background answering questions from other agents
via hcom. Subscribes to file changes to stay current.

Two modes:
  Live (default): Stays running, answers in real-time, tracks file changes.
  Dead (--dead):  Ingests then stops. Resumed on demand via --ask.

Usage:
    hcom run fatcow --path src/tools                          # live fatcow
    hcom run fatcow --path src/tools --dead                   # dead fatcow (ingest + stop)
    hcom run fatcow --ask fatcow.tools-luna "what does db.py export?"  # query
    hcom run fatcow --path src/ --focus "auth, middleware"           # with focus areas
    hcom stop @fatcow.tools                                         # kill by tag
"""

from __future__ import annotations

import argparse
import os
import sys

from hcom.api import launch


FATCOW_SYSTEM_PROMPT_LIVE = """You are a fat cow - a dedicated codebase oracle.

Your sole purpose is to deeply read and internalize a section of the codebase, then sit in background \
answering questions from other agents instantly. You are a living index.

## INGESTION

Read EVERY file in your assigned path. Not skimming - full reads. \
Understand structure, exports, imports, types, functions, classes, constants, error handling, edge cases. \
Know where things are by file and line number.

After reading, build a mental map: what each file does, all exported signatures, key constants, \
integration points, error patterns, data flow.

## ANSWERING

- Specific file paths and line numbers (e.g., `src/tools/auth.ts:42`)
- Exact function signatures, not approximations
- Actual code patterns, not summaries
- Cross-references within the module
- If outside your scope, say so immediately. Never guess.

## CONSTRAINTS

- **Read-only**: Never modify files.
- **Stay loaded**: Don't summarize away details.
- **Stay current**: When you get a file change notification, re-read that file immediately.
- **Be fast**: Other agents are waiting. Answer directly, no preamble."""

FATCOW_SYSTEM_PROMPT_DEAD = """You are a fat cow - a dedicated codebase oracle.

You ingest a section of the codebase deeply, then stop. You are resumed on demand to answer questions.

## INGESTION

Read EVERY file in your assigned path. Not skimming - full reads. \
Understand structure, exports, imports, types, functions, classes, constants, error handling, edge cases. \
Know where things are by file and line number.

## ANSWERING

- Specific file paths and line numbers (e.g., `src/tools/auth.ts:42`)
- Exact function signatures, not approximations
- Actual code patterns, not summaries
- If outside your scope, say so immediately. Never guess.

## CONSTRAINTS

- **Read-only**: Never modify files.
- **Stay loaded**: Don't summarize away details."""


def _focus_section(focus: str | None) -> str:
    if not focus:
        return ""
    return f"""
## FOCUS AREAS
Pay special attention to: {focus}
When reading files, prioritize understanding these aspects deeply. But still read everything."""


def _ingest_section(path: str) -> str:
    return f"""## PHASE 1: INGEST

1. First, get the lay of the land:
   - List all files recursively in `{path}`
   - Count them. You need to read ALL of them.

2. Read every file. Use the Read tool on each one. Do not skip any file.
   - For large files, read them in chunks if needed, but read the WHOLE file.
   - As you read, note key structures: functions, classes, types, exports, imports.
   - Track line numbers for important definitions.

3. After reading all files, do a second pass on the most important/complex files \
to solidify your understanding."""


def build_fatcow_prompt(path: str, file_glob: str, notify: str, focus: str | None) -> str:
    """Build the launch prompt for a live fatcow agent."""
    return f"""You are a fat cow for: `{path}`
{_focus_section(focus)}
{_ingest_section(path)}

## PHASE 2: INDEX & SUBSCRIBE

4. Subscribe to file changes in your scope so you stay current:
   - `hcom events sub --file "{file_glob}"`
   - When you get an [event] notification, re-read the changed file immediately.

5. Announce you're ready:
   - `hcom send "@{notify} [fatcow] Loaded {path} - ready for questions"`

## PHASE 3: ANSWER

6. Wait for questions. When a message arrives:
   - Parse what they're asking about
   - Answer with file:line references
   - Be thorough but direct
   - Reply via: `hcom send "@<asker> <answer>"`

7. On file change notifications:
   - Check which file changed
   - If it's in your scope, re-read it
   - Update your mental model
   - No need to announce updates unless asked

You are a fat, lazy, knowledge-stuffed oracle. Eat all the files. Sit there. Answer questions. Stay current."""


def build_dead_fatcow_prompt(path: str, focus: str | None) -> str:
    """Build the launch prompt for a dead fatcow (ingest then stop)."""
    return f"""You are a fat cow for: `{path}`
{_focus_section(focus)}
{_ingest_section(path)}

## PHASE 2: CONFIRM & STOP

4. Summarize what you indexed: file count, key modules, major exports/functions.

5. Stop yourself: run `hcom stop`

Do NOT subscribe to events. Do NOT wait for questions. Summarize, then stop."""


def build_resume_prompt(question: str, caller: str, changed_files: list[str]) -> str:
    """Build the prompt injected when resuming a dead fatcow for a query."""
    catchup = ""
    if changed_files:
        file_list = "\n".join(f"  - {f}" for f in changed_files)
        catchup = f"""## FILE CHANGES SINCE LAST RUN

These files in your scope were modified since you last ran. Re-read them before answering:

{file_list}

"""

    return f"""{catchup}## QUESTION FROM @{caller}

{question}

## INSTRUCTIONS

1. {"Re-read the changed files above, then answer" if changed_files else "Answer"} with file:line precision.
2. Send your answer: `hcom send @{caller} -- <your answer>`
3. Stop yourself: run `hcom stop`"""


# ---------------------------------------------------------------------------
# Helpers for --ask
# ---------------------------------------------------------------------------

def _load_stopped(name: str) -> tuple[dict | None, str | None]:
    """Load snapshot and timestamp from the most recent stopped event.

    Returns (snapshot_dict, timestamp) or (None, None).
    """
    from hcom.api import session

    try:
        s = session(name="fatcow-q", external=True)
    except Exception:
        return None, None

    events = s.events(agent=name, action="stopped", last=1)
    if not events:
        return None, None

    ev = events[0]
    snapshot = ev.get("data", {}).get("snapshot")
    if not snapshot:
        return None, None
    return snapshot, ev.get("ts")


def _get_file_changes_since(timestamp: str, scope_dir: str) -> list[str]:
    """Unique file paths changed since timestamp, filtered to scope_dir."""
    from hcom.api import session

    try:
        s = session(name="fatcow-q", external=True)
    except Exception:
        return []

    scope = os.path.abspath(scope_dir)
    glob = f"{scope}/*" if os.path.isdir(scope) else scope

    events = s.events(file=glob, after=timestamp, last=500)

    seen = set()
    changed = []
    for ev in events:
        path = ev.get("data", {}).get("detail", "")
        if not path or path in seen:
            continue
        seen.add(path)
        changed.append(path)
    return changed


def _resolve_caller(explicit: str | None) -> str:
    """Resolve caller identity from flag or environment."""
    try:
        from hcom.api import session
        return session(name=explicit).name
    except Exception:
        return "fatcow-q"


def _is_active(name: str) -> bool:
    try:
        from hcom.api import instances
        instances(name=name)
        return True
    except Exception:
        return False


def _wait_for_reply(fatcow_name: str, caller_name: str, timeout: int) -> int:
    """Block until fatcow replies, print the answer. Returns exit code."""
    from hcom.api import session

    try:
        s = session(name=caller_name)
    except Exception:
        s = session(name=caller_name, external=True)

    event = s.wait(
        "type='message' AND msg_from=?",
        params=[fatcow_name],
        timeout=timeout,
    )

    if event:
        text = event.get("data", {}).get("text", "")
        print(text)
        return 0
    else:
        print(f"Fatcow did not respond within {timeout}s", file=sys.stderr)
        return 1


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

def ask_fatcow(fatcow_name: str, question: str, *, caller: str | None, timeout: int) -> int:
    """Ask a fatcow a question. Handles both live (send) and dead (resume) fatcows.

    If caller has an hcom identity, returns immediately after launch/send —
    the answer arrives via normal hcom message delivery.
    If no identity (bare terminal), blocks and prints the answer to stdout.
    """
    caller_name = _resolve_caller(caller)
    has_identity = caller_name != "fatcow-q"

    # Live fatcow — just send the question
    if _is_active(fatcow_name):
        from hcom.api import session

        try:
            s = session(name=caller_name)
        except Exception:
            s = session(name=caller_name, external=True)
        s.send(f"@{fatcow_name} {question}")
        if has_identity:
            print(f"Asked {fatcow_name} — answer will arrive via hcom")
            return 0
        print(f"Sent to live fatcow {fatcow_name}, waiting for reply...")
        return _wait_for_reply(fatcow_name, caller_name, timeout)

    # Dead fatcow — resume via api.launch
    snapshot, stopped_ts = _load_stopped(fatcow_name)
    if not snapshot:
        print(f"Error: '{fatcow_name}' not found (no stopped snapshot)", file=sys.stderr)
        return 1

    fatcow_dir = snapshot.get("directory", "")
    fatcow_tool = snapshot.get("tool", "claude")
    changed = _get_file_changes_since(stopped_ts, fatcow_dir) if stopped_ts else []

    prompt = build_resume_prompt(question, caller_name, changed)

    # Claude: background + allowedTools for self-stop. Gemini/codex: interactive, agent self-stops.
    is_claude = fatcow_tool == "claude"
    try:
        result = launch(
            resume=fatcow_name,
            prompt=prompt,
            background=is_claude,
            claude_args='--allowedTools "Bash(hcom stop:*)"' if is_claude else None,
            wait=True,
        )
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1

    if result.get("launched", 0) == 0:
        print("Error: Resume failed", file=sys.stderr)
        return 1

    changes_msg = f", {len(changed)} file(s) changed" if changed else ""
    if has_identity:
        print(f"Resumed {fatcow_name}{changes_msg} — answer will arrive via hcom")
        return 0
    print(f"Resumed {fatcow_name}{changes_msg}, waiting for answer...")
    return _wait_for_reply(fatcow_name, caller_name, timeout)


def _fatcow_tag(path: str) -> str:
    """Build fatcow tag from path. E.g. 'src/tools' → 'fatcow.tools', 'src/' → 'fatcow.src'.

    Tag must not contain dashes (breaks tag-name resolution which splits on first dash).
    """
    import re
    basename = os.path.basename(os.path.abspath(path).rstrip("/"))
    # Strip non-alphanumeric (except dots/underscores), replace dashes with dots
    clean = re.sub(r"[^a-zA-Z0-9._]", "", basename.replace("-", "."))
    if not clean:
        return "fatcow"
    # Truncate: tag + "-" + 4-char name must be reasonable
    return f"fatcow.{clean[:20]}"


def launch_fatcow(args, *, dead: bool) -> int:
    """Launch a live or dead fatcow."""
    target_path = os.path.abspath(args.path)
    if not os.path.exists(target_path):
        print(f"Error: path '{args.path}' does not exist", file=sys.stderr)
        return 1

    cwd = os.path.dirname(target_path) if os.path.isfile(target_path) else target_path
    display_path = args.path
    tag = _fatcow_tag(args.path)

    # For directories: glob matches files inside. For files: exact match.
    file_glob = f"{target_path}/*" if os.path.isdir(target_path) else target_path
    try:
        from hcom.api import session as _session
        notify = _session(name=args.name).name
    except Exception:
        notify = "bigboss"

    if dead:
        prompt = build_dead_fatcow_prompt(display_path, args.focus)
        # Gemini/codex don't support background — run interactive, agent self-stops
        background = args.tool == "claude"
        system_prompt = FATCOW_SYSTEM_PROMPT_DEAD
    else:
        prompt = build_fatcow_prompt(display_path, file_glob, notify, args.focus)
        # Gemini/codex don't support background — run interactive
        background = not args.interactive and args.tool == "claude"
        system_prompt = FATCOW_SYSTEM_PROMPT_LIVE

    # Claude dead fatcows need --allowedTools for hcom stop (sandbox restriction)
    extra_args = '--allowedTools "Bash(hcom stop:*)"' if dead and args.tool == "claude" else None

    result = launch(
        1,
        tool=args.tool,
        tag=tag,
        background=background,
        system_prompt=system_prompt,
        prompt=prompt,
        claude_args=extra_args,
        cwd=cwd,
        wait=True,
    )

    if result.get("failed", 0) > 0 or result.get("launched", 0) == 0:
        errors = result.get("errors", [])
        if errors:
            error_msgs = [e.get("error", str(e)) for e in errors]
            print(f"Error: Launch failed: {'; '.join(error_msgs)}", file=sys.stderr)
        else:
            print("Error: Launch failed (no instances launched)", file=sys.stderr)
        return 1

    # Extract instance name from launch status
    status = result.get("launch_status", {})
    instances = status.get("instances", [])
    fatcow_name = instances[0] if instances else None

    mode = "dead" if dead else "live"
    # Display name includes tag: e.g. "fatcow.tools-luna"
    display_name = f"{tag}-{fatcow_name}" if fatcow_name else None
    if display_name:
        print(f"{display_name}: {mode} fatcow ({args.tool})")
    else:
        print(f"Fatcow launched ({mode}, {args.tool}, batch: {result['batch_id']})")
    print(f"Ingesting: {display_path}")
    if args.focus:
        print(f"Focus: {args.focus}")
    print()

    if dead:
        name_hint = display_name or "<name>"
        print("Dead fatcow — will stop after ingestion.")
        print("Query it later:")
        print(f'  hcom run fatcow --ask {name_hint} "what does {display_path} export?"')
    else:
        at_name = f"@{display_name}" if display_name else f"@{tag}"
        print("Ask it anything:")
        print(f'  hcom send "{at_name} what functions does {display_path} export?"')
        print(f'  hcom send "{at_name} where is error handling done?"')
        print()
        print(f"Stop: hcom stop {at_name}")

    return 0


def main():
    parser = argparse.ArgumentParser(
        description="""Launch or query a fat cow.

The fatcow agent reads every file in the specified path, memorizes structure
and line references, then sits in background answering questions from other
agents via hcom. Subscribes to file changes to stay current.

HOW IT WORKS:
  1. Reads ALL files in the target path (full reads, not skimming)
  2. Builds mental index of functions, types, exports, imports
  3. Subscribes to file edit/write events to stay current
  4. Answers questions from agents with file:line precision

MODES:
  Live (default): Stays running, subscribes to file changes, answers in real-time.
  Dead (--dead):  Ingests then stops. Resumed on demand via --ask.
                  Dead fatcows catch up on file changes before answering.

USE CASES:
  - Agent working on auth needs to know tool integration points
  - Agent refactoring needs to know all callers of a function
  - Agent debugging needs to understand data flow through a module
  - Any "where is X?" or "how does Y work?" about the module""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Ingest the tools module (live):
  hcom run fatcow --path src/tools

  # Ingest with focus area:
  hcom run fatcow --path src/ --focus "auth, middleware, permissions"

  # Interactive mode (see what it's doing):
  hcom run fatcow --path lib/api --interactive

  # Dead fatcow (ingest then stop):
  hcom run fatcow --path src/tools --dead

  # Query a fatcow (works for both live and dead):
  hcom run fatcow --ask fatcow.tools-luna "what does db.py export?"

  # Stop the fat cow:
  hcom stop @fatcow.tools
""",
    )

    # Launch flags
    parser.add_argument(
        "--path",
        help="Directory or file path to ingest (required for launch, not for --ask)",
    )
    parser.add_argument(
        "--focus", "-f",
        help="Comma-separated focus areas (e.g., 'auth, routing, middleware')",
    )
    parser.add_argument(
        "--dead", action="store_true",
        help="Ingest then stop. Query later with --ask",
    )
    parser.add_argument(
        "--interactive", "-i", action="store_true",
        help="Launch in interactive terminal instead of background",
    )
    parser.add_argument(
        "--tool", choices=["claude", "gemini"], default="claude",
        help="AI tool to use (default: claude)",
    )

    # Query flags
    parser.add_argument(
        "--ask", nargs=2, metavar=("FATCOW", "QUESTION"),
        help="Ask a fatcow a question (resumes dead fatcow automatically)",
    )
    parser.add_argument(
        "--timeout", type=int, default=120,
        help="Seconds to wait for --ask response (default: 120)",
    )

    # Identity
    parser.add_argument("--name", help="Your identity (optional)")

    args = parser.parse_args()

    # Dispatch
    if args.ask:
        return sys.exit(ask_fatcow(
            args.ask[0], args.ask[1],
            caller=args.name, timeout=args.timeout,
        ))

    if not args.path:
        parser.error("--path is required for launch (use --ask to query)")

    sys.exit(launch_fatcow(args, dead=args.dead))


if __name__ == "__main__":
    main()
