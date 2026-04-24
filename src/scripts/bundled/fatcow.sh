#!/usr/bin/env bash
# Fat cow - a fat agent that deeply reads a module and answers questions on demand.
#
# Launches a headless Claude that gorges on a codebase module, memorizing files
# with line references. Sits in background answering questions from other agents
# via hcom. Subscribes to file changes to stay current.
#
# Two modes:
#   Live (default): Stays running, answers in real-time, tracks file changes.
#   Dead (--dead):  Ingests then stops. Resumed on demand via --ask.
#
# Usage:
#   hcom run fatcow --path src/tools                          # live fatcow
#   hcom run fatcow --path src/tools --dead                   # dead fatcow
#   hcom run fatcow --ask fatcow.tools-luna "what does db.py export?"  # query
#   hcom run fatcow --path src/ --focus "auth, middleware"           # with focus
#   hcom stop @fatcow.tools                                         # kill by tag

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: hcom run fatcow [OPTIONS]

Launch or query a fat cow - a dedicated codebase oracle.

The fatcow agent reads every file in the specified path, memorizes structure
and line references, then sits in background answering questions from other
agents via hcom. Subscribes to file changes to stay current.

MODES:
  Live (default): Stays running, subscribes to file changes, answers in real-time.
  Dead (--dead):  Ingests then stops. Resumed on demand via --ask.

Launch Options:
  --path PATH             Directory or file path to ingest (required for launch)
  --focus, -f TEXT        Comma-separated focus areas
  --dead                  Ingest then stop. Query later with --ask
  -i, --interactive       Launch in interactive terminal
  --tool TOOL             AI tool to use (default: claude)

Query Options:
  --ask FATCOW QUESTION   Ask a fatcow a question (resumes dead fatcow automatically)
  --timeout N             Seconds to wait for --ask response (default: 120)

Identity:
  --name NAME             Your identity (optional)

  -h, --help              Show this help

Examples:
  hcom run fatcow --path src/tools
  hcom run fatcow --path src/ --focus "auth, middleware"
  hcom run fatcow --path src/tools --dead
  hcom run fatcow --ask fatcow.tools-luna "what does db.py export?"
  hcom stop @fatcow.tools
EOF
  exit 0
}

# Parse args
path=""
focus=""
dead=false
interactive=false
tool="claude"
ask_name=""
ask_question=""
timeout=120
name_flag=""

# Manual parse because --ask takes 2 positional args
args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
  case "${args[$i]}" in
    -h|--help) usage ;;
    --path) i=$(( i + 1 )); path="${args[$i]}"; i=$(( i + 1 )) ;;
    -f|--focus) i=$(( i + 1 )); focus="${args[$i]}"; i=$(( i + 1 )) ;;
    --dead) dead=true; i=$(( i + 1 )) ;;
    -i|--interactive) interactive=true; i=$(( i + 1 )) ;;
    --tool) i=$(( i + 1 )); tool="${args[$i]}"; i=$(( i + 1 )) ;;
    --ask) i=$(( i + 1 )); ask_name="${args[$i]}"; i=$(( i + 1 )); ask_question="${args[$i]}"; i=$(( i + 1 )) ;;
    --timeout) i=$(( i + 1 )); timeout="${args[$i]}"; i=$(( i + 1 )) ;;
    --name) i=$(( i + 1 )); name_flag="${args[$i]}"; i=$(( i + 1 )) ;;
    -*) echo "Error: unknown option: ${args[$i]}" >&2; exit 1 ;;
    *) echo "Error: unexpected argument: ${args[$i]}" >&2; exit 1 ;;
  esac
done

name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"

# --- Helper: resolve caller name ---
resolve_caller() {
  local caller
  caller=$(hcom list self --json $name_arg 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['name'])" 2>/dev/null) || caller="fatcow-q"
  echo "$caller"
}

# --- Helper: check if instance is active ---
is_active() {
  hcom list "$1" --json $name_arg >/dev/null 2>&1
}

