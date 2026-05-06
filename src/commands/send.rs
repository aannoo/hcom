//! `hcom send` command — send messages to hcom instances.

use std::io::{IsTerminal, Read as IoRead};

use crate::db::HcomDb;
use crate::db::subscriptions::create_request_watches;
use crate::identity;
use crate::instances;
use crate::messages::{
    InstanceInfo, MessageEnvelope, MessageScope, compute_scope, should_deliver_message,
    validate_intent, validate_message,
};
use crate::shared::{CommandContext, SENDER, SenderIdentity, SenderKind, status_icon};

const SEND_AFTER_HELP: &str = "\
Target matching:
    @luna                          base name (matches luna, api-luna)
    @api-luna                      exact full name
    @api-                          prefix: all with tag 'api'
    @luna:BOXE                     remote agent on another device
  Underscore blocks prefix: @luna does NOT match luna_reviewer_1

Inline bundle (attach structured context):
    --title <text>                 Create and attach bundle inline
    --description <text>           Bundle description (required with --title)
    --events <ids>                 Event IDs/ranges: 1,2,5-10
    --files <paths>                Comma-separated file paths
    --transcript <ranges>          Format: 3-14:normal,6:full,22-30:detailed
    --extends <id>                 Parent bundle (optional)
  See 'hcom bundle --help' for bundle details

Examples:
    hcom send @luna -- Hello there!
    hcom send @luna @nova --intent request -- Can you help?
    hcom send -- Broadcast message to everyone
    echo 'Complex message' | hcom send @luna
    hcom send @luna <<'EOF'
    Multi-line message with special chars
    EOF";

/// Parse positional arg: accept both @targets and bare text.
/// Bare text (no @ prefix) is accepted by clap and separated later in cmd_send.
fn parse_positional(s: &str) -> Result<String, String> {
    if s == "@" {
        Err("Empty target '@' is not allowed".to_string())
    } else {
        Ok(s.to_string())
    }
}

/// Parsed arguments for `hcom send`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "send",
    about = "Send a message to agents",
    after_help = SEND_AFTER_HELP,
)]
pub struct SendArgs {
    /// Positional args: @targets and/or bare message text (backward compat)
    #[arg(value_parser = parse_positional)]
    pub positionals: Vec<String>,

    /// Message text (after --)
    #[arg(last = true)]
    pub message: Vec<String>,

    // ── Message source ──
    /// Read message from stdin
    #[arg(long)]
    pub stdin: bool,

    /// Read message from file
    #[arg(long)]
    pub file: Option<String>,

    /// Read message from base64-encoded string
    #[arg(long)]
    pub base64: Option<String>,

    // ── Envelope ──
    /// Message intent (request|inform|ack)
    #[arg(long)]
    pub intent: Option<String>,

    /// Reply to event ID (42 or 42:BOXE)
    #[arg(long)]
    pub reply_to: Option<String>,

    /// Threaded routing: seed recipients once, then reuse thread members
    #[arg(long)]
    pub thread: Option<String>,

    // ── Sender ──
    /// External sender identity
    #[arg(long)]
    pub from: Option<String>,

    /// Shorthand for --from bigboss
    #[arg(short = 'b')]
    pub bigboss: bool,

    /// Suppress output
    #[arg(long)]
    pub quiet: bool,

    // ── Inline bundle ──
    /// Bundle title (creates inline bundle)
    #[arg(long)]
    pub title: Option<String>,

    /// Bundle description
    #[arg(long)]
    pub description: Option<String>,

    /// Bundle event IDs/ranges
    #[arg(long)]
    pub events: Option<String>,

    /// Bundle file paths (comma-separated)
    #[arg(long, rename_all = "verbatim")]
    pub files: Option<String>,

    /// Bundle transcript ranges
    #[arg(long)]
    pub transcript: Option<String>,

    /// Parent bundle ID
    #[arg(long)]
    pub extends: Option<String>,

    /// Set by router: whether `--` was present in raw argv.
    /// Clap can't distinguish "no --" from "-- with no args", so the router sets this.
    #[arg(skip)]
    pub had_separator: bool,
}

impl SendArgs {
    /// Resolve the effective --from name (--from overrides -b).
    fn sender_name(&self) -> Option<String> {
        if let Some(ref name) = self.from {
            Some(name.clone())
        } else if self.bigboss {
            Some("bigboss".to_string())
        } else {
            None
        }
    }

    /// Whether a `--` separator was present in the raw argv.
    fn has_separator(&self) -> bool {
        self.had_separator
    }

