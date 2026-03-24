# hcom Core Architecture — Complete Reference

Derived from reading 78K lines of Rust source code (hcom v0.7.6). Schema v17.

## SQLite Database (`~/.hcom/hcom.db`, WAL mode)

WAL journal mode with `busy_timeout=5000ms` for concurrent reads. TUI opens with `query_only=ON` (read-only).

### Table: instances (PRIMARY KEY: `name` TEXT)

| Column | Type | Description |
|--------|------|-------------|
| `name` | TEXT PK | Random 4-letter CVCV word (luna, nemo, bali) |
| `session_id` | TEXT UNIQUE | Tool session identifier (nullable) |
| `agent_id` | TEXT UNIQUE | 7-char hex identifier (nullable) |
| `tag` | TEXT | Group tag (e.g., "worker", "reviewer") |
| `parent_session_id` | TEXT | Parent session for subagent relationship |
| `parent_name` | TEXT | Parent instance name |
| `status` | TEXT | active, listening, blocked, inactive, launching, error |
| `status_time` | INTEGER | Epoch timestamp of last status change |
| `status_context` | TEXT | Context: "new", "tool:Bash", "deliver:luna", "stale:listening" |
| `status_detail` | TEXT | Detail: command text, filename, approval reason |
| `tool` | TEXT | claude, gemini, codex, opencode |
| `launch_args` | TEXT | Original launch arguments (for resume) |
| `terminal_preset_requested` | TEXT | Terminal preset requested at launch |
| `terminal_preset_effective` | TEXT | Terminal preset actually used |
| `tcp_mode` | INTEGER | 1 = TCP listener active for instant wake |
| `background` | INTEGER | 1 = headless background process |
| `wait_timeout` | INTEGER | Default wait timeout for this instance |
| `subagent_timeout` | INTEGER | Subagent keep-alive timeout |
| `last_event_id` | INTEGER | Cursor: last event delivered to this instance |
| `last_stop` | REAL | Heartbeat: epoch timestamp of last activity |
| `name_announced` | INTEGER | 1 = bootstrap has been injected |
| `pid` | INTEGER | OS process ID |
| `directory` | TEXT | Working directory at launch |
| `transcript_path` | TEXT | Path to tool transcript file |
| `launch_context` | TEXT | JSON: pane_id, process_id, terminal_id, kitty_listen_on |
| `origin_device_id` | TEXT | Non-empty = remote instance synced via relay |
| `running_tasks` | INTEGER | Count of running subagent tasks |
| `idle_since` | REAL | Timestamp when agent entered idle/listening |
| `hints` | TEXT | User hints passed to agent |
| `created_at` | REAL | Epoch timestamp of creation |

### Table: events (id INTEGER PK AUTOINCREMENT)

Immutable audit log. Every action is recorded.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Auto-incrementing global sequence |
| `timestamp` | TEXT | ISO-8601 UTC timestamp |
| `type` | TEXT | message, status, life, bundle |
| `instance` | TEXT | Agent name or sys_* for system |
| `data` | TEXT | Full JSON payload |

### View: events_v (flattens JSON)

Computed view for easy SQL querying:

| Column | Source | Description |
|--------|--------|-------------|
| `id` | events.id | Event ID |
| `timestamp` | events.timestamp | ISO-8601 timestamp |
| `type` | events.type | Event type |
| `instance` | events.instance | Agent name |
| `data` | events.data | Raw JSON |
| `msg_from` | `json_extract(data, '$.from')` | Message sender |
| `msg_text` | `json_extract(data, '$.text')` | Message text |
| `msg_scope` | `json_extract(data, '$.scope')` | broadcast or mentions |
| `msg_mentions` | `json_extract(data, '$.mentions')` | JSON array of mentioned agents |
| `msg_intent` | `json_extract(data, '$.intent')` | request, inform, ack |
| `msg_thread` | `json_extract(data, '$.thread')` | Thread identifier |
| `msg_reply_to` | `json_extract(data, '$.reply_to')` | Referenced event ID |
| `delivered_to` | `json_extract(data, '$.delivered_to')` | JSON array of recipients |
| `bundle_id` | `json_extract(data, '$.bundle_id')` | Bundle reference |
| `sender_kind` | `json_extract(data, '$.sender_kind')` | instance, system, external |
| `status_val` | `json_extract(data, '$.status')` | Status value |
| `status_context` | `json_extract(data, '$.context')` | Status context |
| `status_detail` | `json_extract(data, '$.detail')` | Status detail |
| `life_action` | `json_extract(data, '$.action')` | Lifecycle action |

