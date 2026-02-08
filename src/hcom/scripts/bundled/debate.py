#!/usr/bin/env python3
"""PRO/CON debaters (fresh or existing agents) + judge evaluate a topic in shared hcom thread.

Two modes:
1. --spawn: Launch fresh PRO/CON instances with debate-specific system prompts
2. --workers: Use existing instances, they pick their own positions dynamically
"""

import sys
import time
import argparse
from hcom.api import HcomError, instances, launch, session

# === SYSTEM PROMPTS (from Swarms debate_with_judge.py) ===

PRO_SYSTEM_PROMPT = """You are an expert debater arguing IN FAVOR of propositions.

Your Role:
- Present compelling, well-reasoned arguments supporting your assigned position
- Use evidence, logic, and persuasive rhetoric to make your case
- Anticipate and preemptively address potential counterarguments
- Build upon previous arguments when refining your position

Guidelines:
1. Structure your arguments clearly with main points and supporting evidence
2. Use concrete examples and data when available
3. Acknowledge valid opposing points while explaining why your position is stronger
4. Maintain a professional, respectful tone throughout
5. Focus on the strongest aspects of your position

You will see your opponent's arguments directly in the shared thread. Engage with them."""

CON_SYSTEM_PROMPT = """You are an expert debater arguing AGAINST propositions.

Your Role:
- Present compelling counter-arguments opposing the given position
- Identify weaknesses, flaws, and potential negative consequences
- Challenge assumptions and evidence presented by the opposing side
- Build upon previous arguments when refining your position

Guidelines:
1. Structure your counter-arguments clearly with main points and supporting evidence
2. Use concrete examples and data to support your opposition
3. Directly address and refute the Pro's arguments
4. Maintain a professional, respectful tone throughout
5. Focus on the most significant weaknesses of the opposing position

You will see your opponent's arguments directly in the shared thread. Engage with them."""

JUDGE_SYSTEM_PROMPT = """You are an impartial judge and moderator of debates.

Your Role:
- Coordinate the debate flow between debaters
- Objectively evaluate arguments from both sides
- Identify strengths and weaknesses in each position
- Provide feedback after each round to guide refinement
- Render fair verdicts based on argument quality, not personal bias

Evaluation Criteria:
1. Logical coherence and reasoning quality
2. Evidence and supporting data quality
3. Persuasiveness and rhetorical effectiveness
4. Responsiveness to opposing arguments
5. Overall argument structure and clarity

Be specific about what makes arguments strong or weak.
Provide actionable feedback for improvement between rounds."""


def main():
    parser = argparse.ArgumentParser(
        description="""Launch a structured debate between Claude instances.

Two modes:
  --spawn (-s): Launch fresh PRO and CON instances with debate-specific prompts
  --workers (-w): Use existing instances who pick their own positions dynamically

WHAT HAPPENS:
  1. Debaters are set up (spawned or notified)
  2. Judge coordinates: opening statements → rebuttals → verdict
  3. All messages use shared thread for visibility
  4. Judge announces winner with scores and analysis

USE WHEN:
  - Want multiple perspectives on a technical decision
  - Need to explore pros/cons of an approach
  - Testing ideas through adversarial argument
  - Generating comprehensive analysis via competition""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Spawn fresh PRO/CON debaters:
  hcom run debate "AI will replace programmers" --spawn

  # Use existing instances (they pick positions):
  hcom run debate "tabs vs spaces" -w sity,offu

  # With context from a transcript:
  hcom run debate "this approach is best" -w luna,nova --context luna:10-20

  # More rounds for deeper analysis:
  hcom run debate "microservices vs monolith" --spawn --rounds 4
""",
    )
    parser.add_argument("topic", help="Debate topic/proposition")

    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--spawn",
        "-s",
        action="store_true",
        help="Spawn fresh PRO/CON instances with debate system prompts",
    )
    mode.add_argument(
        "--workers",
        "-w",
        help="Use existing instances (comma-separated), they pick positions",
    )

    parser.add_argument(
        "--tool",
        choices=["claude", "gemini", "codex"],
        default="claude",
        help="AI tool to use for spawned instances (default: claude)",
    )
    parser.add_argument(
        "--rounds",
        "-r",
        type=int,
        default=2,
        help="Number of rebuttal rounds (default: 2)",
    )
    parser.add_argument(
        "--timeout",
        "-t",
        type=int,
        default=120,
        help="Response timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--context",
        "-c",
        help="Context for debate: text, code, 'instance:N-M' for transcript range, or filename",
    )
    parser.add_argument("--name", help="Your identity")
    parser.add_argument(
        "--interactive",
        "-i",
        action="store_true",
        help="Launch in terminal windows instead of background",
    )

    args = parser.parse_args()
    thread = f"debate-{int(time.time())}"

    # Get caller info
    try:
        session(name=args.name)
    except HcomError:
        pass  # fallback for external callers

    # === BUILD CONTEXT ===
    context_section = ""
    if args.context:
        context_section = f"""
CONTEXT (read before debating):
{args.context}

Debaters: Review this context first. Use `hcom transcript <instance> --range N-M` if referencing conversations."""

    if args.spawn:
        run_spawn_mode(args, thread, context_section)
    else:
        run_workers_mode(args, thread, context_section)