    /// Build inline bundle data from flags, or None if no bundle flags present.
    fn build_bundle_data(&self) -> Result<Option<serde_json::Value>, String> {
        let has_any = self.title.is_some()
            || self.description.is_some()
            || self.events.is_some()
            || self.files.is_some()
            || self.transcript.is_some()
            || self.extends.is_some();

        if !has_any {
            return Ok(None);
        }

        let title = self.title.as_ref().ok_or_else(|| {
            let present: Vec<&str> = [
                self.description.as_ref().map(|_| "--description"),
                self.events.as_ref().map(|_| "--events"),
                self.files.as_ref().map(|_| "--files"),
                self.transcript.as_ref().map(|_| "--transcript"),
                self.extends.as_ref().map(|_| "--extends"),
            ]
            .into_iter()
            .flatten()
            .collect();
            format!(
                "Bundle flags require --title: found {} without --title",
                present.join(", ")
            )
        })?;

        let description = self
            .description
            .as_ref()
            .ok_or("--description is required when --title is present")?;

        use crate::core::bundles::parse_csv_list;
        let events = parse_csv_list(self.events.as_deref());
        let files = parse_csv_list(self.files.as_deref());
        let transcript = parse_csv_list(self.transcript.as_deref());

        let mut bundle = serde_json::json!({
            "title": title,
            "description": description,
            "refs": {
                "events": events,
                "files": files,
                "transcript": transcript,
            }
        });

        if let Some(ref ext) = self.extends {
            bundle
                .as_object_mut()
                .unwrap()
                .insert("extends".into(), serde_json::json!(ext));
        }

        Ok(Some(bundle))
    }
}

/// Get formatted recipient feedback showing who received the message.
fn get_recipient_feedback(db: &HcomDb, delivered_to: &[String]) -> String {
    if delivered_to.is_empty() {
        return format!("Sent to: {SENDER}");
    }
    if delivered_to.len() > 10 {
        return format!("Sent to {} agents", delivered_to.len());
    }

    let mut parts = Vec::new();
    for name in delivered_to {
        if let Ok(Some(data)) = db.get_instance_full(name) {
            let icon = status_icon(&data.status);
            let display = instances::get_display_name(db, name);
            parts.push(format!("{icon} {display}"));
        } else {
            parts.push(format!("◌ {name}"));
        }
    }
    format!("Sent to: {}", parts.join(", "))
}