### FTS5 Full-Text Search

```sql
CREATE VIRTUAL TABLE events_fts USING fts5(searchable, tokenize='unicode61');

-- Auto-indexing trigger on INSERT
CREATE TRIGGER events_fts_insert AFTER INSERT ON events BEGIN
  INSERT INTO events_fts(rowid, searchable) VALUES (
    new.id,
    COALESCE(json_extract(new.data, '$.text'), '') || ' ' ||
    COALESCE(json_extract(new.data, '$.from'), '') || ' ' ||
    COALESCE(new.instance, '') || ' ' ||
    COALESCE(json_extract(new.data, '$.context'), '') || ' ' ||
    COALESCE(json_extract(new.data, '$.detail'), '') || ' ' ||
    COALESCE(json_extract(new.data, '$.action'), '') || ' ' ||
    COALESCE(json_extract(new.data, '$.reason'), '')
  );
END;
```

### Table: notify_endpoints

| Column | Type | Description |
|--------|------|-------------|
| `instance` | TEXT | Instance name |
| `kind` | TEXT | "pty" or "inject" |
| `port` | INTEGER | TCP port on 127.0.0.1 |
| `updated_at` | TEXT | Last update timestamp |

PRIMARY KEY: (instance, kind)

### Table: process_bindings

| Column | Type | Description |
|--------|------|-------------|
| `process_id` | TEXT PK | UUID assigned at launch |
| `session_id` | TEXT | Tool session ID |
| `instance_name` | TEXT | Instance name (FK) |
| `updated_at` | TEXT | Timestamp |

### Table: session_bindings

| Column | Type | Description |
|--------|------|-------------|
| `session_id` | TEXT PK | Tool session ID |
| `instance_name` | TEXT | Instance name (FK) |

### Table: kv (key-value storage)

| Key Pattern | Description |
|-------------|-------------|
| `_wake_last_wall` | Wall-clock time for sleep detection |
| `_wake_grace_until` | Grace period end timestamp |
| `relay_daemon_port` | TCP port for CLI->daemon push |
| `relay_last_push_id` | Event cursor for push dedup |
| `relay_last_push` | Last push timestamp |
| `relay_last_sync` | Global sync timestamp |
| `relay_events_{device_id}` | Per-device event cursor |
| `relay_sync_time_{device_id}` | Per-device last sync time |
| `relay_reset_{device_id}` | Per-device reset timestamp |
| `relay_short_{short_id}` | Short ID -> device_id mapping |
| `relay_status` | "ok", "error", "disconnected", "waiting" |
| `relay_last_error` | Error message |
| `relay_daemon_fail_count` | Consecutive TCP probe failures |
| `events_sub:sub-{hash}` | Event subscription JSON |
| `events_sub:reqwatch-{id}` | Request-watch subscription |

### Schema Management

- Version stored in `PRAGMA user_version` (currently 17)
- Migrations are cumulative; v17 has one migration (adding terminal_preset columns)
- Stale process detection: if code version < DB version, continue with existing schema (backward compatible)
- Incompatible schemas: archive old DB to `~/.hcom/archive/session-YYYY-MM-DD_HHMMSS/`, reinit fresh
- Archive preserves the old database for recovery

---

## Instance Lifecycle State Machine