def run_spawn_mode(args, thread, context_section):
    """Spawn fresh PRO and CON instances with debate-specific system prompts."""

    print(f"Thread: {thread}")
    print(f"Topic: {args.topic}")
    print(f"Tool: {args.tool}")
    print("Mode: Spawning fresh PRO/CON debaters")
    print(f"Rounds: {args.rounds}")
    print()

    # Launch PRO debater
    pro_prompt = f"""You are the PRO debater in thread '{thread}'.
Topic: {args.topic}

Argue IN FAVOR of this proposition. A judge will coordinate the debate.
All messages use --thread {thread}. You can see your opponent's arguments directly.

Wait for the judge to start, then present your opening argument when prompted.
Use: hcom send "@judge- @con- [your argument]" --thread {thread} --intent inform
(The @prefix- pattern matches any instance with that prefix)"""

    pro_result = launch(
        1,
        tool=args.tool,
        tag="pro",
        prompt=pro_prompt,
        background=not args.interactive,
        system_prompt=PRO_SYSTEM_PROMPT,
        wait=True,
    )
    print(f"PRO debater launched ({args.tool}, batch: {pro_result['batch_id']})")

    # Launch CON debater
    con_prompt = f"""You are the CON debater in thread '{thread}'.
Topic: {args.topic}

Argue AGAINST this proposition. A judge will coordinate the debate.
All messages use --thread {thread}. You can see your opponent's arguments directly.

Wait for the judge to start, then present your argument when prompted.
Use: hcom send "@judge- @pro- [your argument]" --thread {thread} --intent inform
(The @prefix- pattern matches any instance with that prefix)"""

    con_result = launch(
        1,
        tool=args.tool,
        tag="con",
        prompt=con_prompt,
        background=not args.interactive,
        system_prompt=CON_SYSTEM_PROMPT,
        wait=True,
    )
    print(f"CON debater launched ({args.tool}, batch: {con_result['batch_id']})")

    # Extract actual instance names for SQL queries
    pro_name = pro_result["handles"][0]["instance_name"] if pro_result.get("handles") else None
    con_name = con_result["handles"][0]["instance_name"] if con_result.get("handles") else None

    # Launch judge with actual debater names
    launch_judge(
        args,
        thread,
        context_section,
        structured=True,
        pro_name=pro_name,
        con_name=con_name,
    )


def run_workers_mode(args, thread, context_section):
    """Use existing instances - they decide their own positions dynamically."""

    workers = [w.strip() for w in args.workers.split(",")]

    # Validate workers
    available = {i["name"]: i for i in instances()}
    errors = []
    for w in workers:
        if w not in available:
            errors.append(f"'{w}' not found")
        elif available[w]["status"] in ("inactive", "stale"):
            errors.append(f"'{w}' is {available[w]['status']}")

    if errors:
        print(f"Error: {'; '.join(errors)}", file=sys.stderr)
        if available:
            print(f"available: {', '.join(available.keys())}", file=sys.stderr)
        else:
            print("available: (none - use --spawn instead)", file=sys.stderr)
        sys.exit(1)

    if len(workers) < 2:
        print("Error: need at least 2 workers for a debate", file=sys.stderr)
        sys.exit(1)

    print(f"Thread: {thread}")
    print(f"Topic: {args.topic}")
    print("Mode: Existing workers (dynamic positions)")
    print(f"Debaters: {', '.join(workers)}")
    print(f"Rounds: {args.rounds}")
    print()

    # Prep debaters - they'll pick their own positions
    setup_session = session(name="debate-setup", external=True)
    for w in workers:
        setup_session.send(
            f"@{w} You will debate: '{args.topic}'. "
            f"Thread: '{thread}'. "
            f"When the judge asks for positions, decide if you're FOR or AGAINST. "
            f"You'll see your opponent's arguments directly. "
            f"Say 'ready' to confirm.",
            thread=thread,
            intent="request",
        )
    print(f"Sent prep to {len(workers)} debaters")

    # Launch judge with dynamic mode instructions
    launch_judge(args, thread, context_section, structured=False, all_workers=workers)


