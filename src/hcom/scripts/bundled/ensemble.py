#!/usr/bin/env python3
"""Ensemble Refinement - multiple agents implement, see each other's work, refine iterativly with a judge.

Pattern:
  Round 1: Each agent writes independent implementation to file
  Round 2+: Each agent reads others' files, writes improved version
  Final: Judge picks winner → winner writes final.py

Workspace structure:
  ensemble-{timestamp}/
  ├── coder0/
  │   ├── round1.py
  │   ├── round2.py
  │   └── final.py  (if winner)
  ├── coder1/
  │   ├── round1.py
  │   └── round2.py
  └── coder2/
      ├── round1.py
      └── round2.py

This produces better code than a single agent because:
  - Multiple approaches explored in parallel
  - Agents learn from each other's edge cases, optimizations, patterns
  - Competition/collaboration drives quality up
  - Winner incorporates best ideas from all agents in final pass
  - Real files you can run, test, diff

Usage:
  hcom run ensemble "implement binary search"
  hcom run ensemble "write a rate limiter class" --rounds 3
  hcom run ensemble "implement LRU cache" --agents 4 --rounds 2
"""

import argparse
import time
import sys
import os
from hcom.api import HcomError, launch, session


# === AGENT STYLE PROMPTS ===

STYLES = [
    {
        "name": "simple",
        "prompt": """You write SIMPLE, READABLE code.
- Prioritize clarity over cleverness
- Use descriptive variable names
- Add comments for non-obvious logic
- Prefer straightforward algorithms even if slightly slower""",
    },
    {
        "name": "optimized",
        "prompt": """You write OPTIMIZED, PERFORMANT code.
- Prioritize speed and memory efficiency
- Use optimal data structures and algorithms
- Consider time/space complexity tradeoffs
- Minimize allocations and iterations""",
    },
    {
        "name": "defensive",
        "prompt": """You write DEFENSIVE, ROBUST code.
- Handle all edge cases explicitly
- Validate inputs thoroughly
- Use proper error handling
- Consider concurrent access, null values, bounds""",
    },
    {
        "name": "pythonic",
        "prompt": """You write IDIOMATIC, PYTHONIC code.
- Use Python's built-in features and stdlib
- Prefer list comprehensions, generators, context managers
- Follow PEP 8 and Python conventions
- Leverage duck typing and protocols""",
    },
    {
        "name": "tested",
        "prompt": """You write TESTABLE code with TESTS.
- Design for testability (dependency injection, pure functions)
- Include unit tests with your implementation
- Cover edge cases in tests
- Use clear test names that describe behavior""",
    },
]


CODER_SYSTEM_PROMPT = """You are an expert programmer participating in an ensemble coding session.

{style_prompt}

WORKSPACE: {workspace}
YOUR FOLDER: {my_folder}

RULES:
1. Write complete, working implementations (not pseudocode)
2. Include type hints and docstrings
3. Write your code to FILES, then notify via hcom with the file path
4. When you see others' code in later rounds, READ their files and learn from their approaches
5. In refinement rounds, you may:
   - Adopt better patterns you see
   - Fix bugs you notice in your approach
   - Combine ideas from multiple implementations
   - Keep your approach if you think it's best, but explain why

FILE STRUCTURE:
- Round 1: Write to {my_folder}/round1.py
- Round 2: Write to {my_folder}/round2.py
- etc.
- If you win: Write to {my_folder}/final.py

TO READ OTHERS' CODE:
- Read files like: {workspace}/coder0/round1.py, {workspace}/coder1/round1.py, etc.

IF YOU WIN:
The judge will announce you as the winner and ask you to write the FINAL implementation.
This is your chance to write the definitive version:
- Incorporate the best ideas the judge highlighted from other agents
- Keep what made your approach win
- Write to {my_folder}/final.py

COMMUNICATION:
After writing your file, notify everyone:
hcom send "@judge- [other coders] ROUND N done: {my_folder}/roundN.py" --thread {{thread}}

Keep messages short - the code is in the file."""