```
inactive (entry point)
   |
   v
launching (status_context="new", age < 30s)
   |
   +---> ready (first status update, session binding succeeds)
   |     |
   |     v
   +-- active (agent processing, hook sets status)
   |     |
   |     v (hook sets status="listening" when done with turn)
   +-- listening (idle, ready for messages)
   |     |
   |     +---> blocked (approval prompt detected via OSC9, or status="blocked" set)
   |     |
   |     +---> heartbeat check:
   |           if age > HEARTBEAT_THRESHOLD_TCP(35s) or HEARTBEAT_THRESHOLD_NO_TCP(10s)
   |           AND no wake grace period active
   |           ---> mark stale: inactive (status="inactive", context="stale:listening")
   |
   +---> launch_failed (if no session binding after 30s or error during launch)
         |
         v
         inactive
```

### Status Computation Flow (get_instance_status)

1. If `status_context="new"` AND age < 30s -> LAUNCHING
2. If `status="listening"` AND heartbeat > threshold (AND no wake grace) -> INACTIVE/"stale:listening"
3. Else if `status != inactive` AND activity timeout (5min) exceeded -> INACTIVE/"stale:{prev}"
4. Otherwise -> keep existing status

### Key Computed Fields

- **age_string**: Human-readable time since last `status_time` or `created_at`
- **heartbeat_age**: `now - last_stop` (only checked for listening status, skipped for remote instances)
- **is_remote**: `origin_device_id.is_some()` (synced status, age always 0)
- **wake_grace_period**: 60s after sleep detected via wall-clock vs monotonic-clock drift > 30s

### Heartbeat Management

- Delivery thread: `update_heartbeat()` sets `last_stop = now` every 30s
- Hooks: every hcom command execution updates `last_stop`
- Idle detection: no heartbeat beyond threshold -> mark stale
- Cross-process persistence: `_wake_grace_until` stored in KV table

---

## Identity Resolution Algorithm (3-Tier Binding)

### Tier 1: Explicit --name Flag
- Validation: base name only (lowercase `[a-z0-9_]+`) OR UUID OR agent_id (7-char hex)
- Lookup: exact instance name match via `db.get_instance()`
- Or: agent_id lookup via `db.get_instance_by_agent_id(name)`
- Error if not found

### Tier 2: Process Binding (HCOM_PROCESS_ID)
- Every launched instance gets `HCOM_PROCESS_ID=<uuid>` environment variable
- Lookup: `db.get_process_binding(process_id)` -> instance_name
- Codex: binds session on first hcom command via `bind_session_to_process()`
- Error if process_id expired or not found

### Tier 3: Session Binding (HCOM_SESSION_ID)
- Claude hooks: SessionStart sets session_id -> instances.session_id
- Lookup: `db.get_session_binding(session_id)` -> instance_name
- Group binding: parent + subagents share parent_session_id via `group_id()`

### Name Resolution Fallback
- Display name: `tag-name` format ("team-luna") -> `resolve_display_name()` splits at hyphen -> base name "luna"
- Remote suffix: `luna:BOXE` -> split(':') for base matching
- Vanilla binding: transcript marker `[hcom:instance-name]` searched backwards in 64MB chunks

### Sender Kinds
- `Instance`: Registered hcom participant (full routing rules apply)
- `External`: `--from` flag or ad-hoc sender (broadcasts to all)
- `System`: System notifications (broadcasts to all, from `[hcom-events]`)

---

## Message Routing and Delivery Logic

### Scope Computation (messages.rs)

**Broadcast scope** (no explicit targets):
- No @mentions in text OR explicit_targets is empty
- `scope="broadcast"` in JSON payload
- Delivered to all active/listening/recently-stopped agents

**Mentions scope** (targeted):
- @mention extraction: `MENTION_PATTERN` regex
  - Valid: `@luna`, `@luna:BOXE` (remote), `@team-luna` (tag-name)
  - Invalid: `user@domain.com`, `path.@name`, `var_@name`
- Target matching: prefix match on full_name (tag-name) OR fallback to base name
  - Remote target `luna:BOXE` matches any instance starting with `luna:BOXE`
  - Local target `luna` matches exact or prefix on full_name or base_name