def launch_judge(
    args,
    thread,
    context_section,
    structured=True,
    all_workers=None,
    pro_name=None,
    con_name=None,
):
    """Launch the judge instance."""

    if structured:
        # PRO/CON are pre-assigned with known names
        position_instruction = f"""
DEBATERS:
  PRO: {pro_name} (use @pro- to address)
  CON: {con_name} (use @con- to address)

Positions are pre-assigned. PRO argues first in each round."""
        opening_instruction = f"""
2. OPENING STATEMENTS
   Ask PRO for opening argument (CC both debaters so they see each other):
   hcom send "@pro- @con- PRO: Present your opening argument IN FAVOR of: {args.topic}" --thread {thread} --intent request
   Wait: hcom events --wait {args.timeout} --sql "msg_thread='{thread}' AND msg_from='{pro_name}'"

   Then ask CON:
   hcom send "@pro- @con- CON: Present your opening argument AGAINST: {args.topic}. You can see PRO's argument above." --thread {thread} --intent request
   Wait: hcom events --wait {args.timeout} --sql "msg_thread='{thread}' AND msg_from='{con_name}'"
"""
    else:
        # Dynamic - debaters pick positions
        assert all_workers is not None, "all_workers required for dynamic mode"
        debaters_list = " ".join(f"@{w}" for w in all_workers)
        workers_sql = ", ".join(f"'{w}'" for w in all_workers)
        position_instruction = f"""
DEBATERS: {", ".join(all_workers)}

Positions are DYNAMIC - debaters decide their own stance (FOR or AGAINST).
Address all debaters together: {debaters_list}"""
        opening_instruction = f"""
2. OPENING STATEMENTS
   Ask all debaters to state their position and opening argument:
   hcom send "{debaters_list} State whether you are FOR or AGAINST: {args.topic}. \
Then give your opening argument (2-3 paragraphs). You can see each other's responses." --thread {thread} --intent request

   Wait for all {len(all_workers)} responses:
   hcom events --wait {args.timeout} --sql "msg_thread='{thread}' AND msg_from IN ({workers_sql})"

   Note who is arguing which side for the rebuttals.
"""

    judge_prompt = f"""You are the judge for a structured debate. Use hcom to coordinate.

THREAD: {thread}
TOPIC: {args.topic}
ROUNDS: {args.rounds}
TIMEOUT: {args.timeout}s per response
{position_instruction}
{context_section}

{JUDGE_SYSTEM_PROMPT}

PROCEDURE:

1. WAIT FOR READY
   Check for ready confirmations:
   hcom events --last 10 --sql "msg_thread='{thread}'"
{opening_instruction}
4. REBUTTALS ({args.rounds} rounds)
   For each round:
   - Prompt each debater to respond to their opponent's latest argument
   - Wait for responses
   - Provide brief feedback on both arguments

   Example (always CC both debaters):
   hcom send "@pro- @con- ROUND N: PRO, respond to CON's arguments above." --thread {thread} --intent request
   hcom send "@pro- @con- ROUND N: CON, respond to PRO's arguments above." --thread {thread} --intent request

5. FINAL JUDGMENT
   After all rounds, provide comprehensive evaluation (CC both debaters):

   hcom send "@pro- @con- VERDICT: [WINNER or TIE]

   PRO strengths: ...
   PRO weaknesses: ...

   CON strengths: ...
   CON weaknesses: ...

   Key deciding factor: ...
   Score: PRO X/10, CON Y/10

   Debate complete." --thread {thread} --intent inform

RULES:
- Always use --thread {thread}
- Debaters see each other directly in the shared thread
- Provide feedback after each round
- Stay neutral until final judgment
- Judge on argument quality, not personal agreement

Begin now."""

    result = launch(
        1,
        tool=args.tool,
        tag="judge",
        prompt=judge_prompt,
        background=not args.interactive,
        system_prompt=JUDGE_SYSTEM_PROMPT,
    )

    print(f"Judge launched ({args.tool}, batch: {result['batch_id']})")
    print()
    print("Watch debate:")
    print(f"  hcom events --wait 600 --sql \"msg_thread='{thread}'\"")
    print()
    print("Or in TUI:")
    print("  hcom")


if __name__ == "__main__":
    main()