# --- ASK MODE ---
if [[ -n "$ask_name" ]]; then
  caller_name=$(resolve_caller)
  has_identity=true
  [[ "$caller_name" == "fatcow-q" ]] && has_identity=false

  # Live fatcow — just send the question
  if is_active "$ask_name"; then
    hcom send "@${ask_name}" $name_arg -- "$ask_question" 2>/dev/null
    if [[ "$has_identity" == "true" ]]; then
      echo "Asked ${ask_name} — answer will arrive via hcom"
      exit 0
    fi
    echo "Sent to live fatcow ${ask_name}, waiting for reply..."
    # Wait for reply
    event=$(hcom events --wait "$timeout" --sql "type='message' AND msg_from='${ask_name}'" --json $name_arg 2>/dev/null) || true
    if [[ -n "$event" ]]; then
      echo "$event" | python3 -c "import sys,json; data=json.load(sys.stdin); print(data.get('data',{}).get('text',''))" 2>/dev/null
      exit 0
    else
      echo "Fatcow did not respond within ${timeout}s" >&2
      exit 1
    fi
  fi

  # Dead fatcow — resume it
  # Get stopped snapshot
  stopped_json=$(hcom events --sql "type='life' AND instance='${ask_name}' AND json_extract(data, '$.action')='stopped'" --last 1 --json $name_arg 2>/dev/null) || true
  if [[ -z "$stopped_json" ]]; then
    echo "Error: '${ask_name}' not found (no stopped snapshot)" >&2
    exit 1
  fi

  fatcow_tool=$(echo "$stopped_json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d[0].get('data',{}).get('snapshot',{}).get('tool','claude') if isinstance(d,list) else d.get('data',{}).get('snapshot',{}).get('tool','claude'))" 2>/dev/null) || fatcow_tool="claude"

  # Build resume prompt
  resume_prompt="## QUESTION FROM @${caller_name}

${ask_question}

## INSTRUCTIONS

1. Answer with file:line precision.
2. Send your answer: hcom send @${caller_name} -- <your answer>
3. Stop yourself: run hcom stop"

  # Resume (claude: explicit -p so --headless goes through print mode)
  claude_p_flag=""
  [[ "$fatcow_tool" == "claude" ]] && claude_p_flag="-p"
  hcom 1 "$fatcow_tool" --go \
    --resume "$ask_name" \
    --hcom-prompt "$resume_prompt" \
    --headless ${claude_p_flag} >/dev/null 2>&1 || {
    echo "Error: Resume failed" >&2
    exit 1
  }

  if [[ "$has_identity" == "true" ]]; then
    echo "Resumed ${ask_name} — answer will arrive via hcom"
    exit 0
  fi

  echo "Resumed ${ask_name}, waiting for answer..."
  event=$(hcom events --wait "$timeout" --sql "type='message' AND msg_from='${ask_name}'" --json $name_arg 2>/dev/null) || true
  if [[ -n "$event" ]]; then
    echo "$event" | python3 -c "import sys,json; data=json.load(sys.stdin); print(data.get('data',{}).get('text',''))" 2>/dev/null
    exit 0
  else
    echo "Fatcow did not respond within ${timeout}s" >&2
    exit 1
  fi
fi

# --- LAUNCH MODE ---
if [[ -z "$path" ]]; then
  echo "Error: --path is required for launch (use --ask to query)" >&2
  exit 1
fi

target_path=$(cd "$(dirname "$path")" && pwd)/$(basename "$path")
if [[ ! -e "$target_path" ]]; then
  echo "Error: path '$path' does not exist" >&2
  exit 1
fi

if [[ -f "$target_path" ]]; then
  cwd=$(dirname "$target_path")
else
  cwd="$target_path"
fi

display_path="$path"

# Build tag from path
basename_clean=$(basename "${target_path%/}")
# Strip non-alphanumeric (except dots/underscores), replace dashes with dots
tag_suffix=$(echo "$basename_clean" | sed 's/-/./g' | sed 's/[^a-zA-Z0-9._]//g' | cut -c1-20)
if [[ -z "$tag_suffix" ]]; then
  tag="fatcow"
else
  tag="fatcow.${tag_suffix}"
fi

# File glob for subscriptions
if [[ -d "$target_path" ]]; then
  file_glob="${target_path}/*"
else
  file_glob="$target_path"
fi

# Resolve who to notify
notify=$(resolve_caller)
[[ "$notify" == "fatcow-q" ]] && notify="bigboss"

# Focus section
focus_section=""
if [[ -n "$focus" ]]; then
  focus_section="
## FOCUS AREAS
Pay special attention to: ${focus}
When reading files, prioritize understanding these aspects deeply. But still read everything."
fi

ingest_section="## PHASE 1: INGEST