- Strict validation: unmatched mentions produce errors
- Deduplication: preserve insertion order
- `scope="mentions"`, `mentions=[list]` in JSON

### Delivery Filter (should_deliver_to)
```
skip own messages (from == receiver)
if scope="broadcast" -> deliver
if scope="mentions" -> deliver if receiver_base in mentions
  (cross-device: receiver.split(':')[0] matches mention.split(':')[0])
else -> skip
```

### Instance Scope Query
- All enabled (active/listening/blocked) + stopped within 10-minute window
- Filter by name, status, tool, tags
- Remote instances: suffix notation `luna:BOXE`

---

## Delivery Pipeline (5 Stages)

### Stage 1: Send Command
- `hcom send [@mentions|--] <message>` with optional `--intent`, `--reply-to`, `--thread`
- Compute scope via `messages.rs::compute_scope()`
- Validate mentions against active/recent instances

### Stage 2: Event Logging
```json
{
  "from": "sender_name",
  "text": "message text",
  "scope": "broadcast" | "mentions",
  "mentions": ["luna", "nova"],
  "intent": "request" | "inform" | "ack",
  "reply_to": 42,
  "thread": "review-1",
  "bundle_id": "bundle:abc123",
  "delivered_to": ["luna", "nova"],
  "sender_kind": "instance" | "system" | "external",
  "_relay": false
}
```
INSERT into events table. Triggers FTS5 index update. Triggers subscription checks synchronously.

### Stage 3: Hook Delivery (Claude, Gemini)
- Poll hook: returns unread messages via `get_unread_messages()` filtered by `should_deliver_to()`
- Max 50 messages per delivery (`MAX_MESSAGES_PER_DELIVERY`)
- Messages formatted as JSON in `hookSpecificOutput.additionalContext`
- Update `instance.last_event_id` cursor after delivery
- Hook returns exit code 2 (block) with messages in reason field

### Stage 4: PTY Injection (Codex, adhoc)
- Delivery thread checks gate conditions before injecting
- Gates: require_idle, block_on_approval, block_on_user_activity, require_ready_prompt, require_prompt_empty
- Tool configs differ per tool
- Inject via TCP connection to `notify_endpoints[pty]` port
- Screen tracker monitors terminal output for delivery confirmation

### Stage 5: Delivery Verification
- Monitor terminal for output change after injection
- 3 failed attempts -> mark status="blocked", reason="output_unstable"
- TCP notification wakes delivery thread instantly (no polling)
- Cursor advance: `last_event_id` updated only after successful delivery

---

## Bootstrap Injection Mechanism

### Template Structure (bootstrap.rs)

The bootstrap is a structured system message wrapped in `<hcom_system_context>` XML:

**UNIVERSAL section** (~50 lines, ~600 tokens):
- Instance name, display name, authority (`SENDER="bigboss"`)
- Binding marker: `[hcom:instance_name]` in first response
- Response rules: request -> always respond, inform -> only if useful, ack -> don't respond
- Full command reference: send, list, transcript, events, bundle, spawn, resume, fork, kill, run, term, config
- Active instances snapshot (up to 8, grouped by tool): `Active (snapshot): claude: luna, nemo | codex: kira`
- Available scripts list: `Scripts: confess, debate, fatcow`

**Conditional sections:**
- `TAG_NOTICE`: `You are tagged "{tag}". Message your group: send @{tag}- -- msg`
- `RELAY_NOTICE`: Remote agent suffix notation and event ID format
- `HEADLESS_NOTICE`: `Headless mode: No one sees your chat, only hcom messages.`
- `UVX_CMD_NOTICE`: Alternate command substitution

**Tool-specific delivery:**
- `DELIVERY_AUTO` (Claude, OpenCode, Gemini launched): Messages auto-arrive via <hcom> tags
- `DELIVERY_CODEX_HCOM_LAUNCHED` (Codex launched): <hcom> tags trigger `hcom listen 1`
- `DELIVERY_ADHOC` (non-launched): CONNECTED MODE with listen loop