///
/// Validates message, computes scope, logs event, notifies all instances.
/// Returns delivered_to list (base names).
pub fn send_message(
    db: &HcomDb,
    identity: &SenderIdentity,
    message: &str,
    envelope: Option<&MessageEnvelope>,
    explicit_targets: Option<&[String]>,
) -> Result<Vec<String>, String> {
    validate_message(message)?;

    // Get participating instances
    let rows: Vec<InstanceInfo> = db
        .conn()
        .prepare("SELECT name, tag FROM instances")
        .map_err(|e| format!("DB error: {e}"))?
        .query_map([], |row| {
            Ok(InstanceInfo {
                name: row.get::<_, String>(0)?,
                tag: row.get::<_, Option<String>>(1)?,
            })
        })
        .map_err(|e| format!("DB error: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // Compute scope and routing. Thread-only sends keep their original message
    // semantics; membership only affects the delivery target set.
    let scope_result = compute_scope(message, &rows, explicit_targets.map(|t| t as &[String]))?;
    let thread_delivery_members =
        if let Some(thread) = envelope.and_then(|env| env.thread.as_deref()) {
            if scope_result.scope == MessageScope::Broadcast {
                let members = db.get_thread_members(thread);
                if members.is_empty() {
                    return Err(format!(
                        "Thread '{thread}' has no members. Seed it with @mentions first."
                    ));
                }
                members
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
    let is_thread_resolved = !thread_delivery_members.is_empty();
    let effective_scope = if thread_delivery_members.is_empty() {
        scope_result.scope
    } else {
        MessageScope::Mentions
    };
    let effective_mentions = if thread_delivery_members.is_empty() {
        scope_result.mentions.clone()
    } else {
        thread_delivery_members.clone()
    };

    // Build scope data for should_deliver_message
    let scope_str = effective_scope.as_str();
    let mentions_json: Vec<serde_json::Value> = effective_mentions
        .iter()
        .map(|m| serde_json::json!(m))
        .collect();
    let mut scope_data = serde_json::json!({
        "scope": scope_str,
    });
    if !effective_mentions.is_empty() {
        scope_data["mentions"] = serde_json::json!(mentions_json);
    }
    // Add group_id if identity has one
    if let Some(gid) = identity.group_id() {
        scope_data["group_id"] = serde_json::json!(gid);
    }

    let delivered_to: Vec<String> = rows
        .iter()
        .filter(|inst| {
            should_deliver_message(&scope_data, &inst.name, &identity.name).unwrap_or(false)
        })
        .map(|inst| inst.name.clone())
        .collect();

    // Build event data
    let mut data = serde_json::json!({
        "from": identity.name,
        "sender_kind": match identity.kind {
            SenderKind::External => "external",
            SenderKind::Instance => "instance",
            SenderKind::System => "system",
        },
        "scope": scope_str,
        "text": message,
        "delivered_to": delivered_to,
    });

    // Add scope extra data (mentions)
    if !effective_mentions.is_empty() {
        data["mentions"] = serde_json::json!(effective_mentions);
    }

    if let Some(env) = envelope {
        if let Some(intent) = &env.intent {
            data["intent"] = serde_json::json!(intent.as_str());
        }
        if let Some(reply_to) = &env.reply_to {
            data["reply_to"] = serde_json::json!(reply_to);
            // Resolve to local event ID
            if let Some(local_id) = resolve_reply_to_local(db, reply_to) {
                data["reply_to_local"] = serde_json::json!(local_id);

                // Ack-on-ack loop prevention
                if env.intent.as_ref().map(|i| i.as_str()) == Some("ack") {
                    if let Some(parent_intent) = get_intent_from_event(db, local_id) {
                        if parent_intent == "ack" {
                            return Err("Ack-on-ack loop detected. Message blocked.".to_string());
                        }
                        if parent_intent == "inform" {
                            return Err("Cannot ack an inform - informational messages don't need acknowledgment.".to_string());
                        }
                    }
                }
            }
        }
        if let Some(thread) = &env.thread {
            data["thread"] = serde_json::json!(thread);
        }
        if let Some(bundle_id) = &env.bundle_id {
            data["bundle_id"] = serde_json::json!(bundle_id);
        }
    }

    // Determine routing instance (namespace isolation)
    let routing_instance = match identity.kind {
        SenderKind::External => format!("ext_{}", identity.name),
        SenderKind::System => format!("sys_{}", identity.name),
        SenderKind::Instance => identity.name.clone(),
    };

    // Log event to DB
    let _event_id = db
        .log_event("message", &routing_instance, &data)
        .map_err(|e| format!("Failed to write message to database: {e}"))?;

    // Auto-create request-watch subscriptions for targeted requests
    if let Some(env) = envelope {
        if let Some(thread) = env.thread.as_deref() {
            db.add_thread_memberships(
                thread,
                matches!(identity.kind, SenderKind::Instance).then_some(identity.name.as_str()),
                &delivered_to,
            );
        }

        if env.intent.as_ref().map(|i| i.as_str()) == Some("request")
            && matches!(identity.kind, SenderKind::Instance)
            && effective_scope == MessageScope::Mentions
            && !is_thread_resolved
        {
            create_request_watches(db, &identity.name, _event_id, &delivered_to);
        }
    }

    // Notify all instances (wake delivery loops)
    crate::notify::wake_all(db);

    // Trigger relay push so remote devices see the message immediately
    crate::relay::trigger_push();

    Ok(delivered_to)
}

/// Resolve reply_to to local event ID. Returns None if not found.
fn resolve_reply_to_local(db: &HcomDb, reply_to: &str) -> Option<i64> {
    // reply_to can be "42" or "42:BOXE" (remote)
    let local_part = reply_to.split(':').next()?;
    let id: i64 = local_part.parse().ok()?;

    // Verify event exists and is a message
    let exists: bool = db
        .conn()
        .query_row(
            "SELECT 1 FROM events WHERE id = ? AND type = 'message'",
            rusqlite::params![id],
            |_| Ok(true),
        )
        .unwrap_or(false);

    if exists { Some(id) } else { None }
}

/// Get thread from an event (for --reply-to thread inheritance).
fn get_thread_from_event(db: &HcomDb, event_id: i64) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT json_extract(data, '$.thread') FROM events WHERE id = ?",
            rusqlite::params![event_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
}

/// Get intent from an event (for ack-on-ack prevention).
fn get_intent_from_event(db: &HcomDb, event_id: i64) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT json_extract(data, '$.intent') FROM events WHERE id = ?",
            rusqlite::params![event_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
}

/// Resolve message from one of 5 source modes.
/// Returns (message_text, had_explicit_source).
fn resolve_message(args: &SendArgs) -> Result<String, String> {
    let has_separator = args.has_separator();

    // Mutual exclusivity
    let source_count = [
        args.stdin,
        args.file.is_some(),
        args.base64.is_some(),
        has_separator,
    ]
    .iter()
    .filter(|&&x| x)
    .count();
    if source_count > 1 {
        return Err("Only one of --, --stdin, --file, --base64 can be used".to_string());
    }

    // 1. -- separator
    if has_separator {
        let text = args.message.join(" ");
        if text.is_empty() {
            return Err("No message after --".to_string());
        }
        return Ok(text);
    }

    // 2. --stdin
    if args.stdin {
        return read_stdin();
    }

    // 3. --file
    if let Some(ref path) = args.file {
        let resolved = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        return match std::fs::read_to_string(&resolved) {
            Ok(content) if !content.is_empty() => Ok(content),
            Ok(_) => Err(format!("File is empty: {path}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("File not found: {path}"))
            }
            Err(e) => Err(format!("Cannot read file: {e}")),
        };
    }

    // 4. --base64
    if let Some(ref b64) = args.base64 {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| "Invalid base64 encoding".to_string())?;
        let s =
            String::from_utf8(bytes).map_err(|_| "Base64 decoded to invalid UTF-8".to_string())?;
        if s.is_empty() {
            return Err("Base64 decoded to empty string".to_string());
        }
        return Ok(s);
    }

    // 5. Auto-pipe (stdin is a pipe, no explicit source)
    if !std::io::stdin().is_terminal() {
        return read_stdin();
    }

    // No message source found
    let targets_str = if args.positionals.is_empty() {
        "@target".to_string()
    } else {
        args.positionals
            .iter()
            .take(3)
            .map(|t| {
                if t.starts_with('@') {
                    t.clone()
                } else {
                    format!("@{t}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    Err(format!(
        "No message provided.\nUse: hcom send {targets_str} -- your message\n Or: echo 'msg' | hcom send {targets_str}"
    ))
}

/// Read message from stdin pipe.
fn read_stdin() -> Result<String, String> {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_ok() && !buf.is_empty() {
        Ok(buf)
    } else {
        Err("No input received on stdin".to_string())
    }
}

/// Process positional args without `--` separator.
/// Matches Python messaging.py behavior:
///   - Empty → ([], None)
///   - Single arg with `@` prefix and space → backward compat: entire text is message
///   - Mix of @targets and bare text → separate targets from message
///   - Pure @targets → targets only, no message
fn process_positionals(positionals: &[String]) -> (Vec<String>, Option<String>) {
    if positionals.is_empty() {
        return (vec![], None);
    }

    // Backward compat: single arg starting with @ and containing space
    // e.g. "@luna hi" → whole thing is message (compute_scope extracts @mentions)
    if positionals.len() == 1 && positionals[0].starts_with('@') && positionals[0].contains(' ') {
        return (vec![], Some(positionals[0].clone()));
    }

    // Separate @targets from bare text
    let mut targets = Vec::new();
    let mut remaining = Vec::new();

    for arg in positionals {
        if let Some(name) = arg.strip_prefix('@') {
            targets.push(name.to_string());
        } else {
            remaining.push(arg.clone());
        }
    }

    if remaining.len() > 1 {
        // Multiple non-@ args without -- separator → error
        // Return empty message to trigger "no message" error with helpful hint
        return (targets, None);
    }

    if remaining.len() == 1 {
        return (targets, Some(remaining[0].clone()));
    }

    (targets, None)
}

/// Main entry point for `hcom send` command.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_send(db: &HcomDb, args: &SendArgs, ctx: Option<&CommandContext>) -> i32 {
    // ── Resolve --from name ──
    let from_name = args.sender_name();

    if let Some(ref name) = from_name {
        if name.is_empty() || name.len() > 50 {
            eprintln!("Error: Name too long ({} chars, max 50)", name.len());
            return 1;
        }
        if name.contains([
            '@', '|', '&', ';', '<', '>', '`', '$', '\'', '"', '\\', '\n', '\r',
        ]) {
            eprintln!("Error: Name contains invalid characters");
            return 1;
        }
    }

    // Guard: subagents cannot use --from/-b
    if from_name.is_some() {
        let actor_from_ctx = ctx.and_then(|c| c.identity.clone());
        let actor = actor_from_ctx
            .or_else(|| identity::resolve_identity(db, None, None, None, None, None, None).ok());
        match actor {
            Some(ref actor) if matches!(actor.kind, SenderKind::Instance) => {
                if let Some(ref data) = actor.instance_data {
                    if data
                        .get("parent_name")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty())
                    {
                        eprintln!(
                            "Error: Subagents cannot use --from/-b (external sender spoofing)"
                        );
                        return 1;
                    }
                }
            }
            _ => {}
        }
    }

    let explicit_name = ctx.and_then(|c| c.explicit_name.as_deref());

    // ── Validate envelope flags ──
    let mut envelope = MessageEnvelope::default();

    if let Some(ref val) = args.intent {
        let val = val.to_lowercase();
        if let Err(e) = validate_intent(&val) {
            eprintln!("Error: {e}");
            return 1;
        }
        envelope.intent = val.parse().ok();
    }

    envelope.reply_to = args.reply_to.clone();

    if let Some(ref val) = args.thread {
        if val.len() > 64 {
            eprintln!("Error: Thread name too long ({} chars, max 64)", val.len());
            return 1;
        }
        if !val
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            eprintln!("Error: Thread name must be alphanumeric with hyphens/underscores");
            return 1;
        }
        envelope.thread = Some(val.clone());
    }

    // Ack requires reply_to
    if envelope.intent.as_ref().map(|i| i.as_str()) == Some("ack") && envelope.reply_to.is_none() {
        eprintln!("Error: Intent 'ack' requires --reply-to <id>");
        return 1;
    }

    if let Some(ref reply_to) = envelope.reply_to {
        if let Some(local_id) = resolve_reply_to_local(db, reply_to) {
            if envelope.thread.is_none() {
                if let Some(parent_thread) = get_thread_from_event(db, local_id) {
                    envelope.thread = Some(parent_thread);
                }
            }
        } else {
            eprintln!("Error: Invalid --reply-to: event not found or not a message");
            return 1;
        }
    }

    // ── Inline bundle ──
    let bundle_data = match args.build_bundle_data() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // ── Process positional args: separate @targets from bare message text ──
    // Matches Python behavior in messaging.py:
    //   - Single "@name message" (with space) → entire text is message, @mention parsed by compute_scope
    //   - Non-@ args → message text (broadcast)
    //   - Pure @targets → explicit targets
    let (effective_targets, compat_message) =
        if !args.has_separator() && !args.stdin && args.file.is_none() && args.base64.is_none() {
            process_positionals(&args.positionals)
        } else {
            // With -- separator or explicit source: validate @targets
            let mut validated = Vec::new();
            for arg in &args.positionals {
                if let Some(stripped) = arg.strip_prefix('@') {
                    if stripped.is_empty() {
                        eprintln!("Error: Empty target '@' is not allowed");
                        return 1;
                    }
                    validated.push(stripped.to_string());
                } else {
                    let mut msg = format!("Error: Unexpected argument '{arg}'");
                    if arg.chars().all(|c| c.is_alphabetic()) && arg.len() <= 20 {
                        msg.push_str(&format!("\nDid you mean @{arg}? Targets require @"));
                    }
                    eprintln!("{msg}");
                    return 1;
                }
            }
            (validated, None)
        };

    // ── Resolve message ──
    let mut message = if let Some(msg) = compat_message {
        msg
    } else {
        match resolve_message(args) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    };

    if let Err(err) = validate_message(&message) {
        eprintln!("Error: {err}");
        return 1;
    }

    // ── Resolve sender identity ──
    let sender_identity = if let Some(ref name) = from_name {
        SenderIdentity {
            kind: SenderKind::External,
            name: name.clone(),
            instance_data: None,
            session_id: None,
        }
    } else if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
        id.clone()
    } else if let Some(name) = explicit_name {
        match identity::resolve_identity(db, Some(name), None, None, None, None, None) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    } else {
        match identity::resolve_identity(db, None, None, None, None, None, None) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    };

    // Guard: Block sends from vanilla Claude before opt-in
    if matches!(sender_identity.kind, SenderKind::Instance)
        && sender_identity.instance_data.is_none()
        && std::env::var("CLAUDE_CODE_ENTRYPOINT").is_ok()
    {
        eprintln!("Error: Cannot send without identity.");
        eprintln!("Run 'hcom start' first, then use 'hcom send'.");
        return 1;
    }

    // ── Create bundle event if inline flags provided ──
    if let Some(mut bundle) = bundle_data {
        if let Err(e) = crate::core::bundles::validate_bundle(&mut bundle) {
            eprintln!("Error: {e}");
            return 1;
        }

        let bundle_instance = match sender_identity.kind {
            SenderKind::External => format!("ext_{}", sender_identity.name),
            SenderKind::System => format!("sys_{}", sender_identity.name),
            SenderKind::Instance => sender_identity.name.clone(),
        };

        match crate::core::bundles::create_bundle_event(
            &mut bundle,
            &bundle_instance,
            Some(&sender_identity.name),
            db,
        ) {
            Ok(bundle_id) => {
                crate::relay::worker::ensure_worker(true);
                envelope.bundle_id = Some(bundle_id.clone());

                // Append bundle summary text to message
                let refs = bundle.get("refs").cloned().unwrap_or(serde_json::json!({}));
                let events = refs
                    .get("events")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().or(v.as_i64().map(|_| "")).or(Some("")))
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let files = refs
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let transcript = refs
                    .get("transcript")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| {
                                if let Some(obj) = v.as_object() {
                                    Some(format!(
                                        "{}:{}",
                                        obj.get("range").and_then(|r| r.as_str()).unwrap_or(""),
                                        obj.get("detail").and_then(|d| d.as_str()).unwrap_or("")
                                    ))
                                } else {
                                    v.as_str().map(|s| s.to_string())
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();

                let title = bundle.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let description = bundle
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let extends = bundle.get("extends").and_then(|v| v.as_str());

                let mut bundle_lines = vec![
                    format!("[Bundle {bundle_id}]"),
                    format!("Title: {title}"),
                    format!("Description: {description}"),
                    "Refs:".to_string(),
                    format!("  events: {events}"),
                    format!("  files: {files}"),
                    format!("  transcript: {transcript}"),
                ];
                if let Some(ext) = extends {
                    bundle_lines.push(format!("Extends: {ext}"));
                }
                bundle_lines.push(String::new());
                bundle_lines.push("View bundle:".to_string());
                bundle_lines.push(format!("  hcom bundle cat {bundle_id}"));

                message = format!("{}\n\n{}", message.trim_end(), bundle_lines.join("\n"));
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    }

    // ── Send message ──
    let has_envelope = envelope.intent.is_some()
        || envelope.reply_to.is_some()
        || envelope.thread.is_some()
        || envelope.bundle_id.is_some();
    let targets_to_pass: Option<&[String]> =
        if args.has_separator() || !effective_targets.is_empty() {
            Some(&effective_targets)
        } else {
            None
        };

    let delivered_to = match send_message(
        db,
        &sender_identity,
        &message,
        if has_envelope { Some(&envelope) } else { None },
        targets_to_pass,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // ── Feedback ──
    if args.quiet {
        crate::relay::worker::ensure_worker(true);
        return 0;
    }

    let feedback = get_recipient_feedback(db, &delivered_to);

    // Show unread messages if instance context (full delivery with cursor advance)
    if matches!(sender_identity.kind, SenderKind::Instance) {
        let messages = db.get_unread_messages(&sender_identity.name);
        if !messages.is_empty() {
            // Advance cursor
            if let Some(last) = messages.last() {
                if let Some(id) = last.event_id {
                    let mut updates = serde_json::Map::new();
                    updates.insert("last_event_id".into(), serde_json::json!(id));
                    instances::update_instance_position(db, &sender_identity.name, &updates);
                }
            }

            // Separate subagent messages from main messages
            let subagent_names: std::collections::HashSet<String> = db
                .conn()
                .prepare("SELECT name FROM instances WHERE parent_name = ?")
                .ok()
                .map(|mut stmt| {
                    stmt.query_map(rusqlite::params![&sender_identity.name], |row| row.get(0))
                        .ok()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.ok())
                        .collect()
                })
                .unwrap_or_default();

            let mut main_msgs = Vec::new();
            let mut sub_msgs = Vec::new();
            for msg in &messages {
                if subagent_names.contains(&msg.from) {
                    sub_msgs.push(msg);
                } else {
                    main_msgs.push(msg);
                }
            }

            const MAX_MSGS: usize = 50;

            print!("{feedback}");
            if !main_msgs.is_empty() {
                let capped: Vec<&_> = main_msgs.iter().take(MAX_MSGS).copied().collect();
                let formatted = format_messages_for_hook(db, &capped, &sender_identity.name);
                println!("\n{formatted}");
            }
            if !sub_msgs.is_empty() {
                let capped: Vec<&_> = sub_msgs.iter().take(MAX_MSGS).copied().collect();
                let formatted = format_messages_for_hook(db, &capped, &sender_identity.name);
                println!("\n[Subagent messages]\n{formatted}");
            }
            if main_msgs.is_empty() && sub_msgs.is_empty() {
                println!();
            }
        } else {
            println!("{feedback}");
        }
    } else {
        println!("{feedback}");
    }

    // Adhoc unread delivery: for --name instances, show unread preview
    if explicit_name.is_some() && matches!(sender_identity.kind, SenderKind::Instance) {
        let messages = db.get_unread_messages(&sender_identity.name);
        if !messages.is_empty() {
            println!("\n{}", "─".repeat(40));
            println!("[hcom] new message(s)");
            println!("{}", "─".repeat(40));
            println!("\nRun: hcom listen --name {}", sender_identity.name);
        }
    }

    // Show intent tip
    if let Some(ref intent) = envelope.intent {
        if matches!(sender_identity.kind, SenderKind::Instance) {
            let tip_key = format!("send:intent:{}", intent.as_str());
            crate::core::tips::maybe_show_tip(db, &sender_identity.name, &tip_key, false);
        }
    }

    crate::relay::worker::ensure_worker(true);

    0
}

/// Format messages for hook display (no ANSI).
fn format_messages_for_hook(
    db: &HcomDb,
    messages: &[&crate::db::Message],
    instance_name: &str,
) -> String {
    let recipient_display = instances::get_display_name(db, instance_name);

    if messages.len() == 1 {
        let msg = messages[0];
        let sender_display = instances::get_display_name(db, &msg.from);
        let prefix =
            cli_context_build_prefix(msg.intent.as_deref(), msg.thread.as_deref(), msg.event_id);
        format!(
            "{prefix} {sender_display} → {recipient_display}: {}",
            msg.text
        )
    } else {
        let parts: Vec<String> = messages
            .iter()
            .map(|msg| {
                let sender_display = instances::get_display_name(db, &msg.from);
                let prefix = cli_context_build_prefix(
                    msg.intent.as_deref(),
                    msg.thread.as_deref(),
                    msg.event_id,
                );
                format!(
                    "{prefix} {sender_display} → {recipient_display}: {}",
                    msg.text
                )
            })
            .collect();
        format!("[{} new messages] | {}", parts.len(), parts.join(" | "))
    }
}

fn cli_context_build_prefix(
    intent: Option<&str>,
    thread: Option<&str>,
    event_id: Option<i64>,
) -> String {
    let id_ref = event_id.map(|id| format!("#{id}")).unwrap_or_default();
    let prefix = match (intent, thread) {
        (Some(i), Some(t)) => format!("{i}:{t}"),
        (Some(i), None) => i.to_string(),
        (None, Some(t)) => format!("thread:{t}"),
        (None, None) => "new message".to_string(),
    };
    if id_ref.is_empty() {
        format!("[{prefix}]")
    } else {
        format!("[{prefix} {id_ref}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn parse_basic_send() {
        let args = SendArgs::try_parse_from(["send", "@luna", "--", "hello", "there"]).unwrap();
        assert_eq!(args.positionals, vec!["@luna"]);
        assert_eq!(args.message, vec!["hello", "there"]);
    }

    #[test]
    fn parse_multiple_targets() {
        let args = SendArgs::try_parse_from(["send", "@luna", "@nova", "--", "hello"]).unwrap();
        assert_eq!(args.positionals, vec!["@luna", "@nova"]);
        assert_eq!(args.message, vec!["hello"]);
    }

    #[test]
    fn parse_broadcast() {
        let args = SendArgs::try_parse_from(["send", "--", "broadcast", "msg"]).unwrap();
        assert!(args.positionals.is_empty());
        assert_eq!(args.message, vec!["broadcast", "msg"]);
    }

    #[test]
    fn parse_with_intent_flag() {
        let args =
            SendArgs::try_parse_from(["send", "--intent", "request", "@luna", "--", "hello"])
                .unwrap();
        assert_eq!(args.intent.as_deref(), Some("request"));
        assert_eq!(args.positionals, vec!["@luna"]);
    }

    #[test]
    fn parse_flags_after_targets() {
        let args =
            SendArgs::try_parse_from(["send", "@luna", "--intent", "request", "--", "hello"])
                .unwrap();
        assert_eq!(args.intent.as_deref(), Some("request"));
        assert_eq!(args.positionals, vec!["@luna"]);
    }

    #[test]
    fn parse_bigboss_flag() {
        let args = SendArgs::try_parse_from(["send", "-b", "--", "hello"]).unwrap();
        assert!(args.bigboss);
        assert_eq!(args.sender_name(), Some("bigboss".to_string()));
    }

    #[test]
    fn parse_from_overrides_bigboss() {
        let args =
            SendArgs::try_parse_from(["send", "-b", "--from", "reviewer", "--", "hello"]).unwrap();
        assert_eq!(args.sender_name(), Some("reviewer".to_string()));
    }

    #[test]
    fn parse_stdin_flag() {
        let args = SendArgs::try_parse_from(["send", "--stdin", "@luna"]).unwrap();
        assert!(args.stdin);
        assert_eq!(args.positionals, vec!["@luna"]);
        assert!(args.message.is_empty());
    }

    #[test]
    fn parse_file_flag() {
        let args = SendArgs::try_parse_from(["send", "--file", "/tmp/msg.txt", "@luna"]).unwrap();
        assert_eq!(args.file.as_deref(), Some("/tmp/msg.txt"));
        assert_eq!(args.positionals, vec!["@luna"]);
    }

    #[test]
    fn parse_base64_flag() {
        let args = SendArgs::try_parse_from(["send", "--base64", "aGVsbG8=", "@luna"]).unwrap();
        assert_eq!(args.base64.as_deref(), Some("aGVsbG8="));
    }

    #[test]
    fn parse_reply_to_and_thread() {
        let args = SendArgs::try_parse_from([
            "send",
            "--reply-to",
            "42",
            "--thread",
            "pr-99",
            "@luna",
            "--",
            "hi",
        ])
        .unwrap();
        assert_eq!(args.reply_to.as_deref(), Some("42"));
        assert_eq!(args.thread.as_deref(), Some("pr-99"));
    }

    #[test]
    fn parse_quiet_flag() {
        let args = SendArgs::try_parse_from(["send", "--quiet", "-b", "--", "hi"]).unwrap();
        assert!(args.quiet);
    }

    #[test]
    fn parse_inline_bundle_flags() {
        let args = SendArgs::try_parse_from([
            "send",
            "-b",
            "--title",
            "my-bundle",
            "--description",
            "desc",
            "--events",
            "1-10",
            "--files",
            "a.py",
            "--transcript",
            "1-5:normal",
            "--",
            "msg",
        ])
        .unwrap();
        assert_eq!(args.title.as_deref(), Some("my-bundle"));
        assert_eq!(args.description.as_deref(), Some("desc"));
        assert_eq!(args.events.as_deref(), Some("1-10"));
        assert_eq!(args.files.as_deref(), Some("a.py"));
        assert_eq!(args.transcript.as_deref(), Some("1-5:normal"));
    }

    #[test]
    fn parse_no_separator_targets_only() {
        let args = SendArgs::try_parse_from(["send", "@luna"]).unwrap();
        assert_eq!(args.positionals, vec!["@luna"]);
        assert!(args.message.is_empty());
    }

    #[test]
    fn parse_compat_at_name_with_space() {
        // Backward compat: '@luna hi' as a single quoted arg
        // process_positionals treats as full message text
        let args = SendArgs::try_parse_from(["send", "@luna hi"]).unwrap();
        assert_eq!(args.positionals, vec!["@luna hi"]);
    }

    #[test]
    fn parse_bare_text_accepted() {
        // Bare text without @ is accepted by clap; cmd_send handles as message
        let args = SendArgs::try_parse_from(["send", "hello everyone"]).unwrap();
        assert_eq!(args.positionals, vec!["hello everyone"]);
    }

    #[test]
    fn parse_empty_target_rejected() {
        let result = SendArgs::try_parse_from(["send", "@", "--", "hi"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_bare_text_with_separator_accepted() {
        // Bare text before -- is accepted by clap; validated in cmd_send
        let args = SendArgs::try_parse_from(["send", "luna", "--", "hi"]).unwrap();
        assert_eq!(args.positionals, vec!["luna"]);
        assert_eq!(args.message, vec!["hi"]);
    }

    #[test]
    fn parse_message_with_dashes() {
        let args =
            SendArgs::try_parse_from(["send", "@luna", "--", "--this", "is", "a", "message"])
                .unwrap();
        assert_eq!(args.message, vec!["--this", "is", "a", "message"]);
    }

    #[test]
    fn parse_extends_flag() {
        let args = SendArgs::try_parse_from([
            "send",
            "-b",
            "--title",
            "t",
            "--extends",
            "abc123",
            "--",
            "msg",
        ])
        .unwrap();
        assert_eq!(args.extends.as_deref(), Some("abc123"));
    }

    // ── process_positionals tests ──

    #[test]
    fn process_empty() {
        let (targets, msg) = process_positionals(&[]);
        assert!(targets.is_empty());
        assert!(msg.is_none());
    }

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_send_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (db, db_path)
    }

    fn cleanup_test_db(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        let _ = std::fs::remove_file(wal);
        let _ = std::fs::remove_file(shm);
    }

    #[test]
    fn send_message_threads_seed_and_reuse_memberships() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0), ('miso', 1000.0)",
                [],
            )
            .unwrap();

        let sender = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        let envelope = MessageEnvelope {
            thread: Some("debate-1".into()),
            ..Default::default()
        };

        let delivered = send_message(
            &db,
            &sender,
            "hello",
            Some(&envelope),
            Some(&["nova".to_string(), "miso".to_string()]),
        )
        .unwrap();
        assert_eq!(delivered, vec!["nova".to_string(), "miso".to_string()]);

        let members = db.get_thread_members("debate-1");
        assert_eq!(
            members,
            vec!["nova".to_string(), "miso".to_string(), "luna".to_string()]
        );

        let delivered = send_message(&db, &sender, "round 2", Some(&envelope), None).unwrap();
        assert_eq!(delivered, vec!["nova".to_string(), "miso".to_string()]);

        cleanup_test_db(path);
    }

    #[test]
    fn send_message_thread_without_members_errors() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        let sender = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        let envelope = MessageEnvelope {
            thread: Some("empty-thread".into()),
            ..Default::default()
        };

        let err = send_message(&db, &sender, "hello", Some(&envelope), None).unwrap_err();
        assert!(err.contains("has no members"));

        cleanup_test_db(path);
    }

    #[test]
    fn send_message_external_sender_does_not_auto_subscribe_to_thread() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let sender = SenderIdentity {
            kind: SenderKind::External,
            name: "bigboss".into(),
            instance_data: None,
            session_id: None,
        };
        let envelope = MessageEnvelope {
            thread: Some("ops".into()),
            ..Default::default()
        };

        let delivered = send_message(
            &db,
            &sender,
            "hello",
            Some(&envelope),
            Some(&["nova".to_string()]),
        )
        .unwrap();
        assert_eq!(delivered, vec!["nova".to_string()]);
        assert_eq!(db.get_thread_members("ops"), vec!["nova".to_string()]);

        cleanup_test_db(path);
    }

    #[test]
    fn send_message_thread_request_does_not_create_request_watch_rows() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let sender = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        let seed_envelope = MessageEnvelope {
            thread: Some("ops".into()),
            ..Default::default()
        };
        send_message(
            &db,
            &sender,
            "seed",
            Some(&seed_envelope),
            Some(&["nova".to_string()]),
        )
        .unwrap();

        let request_envelope = MessageEnvelope {
            intent: Some(crate::messages::MessageIntent::Request),
            thread: Some("ops".into()),
            ..Default::default()
        };
        let delivered =
            send_message(&db, &sender, "status?", Some(&request_envelope), None).unwrap();
        assert_eq!(delivered, vec!["nova".to_string()]);

        let reqwatch_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE key LIKE 'events_sub:reqwatch-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reqwatch_count, 0);

        let (scope, mentions_json): (String, String) = db
            .conn()
            .query_row(
                "SELECT json_extract(data, '$.scope'), json_extract(data, '$.mentions')
                 FROM events
                 WHERE type = 'message'
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(scope, "mentions");
        assert!(mentions_json.contains("nova"));

        cleanup_test_db(path);
    }

    #[test]
    fn process_compat_at_with_space() {
        // "@luna hi" → full text as message, no targets
        let (targets, msg) = process_positionals(&["@luna hi".to_string()]);
        assert!(targets.is_empty());
        assert_eq!(msg.as_deref(), Some("@luna hi"));
    }

    #[test]
    fn process_bare_text() {
        // "hello everyone" → message text, no targets (broadcast)
        let (targets, msg) = process_positionals(&["hello everyone".to_string()]);
        assert!(targets.is_empty());
        assert_eq!(msg.as_deref(), Some("hello everyone"));
    }

    #[test]
    fn process_pure_targets() {
        // "@luna" → target, no message
        let (targets, msg) = process_positionals(&["@luna".to_string()]);
        assert_eq!(targets, vec!["luna"]);
        assert!(msg.is_none());
    }

    #[test]
    fn process_target_plus_bare_text() {
        // "@luna", "hello" → target + message
        let (targets, msg) = process_positionals(&["@luna".to_string(), "hello".to_string()]);
        assert_eq!(targets, vec!["luna"]);
        assert_eq!(msg.as_deref(), Some("hello"));
    }
}