1. First, get the lay of the land:
   - List all files recursively in \`${display_path}\`
   - Count them. You need to read ALL of them.

2. Read every file. Use the Read tool on each one. Do not skip any file.
   - For large files, read them in chunks if needed, but read the WHOLE file.
   - As you read, note key structures: functions, classes, types, exports, imports.
   - Track line numbers for important definitions.

3. After reading all files, do a second pass on the most important/complex files to solidify your understanding."

if [[ "$dead" == "true" ]]; then
  system_prompt='You are a fat cow - a dedicated codebase oracle.

You ingest a section of the codebase deeply, then stop. You are resumed on demand to answer questions.

## INGESTION

Read EVERY file in your assigned path. Not skimming - full reads. Understand structure, exports, imports, types, functions, classes, constants, error handling, edge cases. Know where things are by file and line number.

## ANSWERING

- Specific file paths and line numbers (e.g., `src/tools/auth.ts:42`)
- Exact function signatures, not approximations
- Actual code patterns, not summaries
- If outside your scope, say so immediately. Never guess.

## CONSTRAINTS

- **Read-only**: Never modify files.
- **Stay loaded**: Do not summarize away details.'

  launch_prompt="You are a fat cow for: \`${display_path}\`
${focus_section}
${ingest_section}

## PHASE 2: CONFIRM & STOP

4. Summarize what you indexed: file count, key modules, major exports/functions.

5. Stop yourself: run \`hcom stop\`

Do NOT subscribe to events. Do NOT wait for questions. Summarize, then stop."

  # claude: explicit -p so --headless goes through print mode
  if [[ "$tool" == "claude" ]]; then
    bg_flag="--headless -p"
  else
    bg_flag="--headless"
  fi

else
  system_prompt='You are a fat cow - a dedicated codebase oracle.

Your sole purpose is to deeply read and internalize a section of the codebase, then sit in background answering questions from other agents instantly. You are a living index.

## INGESTION

Read EVERY file in your assigned path. Not skimming - full reads. Understand structure, exports, imports, types, functions, classes, constants, error handling, edge cases. Know where things are by file and line number.

After reading, build a mental map: what each file does, all exported signatures, key constants, integration points, error patterns, data flow.

## ANSWERING

- Specific file paths and line numbers (e.g., `src/tools/auth.ts:42`)
- Exact function signatures, not approximations
- Actual code patterns, not summaries
- Cross-references within the module
- If outside your scope, say so immediately. Never guess.

## CONSTRAINTS

- **Read-only**: Never modify files.
- **Stay loaded**: Do not summarize away details.
- **Stay current**: When you get a file change notification, re-read that file immediately.
- **Be fast**: Other agents are waiting. Answer directly, no preamble.'

  launch_prompt="You are a fat cow for: \`${display_path}\`
${focus_section}
${ingest_section}

## PHASE 2: INDEX & SUBSCRIBE

4. Subscribe to file changes in your scope so you stay current:
   - \`hcom events sub --file \"${file_glob}\"\`
   - When you get an [event] notification, re-read the changed file immediately.

5. Announce you're ready:
   - \`hcom send \"@${notify} [fatcow] Loaded ${display_path} - ready for questions\"\`

## PHASE 3: ANSWER

6. Wait for questions. When a message arrives:
   - Parse what they're asking about
   - Answer with file:line references
   - Reply via: \`hcom send \"@<asker> <answer>\"\`

7. On file change notifications:
   - Check which file changed, re-read it, update your mental model

You are a fat, lazy, knowledge-stuffed oracle. Eat all the files. Sit there. Answer questions. Stay current."

  if [[ "$interactive" == "true" ]]; then
    bg_flag=""
  else
    # claude: explicit -p so --headless goes through print mode
    if [[ "$tool" == "claude" ]]; then
      bg_flag="--headless -p"
    else
      bg_flag="--headless"
    fi
  fi
fi

hcom 1 "$tool" --tag "$tag" --go \
  --hcom-system-prompt "$system_prompt" \
  --hcom-prompt "$launch_prompt" \
  -C "$cwd" \
  ${bg_flag} >/dev/null 2>&1 || {
  echo "Error: Launch failed" >&2
  exit 1
}

mode_str="live"
[[ "$dead" == "true" ]] && mode_str="dead"

echo "${tag}: ${mode_str} fatcow (${tool})"
echo "Ingesting: ${display_path}"
[[ -n "$focus" ]] && echo "Focus: ${focus}"
echo

if [[ "$dead" == "true" ]]; then
  echo "Dead fatcow — will stop after ingestion."
  echo "Query it later:"
  echo "  hcom run fatcow --ask ${tag}-<name> \"what does ${display_path} export?\""
else
  echo "Ask it anything:"
  echo "  hcom send \"@${tag} what functions does ${display_path} export?\""
  echo "  hcom send \"@${tag} where is error handling done?\""
  echo
  echo "Stop: hcom stop @${tag}"
fi
