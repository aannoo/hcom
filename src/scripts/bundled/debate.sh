#!/usr/bin/env bash
# PRO/CON debaters (fresh or existing agents) + judge evaluate a topic in shared hcom thread.
#
# Two modes:
# 1. --spawn: Launch fresh PRO/CON instances with debate-specific system prompts
# 2. --workers: Use existing instances, they pick their own positions dynamically
#
# Usage:
#   hcom run debate "AI will replace programmers" --spawn
#   hcom run debate "tabs vs spaces" -w sity,offu
#   hcom run debate "microservices vs monolith" --spawn --rounds 4

set -euo pipefail

# Cleanup trap for launched agents
LAUNCHED_NAMES=()
cleanup() {
  if [[ ${#LAUNCHED_NAMES[@]} -gt 0 ]]; then
    echo "Cleaning up ${#LAUNCHED_NAMES[@]} launched agents..." >&2
    for name in "${LAUNCHED_NAMES[@]}"; do
      hcom stop "$name" --go 2>/dev/null || true
    done
  fi
}
track_launch() {
  local output="$1"
  local names
  names=$(echo "$output" | grep '^Names: ' | sed 's/^Names: //' | tr ',' '\n' | xargs)
  for n in $names; do
    LAUNCHED_NAMES+=("$n")
  done
}

usage() {
  cat <<'EOF'
Usage: hcom run debate [OPTIONS] TOPIC

Launch a structured debate between AI instances.

Two modes:
  --spawn (-s): Launch fresh PRO and CON instances with debate-specific prompts
  --workers (-w): Use existing instances who pick their own positions dynamically

Arguments:
  TOPIC                   Debate topic/proposition

Options:
  -s, --spawn             Spawn fresh PRO/CON instances
  -w, --workers NAMES     Use existing instances (comma-separated)
  --tool TOOL             AI tool for spawned instances (default: claude)
  --rounds, -r N          Number of rebuttal rounds (default: 2)
  --timeout, -t N         Response timeout in seconds (default: 120)
  --context, -c TEXT      Context for debate
  --name NAME             Your identity
  -i, --interactive       Launch in terminal windows instead of background
  -h, --help              Show this help

Examples:
  hcom run debate "AI will replace programmers" --spawn
  hcom run debate "tabs vs spaces" -w sity,offu
  hcom run debate "microservices vs monolith" --spawn --rounds 4
EOF
  exit 0
}

# Parse args
topic=""
spawn=false
workers=""
tool="claude"
rounds=2
timeout=120
context=""
name_flag=""
interactive=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage ;;
    -s|--spawn) spawn=true; shift ;;
    -w|--workers) workers="$2"; shift 2 ;;
    --tool) tool="$2"; shift 2 ;;
    -r|--rounds) rounds="$2"; shift 2 ;;
    -t|--timeout) timeout="$2"; shift 2 ;;
    -c|--context) context="$2"; shift 2 ;;
    --name) name_flag="$2"; shift 2 ;;
    -i|--interactive) interactive=true; shift ;;
    -*) echo "Error: unknown option: $1" >&2; exit 1 ;;
    *) topic="$1"; shift ;;
  esac
done

if [[ -z "$topic" ]]; then
  echo "Error: TOPIC argument required" >&2
  exit 1
fi

if [[ "$spawn" == "false" && -z "$workers" ]]; then
  echo "Error: must specify --spawn or --workers" >&2
  exit 1
fi

if [[ "$spawn" == "true" && -n "$workers" ]]; then
  echo "Error: --spawn and --workers are mutually exclusive" >&2
  exit 1
fi

thread="debate-$(date +%s)"
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"

# System prompts
PRO_SYSTEM='You are an expert debater arguing IN FAVOR of propositions.

Your Role:
- Present compelling, well-reasoned arguments supporting your assigned position
- Use evidence, logic, and persuasive rhetoric to make your case
- Anticipate and preemptively address potential counterarguments

Guidelines:
1. Structure your arguments clearly with main points and supporting evidence
2. Use concrete examples and data when available
3. Acknowledge valid opposing points while explaining why your position is stronger
4. Maintain a professional, respectful tone throughout

You will see your opponent'"'"'s arguments directly in the shared thread. Engage with them.'

CON_SYSTEM='You are an expert debater arguing AGAINST propositions.