**Claude-only SUBAGENTS section:**
- How to spawn subagents via Task with background=true
- Subagent config: `hcom config -i self subagent_timeout [SEC]`

### Subagent Bootstrap (simpler)
- Parent relationship: `Your parent: {parent_name}`
- Announcement instruction
- Abbreviated command reference
- No subagent spawning capability

### Template Substitution Variables
- `{display_name}`, `{instance_name}`, `{tag}`, `{hcom_cmd}`, `{SENDER}`
- `{active_instances}`, `{scripts}`
- `{{name}}` -> `{name}` (escaped braces for literal output)

### Token Efficiency
- Total bootstrap: ~600 tokens
- Agents learn details via `--help` at runtime
- No full documentation embedded; just enough for autonomous operation

---

## Instance Lifecycle Initialization

### Launch Sequence

1. **Placeholder creation** (launcher.rs)
   - Create instance row with status="inactive", status_context="new"
   - Assign: process_id, session_id (if available), tag, terminal_preset
   - Set created_at timestamp

2. **Process binding**
   - Store `HCOM_PROCESS_ID` -> instance.name in process_bindings table
   - Enables recovery if session not yet bound

3. **Session binding**
   - Claude: SessionStart hook immediately
   - Codex: First codex-notify event (agent-turn-complete)
   - Gemini: BeforeAgent hook
   - OpenCode: Plugin binding ceremony

4. **Status progression**
   - First status update: `set_status()` emits "ready" event
   - Batch notification: if all instances in batch ready, send launcher notification
   - Hook: agent sets status="active" during turn, status="listening" when idle

5. **Heartbeat management**
   - Delivery thread: update every 30s
   - Hooks: every hcom command execution
   - Idle detection: threshold exceeded -> mark stale

6. **Cleanup**
   - Stale timeout: launching > 30s without binding -> launch_failed
   - Placeholder cleanup: created but never bound after 2 min -> cleanup_stale_instances()
   - Process binding expiry: if process dies -> remove binding

---

## Advanced Features

### Tag-Based Grouping
- `--tag team-name` -> instance.tag stored
- Full display name: `{tag}-{instance_name}` (e.g., "worker-luna")
- Routing: `send @team-` broadcasts to all with that tag

### Subagent Sessions
- Parent-child relationship: parent_session_id links subagent to parent
- Group message delivery: subagent shares parent's group_id()
- Orphan recovery: pidtrack snapshots running instances before DB reset

### Wake Detection (Sleep/Wake)
- Wall-clock vs monotonic-clock drift > 30s = wake detected
- Grace period: 60s after wake, heartbeat checks suspended
- Cross-process: persisted via KV (`_wake_grace_until`, `_wake_last_wall`)

### Bundle Context (Task Handoff)
- `bundle prepare`: snapshot events, files, transcript
- bundle_id links messages to context bundles
- Extends: inheritance for nested bundles
- Detail levels: normal, full, detailed

---

## File Reference

| Component | File(s) | Key Export |
|-----------|---------|-----------|
| Schema + Init | db.rs | `HcomDb::init_db()`, `ensure_schema()`, migration v17 |
| Instances + Status | instances.rs | `get_instance_status()`, `is_in_wake_grace()`, `ComputedStatus` |
| Identity Resolve | identity.rs | `resolve_identity()`, `resolve_from_name()`, 3-tier binding |
| Router (Dispatch) | router.rs | `resolve_action()`, hook/command detection, global flags |
| Message Logic | messages.rs | `compute_scope()`, `MessageScope`, mention parsing, delivery filter |
| PTY Delivery | delivery.rs | `ToolConfig`, `GateResult`, delivery loop, gate checks |
| Bootstrap Context | bootstrap.rs | `get_bootstrap()`, template substitution, tool-specific rules |
| Constants | shared/constants.rs | `MENTION_PATTERN`, status icons, `RELEASED_TOOLS`, limits |
