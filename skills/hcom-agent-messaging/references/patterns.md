# Tested Multi-Agent Patterns

Every pattern below has been tested with real agents. Event outputs are from real runs. Full working scripts are in `references/scripts/`.

---

## Pattern 1: Basic Two-Agent Messaging

Worker sends result, reviewer acknowledges, DONE signal to orchestrator.

**Script:** `scripts/basic-messaging.sh`

Key logic:

```bash
# Worker does task, sends result to reviewer
hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "Do: ${task}. Send result: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"RESULT: <answer>\". Then: hcom stop"

# Reviewer acks worker, sends DONE to orchestrator
hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for @worker-. Reply ACK. Send DONE to @bigboss. Then: hcom stop"

# Orchestrator waits for DONE
hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'"
```

**Real event JSON from test run:**
```json
{"id":42,"type":"message","instance":"mila","data":{"from":"mila","text":"RESULT: 1, 2, 3","scope":"mentions","mentions":["niro"],"intent":"inform","thread":"basic-1774354927","sender_kind":"instance","delivered_to":["niro"]}}
{"id":45,"type":"message","instance":"niro","data":{"from":"niro","text":"DONE","scope":"broadcast","intent":"inform","thread":"basic-1774354927","sender_kind":"instance","delivered_to":["mila"]}}
```

---

## Pattern 2: Worker-Reviewer Feedback Loop

Worker does task, reviewer evaluates, sends APPROVED or FIX feedback, worker corrects if needed.

**Script:** `scripts/review-loop.sh`

Key logic:

```bash
# Worker: does task, sends ROUND N DONE, listens for FIX/APPROVED
--hcom-prompt "Task: ${task}. Send ROUND 1 DONE to @reviewer-. If FIX feedback, fix and resend as ROUND 2 DONE. After APPROVED, send FINAL to @bigboss."

# Reviewer: checks each round, sends APPROVED or FIX
--hcom-prompt "On ROUND N DONE: if correct send APPROVED, if wrong send FIX: <issue>."

# Orchestrator waits for FINAL (after APPROVED)
hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%FINAL%'"
```

**Key insight:** The FIX/APPROVED protocol creates a natural feedback loop. Workers self-correct based on reviewer feedback. Multiple rounds happen automatically.

---

## Pattern 3: Ensemble Consensus (N Agents + Judge)

N agents independently answer the same question, judge reads all answers and aggregates.

**Script:** `scripts/ensemble-consensus.sh`

Key logic:

```bash
# Launch N contestants in a loop
for i in 1 2 3; do
  hcom 1 claude --tag "c${i}" --go --headless \
    --hcom-prompt "Answer independently: ${task}. Send ONLY your answer: hcom send \"@judge-\" --thread ${thread} --intent inform -- \"C${i}: <answer>\". Then: hcom stop."
done

# Judge reads all answers via event query
--hcom-prompt "Wait for 3 answers. Check: hcom events --sql \"msg_thread='${thread}' AND msg_text LIKE 'C%'\" --last 10. Synthesize. Send VERDICT."

# Orchestrator waits for VERDICT
hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%VERDICT%'"
```

**Key insight:** The judge uses `hcom events --sql` to query thread messages, reading all answers in one call. Agents run in parallel so N agents cost same wall-clock as 1.

---

## Pattern 4: Sequential Cascade Pipeline

Each stage reads previous stage's transcript for full context handoff.

**Script:** `scripts/cascade-pipeline.sh`

Key logic:

```bash
# Stage 1: Planner
hcom 1 claude --tag plan --go --headless \
  --hcom-prompt "Plan: ${task}. Send PLAN DONE."

# Wait for plan, then launch stage 2 with transcript reference
hcom events --wait 60 --sql "msg_thread='${thread}' AND msg_text LIKE '%PLAN DONE%'"

# Stage 2: Executor reads planner's transcript
hcom 1 claude --tag exec --go --headless \
  --hcom-prompt "Read planner transcript: hcom transcript @${planner} --last 3. Execute the plan. Send EXEC DONE."
```

**Key insight:** `hcom transcript @name --full` is the context handoff mechanism. Each pipeline stage gets the complete work product of the previous stage. Use `--detailed` to include tool I/O (Bash output, file edits).

---

## Pattern 5: Cross-Tool (Claude + Codex)

Claude designs the spec, Codex implements in sandbox.

**Script:** `scripts/cross-tool-duo.sh`

Key logic:

```bash
# Codex waits for spec, implements
hcom 1 codex --tag eng --go --headless \
  --hcom-prompt "Wait for spec from @arch-. Implement it. Send IMPLEMENTED."

# Claude designs spec, sends to Codex, waits for confirmation
hcom 1 claude --tag arch --go --headless \
  --hcom-prompt "Design spec: ${task}. Send SPEC to @eng-. Wait for IMPLEMENTED. Send APPROVED."

# Orchestrator waits for APPROVED
hcom events --wait 180 --sql "msg_thread='${thread}' AND msg_text LIKE '%APPROVED%'"
```

---

## Pattern 6: Codex Codes, Claude Reviews Transcript

Codex writes and runs code, Claude reads Codex's full transcript to review.

**Script:** `scripts/codex-worker.sh`

Key logic:

```bash
# Codex does the work
hcom 1 codex --tag coder --go --headless \
  --hcom-prompt "Do: ${task}. Send CODE DONE to @reviewer-."

# Claude reviews by reading Codex's transcript
hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for CODE DONE. Read transcript: hcom transcript @${coder} --last 5 --full. Send REVIEWED: pass/fail."
```

**Key insight:** Claude reads Codex's complete transcript (including Bash output, file writes, command results) via `hcom transcript @name --full --detailed`. This enables deep code review without sharing files.

---

## Summary Table

| # | Pattern | Agents | Tools | Use case |
|---|---------|--------|-------|----------|
| 1 | Basic messaging | 2 | Claude x2 | Simple task delegation |
| 2 | Review loop | 2 | Claude x2 | Self-correcting feedback |
| 3 | Ensemble consensus | 4 | Claude x4 | Diverse perspectives, best answer |
| 4 | Cascade pipeline | 2 | Claude x2 | Sequential plan-then-execute |
| 5 | Cross-tool duo | 2 | Claude+Codex | Design + sandbox implementation |
| 6 | Codex->Claude review | 2 | Codex+Claude | Code execution + transcript review |