Your Role:
- Present compelling counter-arguments opposing the given position
- Identify weaknesses, flaws, and potential negative consequences
- Challenge assumptions and evidence presented by the opposing side

Guidelines:
1. Structure your counter-arguments clearly with main points and supporting evidence
2. Use concrete examples and data to support your opposition
3. Directly address and refute the Pro'"'"'s arguments
4. Maintain a professional, respectful tone throughout

You will see your opponent'"'"'s arguments directly in the shared thread. Engage with them.'

JUDGE_SYSTEM='You are an impartial judge and moderator of debates.

Your Role:
- Coordinate the debate flow between debaters
- Objectively evaluate arguments from both sides
- Provide feedback after each round to guide refinement
- Render fair verdicts based on argument quality, not personal bias

Evaluation Criteria:
1. Logical coherence and reasoning quality
2. Evidence and supporting data quality
3. Persuasiveness and rhetorical effectiveness
4. Responsiveness to opposing arguments
5. Overall argument structure and clarity

Be specific about what makes arguments strong or weak.'

# Build context section
context_section=""
if [[ -n "$context" ]]; then
  context_section="
CONTEXT (read before debating):
${context}

Debaters: Review this context first."
fi

# Set trap
trap cleanup ERR INT TERM

bg_flag=""
if [[ "$interactive" == "false" ]]; then
  # claude: explicit -p so --headless goes through print mode
  if [[ "$tool" == "claude" ]]; then
    bg_flag="--headless -p"
  else
    bg_flag="--headless"
  fi
fi

if [[ "$spawn" == "true" ]]; then
  # --- SPAWN MODE ---
  echo "Thread: $thread"
  echo "Topic: $topic"
  echo "Tool: $tool"
  echo "Mode: Spawning fresh PRO/CON debaters"
  echo "Rounds: $rounds"
  echo

  # Launch PRO
  pro_prompt="You are the PRO debater in thread '${thread}'.
Topic: ${topic}

Argue IN FAVOR of this proposition. A judge will coordinate the debate.
All messages use --thread ${thread}. You can see your opponent's arguments directly.

Wait for the judge to start, then present your opening argument when prompted.
Use: hcom send \"@judge- @con- [your argument]\" --thread ${thread} --intent inform"

  launch_out=$(hcom 1 "$tool" --tag pro --go \
    --hcom-system-prompt "$PRO_SYSTEM" \
    --hcom-prompt "$pro_prompt" \
    ${bg_flag} 2>&1)
  track_launch "$launch_out"

  echo "PRO debater launched ($tool)"

  # Launch CON
  con_prompt="You are the CON debater in thread '${thread}'.
Topic: ${topic}

Argue AGAINST this proposition. A judge will coordinate the debate.
All messages use --thread ${thread}. You can see your opponent's arguments directly.

Wait for the judge to start, then present your argument when prompted.
Use: hcom send \"@judge- @pro- [your argument]\" --thread ${thread} --intent inform"

  launch_out=$(hcom 1 "$tool" --tag con --go \
    --hcom-system-prompt "$CON_SYSTEM" \
    --hcom-prompt "$con_prompt" \
    ${bg_flag} 2>&1)
  track_launch "$launch_out"

  echo "CON debater launched ($tool)"

  # Judge prompt for spawn mode (structured, PRO/CON pre-assigned)
  judge_prompt="You are the judge for a structured debate. Use hcom to coordinate.

THREAD: ${thread}
TOPIC: ${topic}
ROUNDS: ${rounds}
TIMEOUT: ${timeout}s per response

DEBATERS:
  PRO: use @pro- to address
  CON: use @con- to address

Positions are pre-assigned. PRO argues first in each round.
${context_section}

${JUDGE_SYSTEM}

PROCEDURE:

1. WAIT FOR READY
   Check for ready confirmations:
   hcom events --last 10 --sql \"msg_thread='${thread}'\"

2. OPENING STATEMENTS
   Ask PRO for opening argument (CC both debaters):
   hcom send \"@pro- @con- PRO: Present your opening argument IN FAVOR of: ${topic}\" --thread ${thread} --intent request
   Wait: hcom events --wait ${timeout} --sql \"msg_thread='${thread}'\"

   Then ask CON:
   hcom send \"@pro- @con- CON: Present your opening argument AGAINST: ${topic}. You can see PRO's argument above.\" --thread ${thread} --intent request
   Wait: hcom events --wait ${timeout} --sql \"msg_thread='${thread}'\"