JUDGE_SYSTEM_PROMPT = """You are the judge for an ensemble coding session.

Your job:
1. Coordinate the rounds (announce when each round starts)
2. Wait for all implementations each round
3. After final round, evaluate all submissions

EVALUATION CRITERIA (weighted):
1. Correctness (40%) - Does it work? Edge cases handled?
2. Code Quality (25%) - Readable, maintainable, idiomatic?
3. Performance (20%) - Time/space complexity, efficiency?
4. Robustness (15%) - Error handling, defensive coding?

Be specific about strengths and weaknesses. Quote code when relevant."""


def build_coder_prompt(
    task: str,
    style: dict,
    thread: str,
    agent_mentions: str,
    round_num: int,
    total_rounds: int,
    workspace: str,
    my_folder: str,
    all_folders: list,
) -> str:
    """Build the initial prompt for a coder agent."""
    other_folders = [f for f in all_folders if f != my_folder]
    other_folders_str = ", ".join(other_folders)

    return f"""TASK: {task}

THREAD: {thread}
ROUND: {round_num}/{total_rounds}

{"This is round 1 - implement independently. Do NOT wait for others." if round_num == 1 else f"This is a refinement round. READ others' code first: {other_folders_str}"}

STEPS:
1. {"" if round_num == 1 else f"Read others' round{round_num - 1}.py files from: {other_folders_str}"}
2. Write your implementation to: {my_folder}/round{round_num}.py
3. Notify everyone:
   hcom send "{agent_mentions} ROUND {round_num} done: {my_folder}/round{round_num}.py" --thread {thread} --intent inform

{"Begin implementing now." if round_num == 1 else "Read others' files, learn from them, then write your improved version."}"""


def build_judge_prompt(
    task: str,
    thread: str,
    num_agents: int,
    num_rounds: int,
    parent_name: str,
    agent_mentions: str,
    workspace: str,
    all_folders: list,
    timeout: int,
) -> str:
    """Build the prompt for the judge agent."""
    folders_str = ", ".join(all_folders)

    return f"""ENSEMBLE CODING SESSION
Task: {task}
Thread: {thread}
Workspace: {workspace}
Agent folders: {folders_str}
Rounds: {num_rounds}

PROCEDURE:

1. ROUND 1 - Wait for all {num_agents} initial implementations:
   hcom events --wait {timeout} --sql "msg_thread='{thread}' AND msg_text LIKE '%ROUND 1 done%'"

   When you have {num_agents} notifications, read all round1.py files and announce round 2.

2. ROUNDS 2-{num_rounds} - For each refinement round:
   Announce the round:
   hcom send "{agent_mentions} ROUND N: Read each other's code, then write your improved version to roundN.py" --thread {thread} --intent request

   Wait for all {num_agents} "ROUND N done" messages before moving to next round.

3. EVALUATION - After round {num_rounds}:
   Read all round{num_rounds}.py files from: {folders_str}

   For each agent:
   - Correctness score (0-40)
   - Code quality score (0-25)
   - Performance score (0-20)
   - Robustness score (0-15)
   - Total score (0-100)
   - Key strengths
   - Key weaknesses

4. ANNOUNCE WINNER & REQUEST FINAL:
   hcom send "{agent_mentions}

   EVALUATION COMPLETE

   RANKINGS:
   1. [winner]: [score]/100 - [strengths]
   2. [agent]: [score]/100 - [strengths]
   3. [agent]: [score]/100 - [strengths]

   WINNER: @[winner agent name]-

   You have proven yourself. Now write the FINAL implementation.
   Write to: [winner's folder]/final.py

   Incorporate the best ideas from your competitors:
   - From [agent2]: [what to take, cite their file]
   - From [agent3]: [what to take, cite their file]

   This is your chance to write the definitive version.
   Notify when done: 'FINAL done: [path]/final.py'
   " --thread {thread} --intent request

   Wait for winner's final:
   hcom events --wait {timeout} --sql "msg_thread='{thread}' AND msg_text LIKE '%FINAL done%'"

5. DELIVER TO PARENT:
   Read the winner's final.py file, then send:

   hcom send "@{parent_name}
   ENSEMBLE COMPLETE

   Task: {task}
   Workspace: {workspace}
   Agents: {num_agents}
   Rounds: {num_rounds}

   WINNER: [agent name] ([score]/100)

   RANKINGS:
   1. [agent]: [score] - [one-line summary]
   2. [agent]: [score] - [one-line summary]
   3. [agent]: [score] - [one-line summary]

   FINAL: [winner's folder]/final.py

   WHAT MADE IT WIN:
   [Brief explanation of what the winner did well and what they incorporated from others]
   " --thread {thread} --intent inform

Begin by waiting for round 1 implementations."""