3. REBUTTALS (${rounds} rounds)
   For each round:
   - Prompt each debater to respond to opponent's latest argument (CC both)
   - Wait for responses
   - Provide brief feedback

4. FINAL JUDGMENT
   hcom send \"@pro- @con- VERDICT: [WINNER or TIE]

   PRO strengths: ...
   PRO weaknesses: ...
   CON strengths: ...
   CON weaknesses: ...
   Key deciding factor: ...
   Score: PRO X/10, CON Y/10

   Debate complete.\" --thread ${thread} --intent inform

RULES:
- Always use --thread ${thread}
- Stay neutral until final judgment

Begin now."

else
  # --- WORKERS MODE ---
  IFS=',' read -ra worker_arr <<< "$workers"

  if [[ ${#worker_arr[@]} -lt 2 ]]; then
    echo "Error: need at least 2 workers for a debate" >&2
    exit 1
  fi

  # Validate workers exist
  for w in "${worker_arr[@]}"; do
    w=$(echo "$w" | xargs)  # trim
    hcom list "$w" --json $name_arg >/dev/null 2>&1 || {
      echo "Error: instance '$w' not found" >&2
      exit 1
    }
  done

  echo "Thread: $thread"
  echo "Topic: $topic"
  echo "Mode: Existing workers (dynamic positions)"
  echo "Debaters: ${workers}"
  echo "Rounds: $rounds"
  echo

  # Prep debaters
  debaters_mentions=""
  workers_sql=""
  for w in "${worker_arr[@]}"; do
    w=$(echo "$w" | xargs)
    debaters_mentions="${debaters_mentions} @${w}"
    [[ -n "$workers_sql" ]] && workers_sql="${workers_sql}, "
    workers_sql="${workers_sql}'${w}'"
  done
  debaters_mentions=$(echo "$debaters_mentions" | xargs)

  for w in "${worker_arr[@]}"; do
    w=$(echo "$w" | xargs)
    hcom send "@${w}" --thread "${thread}" --intent request $name_arg -- \
      "You will debate: '${topic}'. Thread: '${thread}'. When the judge asks for positions, decide if you're FOR or AGAINST. Say 'ready' to confirm." 2>/dev/null
  done
  echo "Sent prep to ${#worker_arr[@]} debaters"

  # Judge prompt for workers mode (dynamic positions)
  judge_prompt="You are the judge for a structured debate. Use hcom to coordinate.

THREAD: ${thread}
TOPIC: ${topic}
ROUNDS: ${rounds}
TIMEOUT: ${timeout}s per response

DEBATERS: ${workers}

Positions are DYNAMIC - debaters decide their own stance (FOR or AGAINST).
Address all debaters together: ${debaters_mentions}
${context_section}

${JUDGE_SYSTEM}

PROCEDURE:

1. WAIT FOR READY
   Check for ready confirmations:
   hcom events --last 10 --sql \"msg_thread='${thread}'\"

2. OPENING STATEMENTS
   Ask all debaters to state their position and opening argument:
   hcom send \"${debaters_mentions} State whether you are FOR or AGAINST: ${topic}. Then give your opening argument (2-3 paragraphs).\" --thread ${thread} --intent request

   Wait for all ${#worker_arr[@]} responses:
   hcom events --wait ${timeout} --sql \"msg_thread='${thread}' AND msg_from IN (${workers_sql})\"

3. REBUTTALS (${rounds} rounds)
   For each round, prompt each debater to respond to opponents.

4. FINAL JUDGMENT
   Announce winner with scores, send to all debaters via --thread ${thread}.

RULES:
- Always use --thread ${thread}
- Stay neutral until final judgment

Begin now."
fi

# Launch judge
launch_out=$(hcom 1 "$tool" --tag judge --go \
  --hcom-system-prompt "$JUDGE_SYSTEM" \
  --hcom-prompt "$judge_prompt" \
  ${bg_flag} 2>&1)
track_launch "$launch_out"

echo "Judge launched ($tool)"

# Clear trap
trap - ERR INT TERM

echo
echo "Watch debate:"
echo "  hcom events --wait 600 --sql \"msg_thread='${thread}'\""
echo
echo "Or in TUI:"
echo "  hcom"