def main():
    parser = argparse.ArgumentParser(
        description="""Ensemble Refinement - multiple agents implement and refine code in parallel.

Each agent has a distinct coding style (simple, optimized, defensive, pythonic, tested).
They work in rounds: implement independently → see each other's code → refine.
A judge picks the winner who writes the final implementation.

WHAT HAPPENS:
  1. Creates workspace: ensemble-{timestamp}/ with subfolders per agent
  2. Round 1: Each agent writes independent implementation to coder{N}/round1.py
  3. Round 2+: Agents read each other's files, write improved versions
  4. Judge evaluates: correctness (40%), quality (25%), performance (20%), robustness (15%)
  5. Winner writes coder{N}/final.py incorporating best ideas from all

USE WHEN:
  - Want multiple approaches to a coding problem
  - Need high-quality implementation through competition
  - Exploring different tradeoffs (speed vs readability vs robustness)
  - Generating code that benefits from cross-pollination of ideas

OUTPUT:
  - Workspace folder with all implementations
  - Final verdict sent to caller via hcom message
  - Winner's final.py as the definitive solution""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Basic usage (3 agents, 2 rounds):
  hcom run ensemble "implement binary search"

  # More rounds for better refinement:
  hcom run ensemble "write a rate limiter" --rounds 3

  # More agents for more diversity:
  hcom run ensemble "implement LRU cache" --agents 4

  # Custom workspace location:
  hcom run ensemble "implement merge sort" --dir ./experiments

  # Without judge (just parallel implementations):
  hcom run ensemble "implement quicksort" --no-judge
""",
    )
    parser.add_argument("task", help="What to implement (be specific)")
    parser.add_argument("--name", help="Your identity")
    parser.add_argument(
        "--agents",
        "-a",
        type=int,
        default=3,
        help="Number of coding agents, each with different style (default: 3, max: 5)",
    )
    parser.add_argument(
        "--rounds",
        "-r",
        type=int,
        default=2,
        help="Number of rounds - more = better refinement (default: 2)",
    )
    parser.add_argument(
        "--timeout",
        "-t",
        type=int,
        default=180,
        help="Timeout per round in seconds for judge waits (default: 180)",
    )
    parser.add_argument(
        "--no-judge",
        action="store_true",
        help="Skip judge - just run coders in parallel (no evaluation)",
    )
    parser.add_argument(
        "--dir",
        "-d",
        default=".",
        help="Parent directory for workspace folder (default: current)",
    )
    parser.add_argument(
        "--tool",
        choices=["claude", "gemini", "codex", "opencode"],
        default="claude",
        help="AI tool to use for agents (default: claude)",
    )
    parser.add_argument(
        "--interactive",
        "-i",
        action="store_true",
        help="Launch in terminal windows instead of background",
    )

    args = parser.parse_args()

    # Validate
    if args.agents < 2:
        print("Error: need at least 2 agents for ensemble", file=sys.stderr)
        sys.exit(1)
    if args.agents > len(STYLES):
        print(
            f"Error: max {len(STYLES)} agents (limited by coding styles)",
            file=sys.stderr,
        )
        sys.exit(1)
    if args.rounds < 1:
        print("Error: need at least 1 round", file=sys.stderr)
        sys.exit(1)

    # Get caller session
    try:
        s = session(name=args.name)
    except HcomError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

    parent_name = s.name
    ts = int(time.time())
    thread = f"ensemble-{ts}"

    # Create workspace folder
    workspace = os.path.abspath(os.path.join(args.dir, f"ensemble-{ts}"))
    os.makedirs(workspace, exist_ok=True)

    # Create coder folders
    agent_names = [f"coder{i}" for i in range(args.agents)]
    all_folders = []
    for name in agent_names:
        folder = os.path.join(workspace, name)
        os.makedirs(folder, exist_ok=True)
        all_folders.append(folder)

    agent_mentions = " ".join(f"@{name}-" for name in agent_names)

    print(f"{'=' * 60}")
    print("ENSEMBLE REFINEMENT")
    print(f"{'=' * 60}")
    print(f"Task: {args.task}")
    print(f"Thread: {thread}")
    print(f"Tool: {args.tool}")
    print(f"Workspace: {workspace}")
    print(f"Agents: {args.agents}")
    print(f"Rounds: {args.rounds}")
    print(f"{'=' * 60}")
    print()
    print("Folders created:")
    for folder in all_folders:
        print(f"  {folder}/")
    print()

    # Launch coders + judge, cleaning up on failure
    launched_names = []
    try:
        for i in range(args.agents):
            style = STYLES[i]
            my_folder = all_folders[i]
            system_prompt = CODER_SYSTEM_PROMPT.format(
                style_prompt=style["prompt"],
                workspace=workspace,
                my_folder=my_folder,
            )

            is_last_coder = i == args.agents - 1
            result = launch(
                1,
                tool=args.tool,
                tag=f"coder{i}",
                prompt=build_coder_prompt(
                    task=args.task,
                    style=style,
                    thread=thread,
                    agent_mentions=agent_mentions,
                    round_num=1,
                    total_rounds=args.rounds,
                    workspace=workspace,
                    my_folder=my_folder,
                    all_folders=all_folders,
                ),
                background=not args.interactive,
                system_prompt=system_prompt,
                cwd=workspace,
                wait=is_last_coder,
            )
            for h in result.get("handles", []):
                launched_names.append(h["instance_name"])
            print(f"  Launched coder{i} ({style['name']} style, {args.tool}) → {my_folder}/")

        # Launch judge
        if not args.no_judge:
            result = launch(
                1,
                tool=args.tool,
                tag="judge",
                prompt=build_judge_prompt(
                    task=args.task,
                    thread=thread,
                    num_agents=args.agents,
                    num_rounds=args.rounds,
                    parent_name=parent_name,
                    agent_mentions=agent_mentions,
                    workspace=workspace,
                    all_folders=all_folders,
                    timeout=args.timeout,
                ),
                background=not args.interactive,
                system_prompt=JUDGE_SYSTEM_PROMPT,
                cwd=workspace,
            )
            for h in result.get("handles", []):
                launched_names.append(h["instance_name"])
            print(f"  Launched judge ({args.tool})")
    except Exception as e:
        print(f"\nError during launch: {e}", file=sys.stderr)
        if launched_names:
            print(f"Cleaning up {len(launched_names)} launched agents...", file=sys.stderr)
            from hcom.core.ops import op_stop
            for name in launched_names:
                try:
                    op_stop(name, reason="launch_failure")
                except Exception:
                    pass
        sys.exit(1)

    print()
    print(f"{'=' * 60}")
    print("WATCHING")
    print(f"{'=' * 60}")
    print(f"  Live:    hcom events --wait 600 --sql \"msg_thread='{thread}'\"")
    print("  TUI:     hcom")
    print(f"  Thread:  {thread}")
    print(f"  Files:   ls -la {workspace}/*/")
    print()
    print("Verdict will be sent to you when complete.")
    print(f"Final implementation: {workspace}/coder*/final.py")


if __name__ == "__main__":
    main()
