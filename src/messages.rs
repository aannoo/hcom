//! Message operations — routing, scope computation, and delivery formatting.

use crate::shared::{MAX_MESSAGE_SIZE, SENDER, extract_mentions};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Precompiled regex for @[hcom-*] system notification mentions.
static SYSTEM_BRACKET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@\[hcom-[a-z]+\]").unwrap());

/// Message scope: broadcast (everyone) or mentions (targeted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageScope {
    Broadcast,
    Mentions,
}

impl MessageScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageScope::Broadcast => "broadcast",
            MessageScope::Mentions => "mentions",
        }
    }
}

impl std::str::FromStr for MessageScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "broadcast" => Ok(MessageScope::Broadcast),
            "mentions" => Ok(MessageScope::Mentions),
            _ => Err(format!("invalid message scope: {s}")),
        }
    }
}

/// Message intent for envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageIntent {
    Request,
    Inform,
    Ack,
}

impl MessageIntent {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageIntent::Request => "request",
            MessageIntent::Inform => "inform",
            MessageIntent::Ack => "ack",
        }
    }
}

impl std::str::FromStr for MessageIntent {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "request" => Ok(MessageIntent::Request),
            "inform" => Ok(MessageIntent::Inform),
            "ack" => Ok(MessageIntent::Ack),
            _ => Err(format!("invalid message intent: {s}")),
        }
    }
}

/// Optional envelope fields for messages.
#[derive(Debug, Clone, Default)]
pub struct MessageEnvelope {
    pub intent: Option<MessageIntent>,
    pub reply_to: Option<String>,
    pub thread: Option<String>,
    pub bundle_id: Option<String>,
}

/// Relay metadata for cross-device messages.
#[derive(Debug, Clone)]
pub struct RelayMetadata {
    pub id: String,
    pub short: String,
}

/// Scope computation result.
#[derive(Debug, Clone)]
pub struct ScopeResult {
    pub scope: MessageScope,
    /// For Mentions scope: list of base names targeted.
    pub mentions: Vec<String>,
}

/// Read receipt for a sent message.
#[derive(Debug, Clone)]
pub struct ReadReceipt {
    pub id: i64,
    pub age: String,
    pub text: String,
    pub read_by: Vec<String>,
    pub total_recipients: usize,
}

/// Instance info for scope computation (name + optional tag).
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub name: String,
    pub tag: Option<String>,
    pub project: Option<String>,
}

impl InstanceInfo {
    /// Full display name: "{tag}-{name}" if tag, else just "{name}".
    pub fn full_name(&self) -> String {
        match &self.tag {
            Some(tag) if !tag.is_empty() => format!("{}-{}", tag, self.name),
            _ => self.name.clone(),
        }
    }
}

// validate_scope and validate_intent live in core::helpers — re-export for consumers.
pub use crate::core::helpers::{validate_intent, validate_scope};

/// Validate message content and size.
pub fn validate_message(message: &str) -> Result<(), String> {
    if message.is_empty() || message.trim().is_empty() {
        return Err("Message required".to_string());
    }

    // Reject control characters (except \n, \r, \t)
    for ch in message.chars() {
        if ('\x00'..='\x08').contains(&ch)
            || ('\x0B'..='\x0C').contains(&ch)
            || ('\x0E'..='\x1F').contains(&ch)
            || ('\u{0080}'..='\u{009F}').contains(&ch)
        {
            return Err("Message contains control characters".to_string());
        }
    }

    if message.len() > MAX_MESSAGE_SIZE {
        return Err(format!(
            "Message too large (max {} chars)",
            MAX_MESSAGE_SIZE
        ));
    }

    Ok(())
}

/// Format recipients list for display.
///
/// "luna, nova" or "luna, nova, kira (+2 more)" or "(none)"
pub fn format_recipients(delivered_to: &[String], max_show: usize) -> String {
    if delivered_to.is_empty() {
        return "(none)".to_string();
    }

    if delivered_to.len() > max_show {
        let shown: Vec<&str> = delivered_to[..max_show]
            .iter()
            .map(|s| s.as_str())
            .collect();
        let remaining = delivered_to.len() - max_show;
        format!("{} (+{} more)", shown.join(", "), remaining)
    } else {
        delivered_to.join(", ")
    }
}

/// Build the "unknown @mention" error string with a "Did you mean" hint when
/// an unmatched target (without `:`) has the same base name as a remote agent.
///
/// Without this hint, users hit `@zeli` → "non-existent" even though `zeli:ZOME`
/// is right there in the available list and only takes a colon-suffix to reach.
fn build_unmatched_error(unmatched: &[String], full_names: &[String]) -> String {
    let unmatched_display: Vec<String> = unmatched.iter().map(|t| format!("@{}", t)).collect();

    let mut suggestions: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for target in unmatched {
        if target.contains(':') {
            continue;
        }
        let target_lower = target.to_lowercase();
        for fn_ in full_names {
            if let Some((prefix, _device)) = fn_.split_once(':') {
                if prefix.to_lowercase() == target_lower && seen.insert(fn_.clone()) {
                    suggestions.push(format!("@{}", fn_));
                }
            }
        }
    }

    let mut msg = format!(
        "@mentions to non-existent or stopped agents (or you used '@' char for stuff that wasn't agent name): {}",
        unmatched_display.join(", "),
    );
    if !suggestions.is_empty() {
        msg.push_str(&format!("\nDid you mean: {}?", suggestions.join(", ")));
    }
    msg.push_str(&format!(
        "\nAvailable: {}",
        format_recipients(full_names, 30)
    ));
    msg
}

/// Match a target against instance names with base-name fallback.
///
/// Tries prefix match on full display name ({tag}-{name}) first.
/// Falls back to prefix match on base name ({name} only) if no full-name match.
///
/// Special case: bigboss:SUFFIX resolves to bigboss (virtual identity, device-agnostic).
fn match_target(
    target: &str,
    full_names: &[String],
    full_to_base: &HashMap<String, String>,
) -> Vec<String> {
    let target_lower = target.to_lowercase();

    // bigboss is device-agnostic — strip any remote suffix
    if target_lower.starts_with("bigboss:") {
        if full_names.iter().any(|fn_| fn_ == "bigboss")
            || full_to_base.values().any(|bn| bn == "bigboss")
        {
            return vec!["bigboss".to_string()];
        }
        return vec![];
    }

    if target.contains(':') {
        // Remote target — match any instance with prefix
        return full_names
            .iter()
            .filter(|fn_| fn_.to_lowercase().starts_with(&target_lower))
            .filter_map(|fn_| full_to_base.get(fn_.as_str()).cloned())
            .collect();
    }

    // Local target — prefix match on full display name
    let matches: Vec<String> = full_names
        .iter()
        .filter(|fn_| {
            !fn_.contains(':')
                && fn_.to_lowercase().starts_with(&target_lower)
                && (fn_.len() == target.len() || fn_.as_bytes().get(target.len()) != Some(&b'_'))
        })
        .filter_map(|fn_| full_to_base.get(fn_.as_str()).cloned())
        .collect();

    if !matches.is_empty() {
        return matches;
    }

    // Fallback: prefix match on base name (ignoring tag prefix)
    full_names
        .iter()
        .filter(|fn_| {
            if fn_.contains(':') {
                return false;
            }
            if let Some(base) = full_to_base.get(fn_.as_str()) {
                let base_lower = base.to_lowercase();
                base_lower.starts_with(&target_lower)
                    && (base.len() == target.len()
                        || base.as_bytes().get(target.len()) != Some(&b'_'))
            } else {
                false
            }
        })
        .filter_map(|fn_| full_to_base.get(fn_.as_str()).cloned())
        .collect()
}

/// Compute message scope and routing data.
///
/// Returns Ok((scope_result, None)) on success, Ok((None, error)) on validation failure.
///
/// Scope types:
/// - Broadcast: No targets → everyone
/// - Mentions: Has targets → explicit targets only
///
/// STRICT FAILURE: Targets that don't match enabled instances return error.
pub fn compute_scope(
    message: &str,
    enabled_instances: &[InstanceInfo],
    explicit_targets: Option<&[String]>,
    sender_project: Option<&str>,
) -> Result<ScopeResult, String> {
    // Filter instances by project isolation
    let enabled_instances: Vec<&InstanceInfo> = if let Some(proj) = sender_project {
        let proj = proj.trim();
        if proj.is_empty() {
            enabled_instances.iter().collect()
        } else {
            enabled_instances
                .iter()
                .filter(|inst| {
                    inst.project
                        .as_deref()
                        .map(|p| p == proj)
                        .unwrap_or(true) // include instances with no project
                })
                .collect()
        }
    } else {
        enabled_instances.iter().collect()
    };

    // Build full name lookup: {full_name: base_name}
    let mut full_to_base: HashMap<String, String> = HashMap::new();
    let mut full_names: Vec<String> = Vec::new();

    for inst in enabled_instances {
        let full = inst.full_name();
        full_to_base.insert(full.clone(), inst.name.clone());
        full_names.push(full);
    }

    // Also add bigboss as a plain string target
    full_to_base.insert(SENDER.to_string(), SENDER.to_string());
    full_names.push(SENDER.to_string());

    // If explicit targets specified (via -- separator), use them instead of parsing @mentions
    if let Some(targets) = explicit_targets {
        if !targets.is_empty() {
            let mut matched_base_names: Vec<String> = Vec::new();
            let mut unmatched: Vec<String> = Vec::new();

            for target in targets {
                let matches = match_target(target, &full_names, &full_to_base);
                if matches.is_empty() {
                    unmatched.push(target.clone());
                } else {
                    matched_base_names.extend(matches);
                }
            }

            if !unmatched.is_empty() {
                return Err(build_unmatched_error(&unmatched, &full_names));
            }

            // Deduplicate preserving order
            let unique = dedup_preserving_order(&matched_base_names);
            if !unique.is_empty() {
                return Ok(ScopeResult {
                    scope: MessageScope::Mentions,
                    mentions: unique,
                });
            }
        }

        // Empty explicit_targets or no matches = broadcast
        return Ok(ScopeResult {
            scope: MessageScope::Broadcast,
            mentions: vec![],
        });
    }

    // No explicit targets (None) — check for @mentions in message text
    if message.contains('@') {
        // Check for invalid system notification mention attempts like @[hcom-events]
        let system_attempts: Vec<&str> = SYSTEM_BRACKET_RE
            .find_iter(message)
            .map(|m| m.as_str())
            .collect();
        if !system_attempts.is_empty() {
            return Err(format!(
                "System notifications cannot be mentioned: {}\nSystem notifications (names in []) are not agents and cannot receive messages.",
                system_attempts.join(", "),
            ));
        }

        let mentions = extract_mentions(message);
        if !mentions.is_empty() {
            let mut matched_base_names: Vec<String> = Vec::new();
            let mut unmatched: Vec<String> = Vec::new();

            for mention in &mentions {
                let matches = match_target(mention, &full_names, &full_to_base);
                if matches.is_empty() {
                    unmatched.push(mention.clone());
                } else {
                    matched_base_names.extend(matches);
                }
            }

            // STRICT: fail on unmatched mentions
            if !unmatched.is_empty() {
                // Special cases: literal "@mention", "@name", or "@mentions"
                let special_literals: HashSet<&str> =
                    ["mention", "name", "mentions"].iter().copied().collect();
                let literal_matches: Vec<&String> = unmatched
                    .iter()
                    .filter(|m| special_literals.contains(m.as_str()))
                    .collect();

                if !literal_matches.is_empty() {
                    let literal_text = if literal_matches.len() == 1 {
                        format!("@{}", literal_matches[0])
                    } else {
                        literal_matches
                            .iter()
                            .map(|m| format!("@{}", m))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    return Err(format!(
                        "The literal text {} is not a valid target - use actual instance names",
                        literal_text,
                    ));
                }

                return Err(build_unmatched_error(&unmatched, &full_names));
            }

            let unique = dedup_preserving_order(&matched_base_names);
            return Ok(ScopeResult {
                scope: MessageScope::Mentions,
                mentions: unique,
            });
        }
    }

    // No @mentions → broadcast to everyone
    Ok(ScopeResult {
        scope: MessageScope::Broadcast,
        mentions: vec![],
    })
}

/// Deduplicate a list preserving insertion order.
fn dedup_preserving_order(items: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            result.push(item.clone());
        }
    }
    result
}

/// Check if message should be delivered based on scope.
///
/// Returns true if receiver should get the message.
pub fn should_deliver_message(
    event_data: &Value,
    receiver_name: &str,
    sender_name: &str,
) -> Result<bool, String> {
    if receiver_name == sender_name {
        return Ok(false);
    }

    let scope = event_data
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Message missing 'scope' field (old format)".to_string())?;

    validate_scope(scope)?;

    match scope {
        "broadcast" => Ok(true),
        "mentions" => {
            let mentions = event_data
                .get("mentions")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();

            // Strip device suffix for cross-device matching
            let receiver_base = receiver_name.split(':').next().unwrap_or(receiver_name);
            Ok(mentions
                .iter()
                .any(|m| receiver_base == m.split(':').next().unwrap_or(m)))
        }
        _ => Ok(false),
    }
}

/// Build message prefix from envelope fields.
///
/// Format: [intent:thread #id] or [intent #id] or [thread:name #id] or [new message #id]
/// Remote messages: #id:DEVICE
fn build_message_prefix(msg: &Value) -> String {
    let intent = msg.get("intent").and_then(|v| v.as_str());
    let thread = msg.get("thread").and_then(|v| v.as_str());
    let event_id = msg.get("event_id").and_then(|v| v.as_i64());
    let relay = msg.get("_relay");

    // Build ID reference (local or remote)
    let id_ref = if let Some(relay) = relay {
        let short = relay.get("short").and_then(|v| v.as_str()).unwrap_or("");
        let rid = relay.get("id");
        if !short.is_empty()
            && let Some(rid_val) = rid
        {
            let rid_str = match rid_val {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            };
            if !rid_str.is_empty() {
                format!("#{}:{}", rid_str, short)
            } else {
                String::new()
            }
        } else {
            event_id.map(|id| format!("#{}", id)).unwrap_or_default()
        }
    } else {
        event_id.map(|id| format!("#{}", id)).unwrap_or_default()
    };

    // Build prefix based on envelope fields
    let prefix = match (intent, thread) {
        (Some(i), Some(t)) => format!("{}:{}", i, t),
        (Some(i), None) => i.to_string(),
        (None, Some(t)) => format!("thread:{}", t),
        (None, None) => "new message".to_string(),
    };

    if !id_ref.is_empty() {
        format!("[{} {}]", prefix, id_ref)
    } else {
        format!("[{}]", prefix)
    }
}

/// Format messages for hook feedback.
///
/// Single message uses verbose format: "sender → recipient + N others"
/// Multiple messages use compact format: "sender → recipient (+N)"
///
/// `instance_name`: base name of the receiving instance.
/// `get_instance_data`: callback to get instance data by name (for tag lookup).
/// `get_config_hints`: callback to get config hints.
/// `tip_checker`: optional callback for tip system (has_seen, mark_seen).
#[allow(clippy::type_complexity)]
pub fn format_hook_messages(
    messages: &[Value],
    instance_name: &str,
    get_instance_data: &dyn Fn(&str) -> Option<Value>,
    get_config_hints: &dyn Fn() -> String,
    tip_checker: Option<&dyn Fn(&str, &str) -> (bool, Box<dyn Fn()>)>,
) -> String {
    let recipient_display = get_display_name_from_data(instance_name, get_instance_data);

    let get_sender_display = |sender_base: &str| -> String {
        if let Some(data) = get_instance_data(sender_base) {
            get_full_name_from_value(&data)
        } else {
            sender_base.to_string()
        }
    };

    let reason = if messages.len() == 1 {
        let msg = &messages[0];
        let others = others_count(msg);
        let recipient = if others > 0 {
            let suffix = if others > 1 { "s" } else { "" };
            format!("{} (+{} other{})", recipient_display, others, suffix)
        } else {
            recipient_display.clone()
        };
        let prefix = build_message_prefix(msg);
        let sender_name = msg
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let sender_display = get_sender_display(sender_name);
        let text = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
        format!("{} {} → {}: {}", prefix, sender_display, recipient, text)
    } else {
        let parts: Vec<String> = messages
            .iter()
            .map(|msg| {
                let others = others_count(msg);
                let recipient = if others > 0 {
                    format!("{} (+{})", recipient_display, others)
                } else {
                    recipient_display.clone()
                };
                let prefix = build_message_prefix(msg);
                let sender_name = msg
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let sender_display = get_sender_display(sender_name);
                let text = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
                format!("{} {} → {}: {}", prefix, sender_display, recipient, text)
            })
            .collect();
        format!("[{} new messages] | {}", messages.len(), parts.join(" | "))
    };

    // Append hints
    let mut result = reason;

    // Per-instance hints from data
    let mut hints = String::new();
    if let Some(data) = get_instance_data(instance_name) {
        if let Some(h) = data.get("hints").and_then(|v| v.as_str()) {
            if !h.is_empty() {
                hints = h.to_string();
            }
        }
    }
    if hints.is_empty() {
        hints = get_config_hints();
    }
    if !hints.is_empty() {
        result = format!("{} | [{}]", result, hints);
    }

    // Show recv:thread tip on first receipt in each thread
    if let Some(tip_fn) = tip_checker {
        for msg in messages {
            if let Some(thread) = msg.get("thread").and_then(|v| v.as_str()) {
                let tip_key = format!("recv:thread:{thread}");
                let (seen, mark) = tip_fn(instance_name, &tip_key);
                if !seen {
                    mark();
                    result = format!("{}\n{}", result, get_thread_tip_text(instance_name, thread));
                    return result;
                }
            }
        }

        // Show recv:intent tip on first receipt of each intent type
        for msg in messages {
            if let Some(intent) = msg.get("intent").and_then(|v| v.as_str()) {
                let tip_key = format!("recv:intent:{}", intent);
                let (seen, mark) = tip_fn(instance_name, &tip_key);
                if !seen {
                    if let Some(tip_text) = get_tip_text(&tip_key) {
                        mark();
                        result = format!("{}\n{}", result, tip_text);
                        break; // Only show one tip per delivery
                    }
                }
            }
        }
    }

    result
}

/// Format messages for model injection — wraps in <hcom> tags.
#[allow(clippy::type_complexity)]
pub fn format_messages_json(
    messages: &[Value],
    instance_name: &str,
    get_instance_data: &dyn Fn(&str) -> Option<Value>,
    get_config_hints: &dyn Fn() -> String,
    tip_checker: Option<&dyn Fn(&str, &str) -> (bool, Box<dyn Fn()>)>,
) -> String {
    let formatted = format_hook_messages(
        messages,
        instance_name,
        get_instance_data,
        get_config_hints,
        tip_checker,
    );
    format!("<hcom>{}</hcom>", formatted)
}

/// Get full name from instance data Value.
fn get_full_name_from_value(data: &Value) -> String {
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let tag = data.get("tag").and_then(|v| v.as_str()).unwrap_or("");
    if !tag.is_empty() {
        format!("{}-{}", tag, name)
    } else {
        name.to_string()
    }
}

/// Get display name for an instance by looking up its data.
fn get_display_name_from_data(
    base_name: &str,
    get_instance_data: &dyn Fn(&str) -> Option<Value>,
) -> String {
    if let Some(data) = get_instance_data(base_name) {
        let full = get_full_name_from_value(&data);
        if !full.is_empty() {
            return full;
        }
    }
    base_name.to_string()
}

/// Count other recipients (excluding self) from a message.
fn others_count(msg: &Value) -> usize {
    msg.get("delivered_to")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len().saturating_sub(1))
        .unwrap_or(0)
}

/// Tip text for recv:intent tips. Delegates to core::tips for centralized text.
fn get_tip_text(tip_key: &str) -> Option<&'static str> {
    crate::core::tips::get_tip(tip_key)
}

fn get_thread_tip_text(instance_name: &str, thread: &str) -> String {
    let sub_id = crate::db::subscriptions::thread_membership_sub_id(thread, instance_name);
    format!(
        "[tip] You joined thread {thread}. To leave: hcom events unsub {sub_id} (find your sub-id with: hcom events sub list)"
    )
}

/// Remove bash escape sequences from message content.
///
/// Bash escapes special characters when constructing commands. Since hcom
/// receives messages as command arguments, we unescape common sequences
/// that don't affect the actual message intent.
///
/// NOTE: We do NOT unescape '\\\\' to '\\'. If double backslashes survived
/// bash processing, the user intended them (e.g., Windows paths, regex, JSON).
pub fn unescape_bash(text: &str) -> String {
    text.replace("\\!", "!")
        .replace("\\$", "$")
        .replace("\\`", "`")
        .replace("\\\"", "\"")
        .replace("\\'", "'")
}

/// Check if instance data represents an external sender.
///
/// External senders have empty/null session_id, no parent_session_id,
/// and no origin_device_id.
fn is_external_sender_data(data: &Value) -> bool {
    // Remote instances are not external
    if data
        .get("origin_device_id")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return false;
    }
    // Subagents have parent_session_id, so are not external
    if data
        .get("parent_session_id")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return false;
    }
    // External = no session_id
    let session_id = data
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    session_id.is_empty()
}

/// Compute read receipts from pre-fetched data.
///
/// This is a pure function that takes all needed data as parameters
/// (no DB access). The caller is responsible for querying the DB.
///
/// # Arguments
/// * `sent_messages` - Messages sent by this identity: (id, timestamp, data_json)
/// * `active_instances` - All active instances except sender: {name: {tag, origin_device_id, ...}}
/// * `deliver_events` - Set of instance names that have deliver events after each message
/// * `remote_msg_ts` - For remote instances: {name: latest msg_ts}
/// * `max_text_length` - Max text length before truncation
/// * `format_age_fn` - Function to format seconds as age string
#[allow(clippy::too_many_arguments)]
pub fn compute_read_receipts(
    sent_messages: &[(i64, String, Value)],
    active_instances: &HashMap<String, Value>,
    deliver_events_by_msg: &HashMap<i64, HashSet<String>>,
    remote_msg_ts: &HashMap<String, String>,
    max_text_length: usize,
    format_age_fn: &dyn Fn(f64) -> String,
    now_secs: f64,
    parse_timestamp_fn: &dyn Fn(&str) -> Option<f64>,
) -> Vec<ReadReceipt> {
    let mut receipts = Vec::new();

    for (msg_id, msg_timestamp, msg_data) in sent_messages {
        // Validate scope field present
        if msg_data.get("scope").is_none() {
            continue;
        }

        // Use delivered_to for read receipt denominator
        let delivered_to = match msg_data.get("delivered_to").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>(),
            None => continue,
        };

        let msg_text = msg_data.get("text").and_then(|v| v.as_str()).unwrap_or("");

        let delivered_instances = deliver_events_by_msg
            .get(msg_id)
            .cloned()
            .unwrap_or_default();

        let mut read_by = Vec::new();
        for inst_name in &delivered_to {
            let inst_data = active_instances.get(inst_name);

            // Remote instance: compare msg_ts (timestamp-based)
            if let Some(data) = inst_data {
                if data
                    .get("origin_device_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
                {
                    if let Some(ts) = remote_msg_ts.get(inst_name) {
                        if ts >= msg_timestamp {
                            read_by.push(inst_name.clone());
                        }
                    }
                    continue;
                }
            }

            // Local instance: check for deliver event after message
            if delivered_instances.contains(inst_name) {
                // External senders (no session_id, no parent, not remote) only count
                // as "read" if they were explicitly @mentioned in the message text.
                // This prevents false-positive read receipts for external watchers.
                if let Some(data) = inst_data {
                    if is_external_sender_data(data) {
                        let inst_tag = data.get("tag").and_then(|v| v.as_str());
                        if !is_mentioned(msg_text, inst_name, inst_tag) {
                            continue;
                        }
                    }
                }
                read_by.push(inst_name.clone());
            }
        }

        let total_recipients = delivered_to.len();
        if total_recipients > 0 {
            let age_str = parse_timestamp_fn(msg_timestamp)
                .map(|msg_time| format_age_fn(now_secs - msg_time))
                .unwrap_or_else(|| "?".to_string());

            let truncated_text = if msg_text.len() > max_text_length {
                format!(
                    "{}...",
                    crate::delivery::truncate_chars(msg_text, max_text_length.saturating_sub(3))
                )
            } else {
                msg_text.to_string()
            };

            receipts.push(ReadReceipt {
                id: *msg_id,
                age: age_str,
                text: truncated_text,
                read_by,
                total_recipients,
            });
        }
    }

    receipts
}

/// Max length for message preview in PTY trigger.
pub const PREVIEW_MAX_LEN: usize = 60;

/// Build truncated message preview for PTY injection.
///
/// Reuses format_hook_messages but truncates before user message content.
/// User content may contain @ chars that trigger autocomplete in some CLIs.
pub fn build_message_preview(formatted: &str, max_len: usize) -> String {
    let wrapper_open = "<hcom>";
    let wrapper_close = "</hcom>";
    let wrapper_len = wrapper_open.len() + wrapper_close.len();

    if formatted.is_empty() {
        return format!("{}{}", wrapper_open, wrapper_close);
    }

    let content_max = max_len.saturating_sub(wrapper_len);
    if content_max == 0 {
        return format!("{}{}", wrapper_open, wrapper_close);
    }

    // Truncate before user content (after first ": ") to avoid special chars
    if let Some(colon_pos) = formatted.find(": ") {
        let envelope = &formatted[..colon_pos];
        if envelope.len() > content_max {
            if content_max <= 3 {
                return format!("{}{}", wrapper_open, wrapper_close);
            }
            return format!(
                "{}{}...{}",
                wrapper_open,
                crate::delivery::truncate_chars(envelope, content_max - 3),
                wrapper_close
            );
        }
        return format!("{}{}{}", wrapper_open, envelope, wrapper_close);
    }

    // No colon found, just truncate normally
    if formatted.len() > content_max {
        if content_max <= 3 {
            return format!("{}{}", wrapper_open, wrapper_close);
        }
        return format!(
            "{}{}...{}",
            wrapper_open,
            crate::delivery::truncate_chars(formatted, content_max - 3),
            wrapper_close
        );
    }
    format!("{}{}{}", wrapper_open, formatted, wrapper_close)
}

/// Check if instance is @-mentioned in text using prefix matching on full name.
///
/// Uses same prefix matching logic as compute_scope() for consistency.
pub fn is_mentioned(text: &str, name: &str, tag: Option<&str>) -> bool {
    let full_name = match tag {
        Some(t) if !t.is_empty() => format!("{}-{}", t, name),
        _ => name.to_string(),
    };

    let mentions = extract_mentions(text);

    for mention in &mentions {
        if mention.contains(':') {
            // Remote mention — match any instance with prefix
            if full_name
                .to_lowercase()
                .starts_with(&mention.to_lowercase())
            {
                return true;
            }
        } else {
            // Bare mention — only match local instances (no : in full name)
            // Don't match across underscore boundary
            if !full_name.contains(':')
                && full_name
                    .to_lowercase()
                    .starts_with(&mention.to_lowercase())
                && (full_name.len() == mention.len()
                    || full_name.as_bytes().get(mention.len()) != Some(&b'_'))
            {
                return true;
            }
            // Also check base name match (e.g., @luna matches api-luna)
            if !name.contains(':')
                && name.to_lowercase().starts_with(&mention.to_lowercase())
                && (name.len() == mention.len()
                    || name.as_bytes().get(mention.len()) != Some(&b'_'))
            {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- validate_message ----

    #[test]
    fn test_validate_message_empty() {
        assert_eq!(validate_message(""), Err("Message required".to_string()));
        assert_eq!(validate_message("   "), Err("Message required".to_string()));
    }

    #[test]
    fn test_validate_message_valid() {
        assert!(validate_message("hello world").is_ok());
        assert!(validate_message("line1\nline2\ttab").is_ok());
    }

    #[test]
    fn test_validate_message_control_chars() {
        assert!(validate_message("hello\x00world").is_err());
        assert!(validate_message("hello\x07world").is_err());
    }

    #[test]
    fn test_validate_message_too_large() {
        let big = "x".repeat(MAX_MESSAGE_SIZE + 1);
        assert!(validate_message(&big).is_err());
    }

    // ---- format_recipients ----

    #[test]
    fn test_format_recipients_empty() {
        assert_eq!(format_recipients(&[], 30), "(none)");
    }

    #[test]
    fn test_format_recipients_normal() {
        let names = vec!["luna".to_string(), "nova".to_string()];
        assert_eq!(format_recipients(&names, 30), "luna, nova");
    }

    #[test]
    fn test_format_recipients_truncated() {
        let names: Vec<String> = (0..5).map(|i| format!("agent{}", i)).collect();
        let result = format_recipients(&names, 3);
        assert!(result.contains("+2 more"));
    }

    // ---- validate_scope / validate_intent ----

    #[test]
    fn test_validate_scope() {
        assert!(validate_scope("broadcast").is_ok());
        assert!(validate_scope("mentions").is_ok());
        assert!(validate_scope("invalid").is_err());
    }

    #[test]
    fn test_validate_intent() {
        assert!(validate_intent("request").is_ok());
        assert!(validate_intent("inform").is_ok());
        assert!(validate_intent("ack").is_ok());
        assert!(validate_intent("invalid").is_err());
    }

    // ---- match_target ----

    fn make_instances(names: &[(&str, Option<&str>)]) -> (Vec<String>, HashMap<String, String>) {
        let mut full_to_base = HashMap::new();
        let mut full_names = Vec::new();
        for (name, tag) in names {
            let full = match tag {
                Some(t) => format!("{}-{}", t, name),
                None => name.to_string(),
            };
            full_to_base.insert(full.clone(), name.to_string());
            full_names.push(full);
        }
        (full_names, full_to_base)
    }

    #[test]
    fn test_match_target_exact() {
        let (fns, ftb) = make_instances(&[("luna", None), ("nova", None)]);
        assert_eq!(match_target("luna", &fns, &ftb), vec!["luna"]);
    }

    #[test]
    fn test_match_target_tagged() {
        let (fns, ftb) = make_instances(&[("luna", Some("api")), ("nova", None)]);
        assert_eq!(match_target("api-luna", &fns, &ftb), vec!["luna"]);
    }

    #[test]
    fn test_match_target_tag_prefix() {
        let (fns, ftb) =
            make_instances(&[("luna", Some("api")), ("nova", Some("api")), ("kira", None)]);
        let result = match_target("api-", &fns, &ftb);
        assert!(result.contains(&"luna".to_string()));
        assert!(result.contains(&"nova".to_string()));
        assert!(!result.contains(&"kira".to_string()));
    }

    #[test]
    fn test_match_target_base_name_fallback() {
        let (fns, ftb) = make_instances(&[("luna", Some("api"))]);
        // "luna" doesn't match full name "api-luna" as prefix, but matches base name
        assert_eq!(match_target("luna", &fns, &ftb), vec!["luna"]);
    }

    #[test]
    fn test_match_target_underscore_exclusion() {
        let (fns, ftb) = make_instances(&[("luna", None), ("luna_sub", None)]);
        // @luna should NOT match luna_sub (underscore blocks)
        let result = match_target("luna", &fns, &ftb);
        assert_eq!(result, vec!["luna"]);
    }

    #[test]
    fn test_match_target_bigboss_remote() {
        let (fns, ftb) = make_instances(&[("luna", None)]);
        // Add bigboss
        let mut ftb = ftb;
        let mut fns = fns;
        ftb.insert("bigboss".to_string(), "bigboss".to_string());
        fns.push("bigboss".to_string());

        assert_eq!(match_target("bigboss:BOXE", &fns, &ftb), vec!["bigboss"]);
    }

    #[test]
    fn test_match_target_remote_prefix() {
        let (mut fns, mut ftb) = make_instances(&[("luna", None)]);
        ftb.insert("luna:BOXE".to_string(), "luna".to_string());
        fns.push("luna:BOXE".to_string());

        assert_eq!(match_target("luna:BOXE", &fns, &ftb), vec!["luna"]);
    }

    #[test]
    fn test_match_target_no_match() {
        let (fns, ftb) = make_instances(&[("luna", None)]);
        assert!(match_target("nonexistent", &fns, &ftb).is_empty());
    }

    // ---- compute_scope ----

    fn info(name: &str, tag: Option<&str>) -> InstanceInfo {
        InstanceInfo {
            name: name.to_string(),
            tag: tag.map(|t| t.to_string()),
            project: None,
        }
    }

    #[test]
    fn test_compute_scope_broadcast() {
        let instances = vec![info("luna", None), info("nova", None)];
        let result = compute_scope("hello everyone", &instances, None, None).unwrap();
        assert_eq!(result.scope, MessageScope::Broadcast);
        assert!(result.mentions.is_empty());
    }

    #[test]
    fn test_compute_scope_mention_in_text() {
        let instances = vec![info("luna", None), info("nova", None)];
        let result = compute_scope("hey @luna fix this", &instances, None, None).unwrap();
        assert_eq!(result.scope, MessageScope::Mentions);
        assert_eq!(result.mentions, vec!["luna"]);
    }

    #[test]
    fn test_compute_scope_explicit_targets() {
        let instances = vec![info("luna", None), info("nova", None)];
        let targets = vec!["luna".to_string()];
        let result = compute_scope("fix this", &instances, Some(&targets), None).unwrap();
        assert_eq!(result.scope, MessageScope::Mentions);
        assert_eq!(result.mentions, vec!["luna"]);
    }

    #[test]
    fn test_compute_scope_explicit_empty_broadcast() {
        let instances = vec![info("luna", None)];
        let targets: Vec<String> = vec![];
        let result = compute_scope("hello", &instances, Some(&targets), None).unwrap();
        assert_eq!(result.scope, MessageScope::Broadcast);
    }

    #[test]
    fn test_compute_scope_unknown_target_fails() {
        let instances = vec![info("luna", None)];
        let targets = vec!["nonexistent".to_string()];
        let result = compute_scope("hello", &instances, Some(&targets), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-existent or stopped"));
    }

    #[test]
    fn test_compute_scope_suggests_remote_match() {
        // Bare `@zeli` shouldn't silently match remote `zeli:ZOME`, but the error
        // should point users at the right form instead of just listing 30 names.
        let instances = vec![info("zeli:ZOME", None), info("luna", None)];
        let targets = vec!["zeli".to_string()];
        let err = compute_scope("hello", &instances, Some(&targets), None).unwrap_err();
        assert!(err.contains("@zeli"), "got: {err}");
        assert!(err.contains("Did you mean: @zeli:ZOME"), "got: {err}");
    }

    #[test]
    fn test_compute_scope_no_suggestion_when_no_remote_match() {
        let instances = vec![info("luna", None), info("nova", None)];
        let targets = vec!["zeli".to_string()];
        let err = compute_scope("hello", &instances, Some(&targets), None).unwrap_err();
        assert!(!err.contains("Did you mean"), "got: {err}");
    }

    #[test]
    fn test_compute_scope_unknown_mention_fails() {
        let instances = vec![info("luna", None)];
        let result = compute_scope("hey @nonexistent fix this", &instances, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_scope_system_mention_fails() {
        let instances = vec![info("luna", None)];
        let result = compute_scope("hey @[hcom-events]", &instances, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("System notifications"));
    }

    #[test]
    fn test_compute_scope_literal_mention_fails() {
        let instances = vec![info("luna", None)];
        let result = compute_scope("use @mention to target", &instances, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("literal text @mention is not a valid target")
        );
    }

    #[test]
    fn test_compute_scope_tagged_instances() {
        let instances = vec![info("luna", Some("api")), info("nova", Some("api"))];
        let targets = vec!["api-".to_string()];
        let result = compute_scope("hello", &instances, Some(&targets), None).unwrap();
        assert_eq!(result.scope, MessageScope::Mentions);
        assert!(result.mentions.contains(&"luna".to_string()));
        assert!(result.mentions.contains(&"nova".to_string()));
    }

    #[test]
    fn test_compute_scope_deduplicates() {
        let instances = vec![info("luna", Some("api"))];
        // Both api-luna and luna resolve to the same instance
        let targets = vec!["api-luna".to_string(), "luna".to_string()];
        let result = compute_scope("hello", &instances, Some(&targets), None).unwrap();
        assert_eq!(result.mentions.len(), 1);
        assert_eq!(result.mentions[0], "luna");
    }

    // ---- should_deliver_message ----

    #[test]
    fn test_should_deliver_broadcast() {
        let data = serde_json::json!({"scope": "broadcast", "from": "sender"});
        assert!(should_deliver_message(&data, "receiver", "sender").unwrap());
    }

    #[test]
    fn test_should_deliver_skip_self() {
        let data = serde_json::json!({"scope": "broadcast", "from": "luna"});
        assert!(!should_deliver_message(&data, "luna", "luna").unwrap());
    }

    #[test]
    fn test_should_deliver_mentions_match() {
        let data = serde_json::json!({"scope": "mentions", "mentions": ["luna"]});
        assert!(should_deliver_message(&data, "luna", "nova").unwrap());
    }

    #[test]
    fn test_should_deliver_mentions_no_match() {
        let data = serde_json::json!({"scope": "mentions", "mentions": ["luna"]});
        assert!(!should_deliver_message(&data, "nova", "kira").unwrap());
    }

    #[test]
    fn test_should_deliver_cross_device() {
        let data = serde_json::json!({"scope": "mentions", "mentions": ["luna:BOXE"]});
        // luna matches luna:BOXE after stripping device suffix
        assert!(should_deliver_message(&data, "luna", "nova").unwrap());
    }

    #[test]
    fn test_should_deliver_missing_scope() {
        let data = serde_json::json!({"from": "sender"});
        assert!(should_deliver_message(&data, "receiver", "sender").is_err());
    }

    // ---- build_message_prefix ----

    #[test]
    fn test_build_prefix_intent_thread() {
        let msg = serde_json::json!({"intent": "request", "thread": "pr-42", "event_id": 42});
        assert_eq!(build_message_prefix(&msg), "[request:pr-42 #42]");
    }

    #[test]
    fn test_build_prefix_intent_only() {
        let msg = serde_json::json!({"intent": "ack", "event_id": 10});
        assert_eq!(build_message_prefix(&msg), "[ack #10]");
    }

    #[test]
    fn test_build_prefix_thread_only() {
        let msg = serde_json::json!({"thread": "testing", "event_id": 5});
        assert_eq!(build_message_prefix(&msg), "[thread:testing #5]");
    }

    #[test]
    fn test_build_prefix_no_envelope() {
        let msg = serde_json::json!({"event_id": 1});
        assert_eq!(build_message_prefix(&msg), "[new message #1]");
    }

    #[test]
    fn test_build_prefix_remote() {
        let msg = serde_json::json!({"intent": "inform", "_relay": {"short": "BOXE", "id": 42}});
        assert_eq!(build_message_prefix(&msg), "[inform #42:BOXE]");
    }

    // ---- unescape_bash ----

    #[test]
    fn test_unescape_bash() {
        assert_eq!(unescape_bash("hello\\!world"), "hello!world");
        assert_eq!(unescape_bash("\\$HOME"), "$HOME");
        assert_eq!(unescape_bash("\\`cmd\\`"), "`cmd`");
        assert_eq!(unescape_bash("say \\\"hello\\\""), "say \"hello\"");
        assert_eq!(unescape_bash("it\\'s"), "it's");
    }

    #[test]
    fn test_unescape_bash_preserves_backslash() {
        // Double backslashes are NOT unescaped
        assert_eq!(unescape_bash("path\\\\to\\\\file"), "path\\\\to\\\\file");
    }

    // ---- build_message_preview ----

    #[test]
    fn test_build_message_preview_empty() {
        assert_eq!(build_message_preview("", 60), "<hcom></hcom>");
    }

    #[test]
    fn test_build_message_preview_truncates_at_colon() {
        let formatted = "[request #42] luna → nova: here is a long message";
        let result = build_message_preview(formatted, 60);
        // Should include up to the colon but not the message content
        assert!(result.starts_with("<hcom>"));
        assert!(result.ends_with("</hcom>"));
        assert!(result.contains("[request #42] luna → nova"));
        assert!(!result.contains("here is a long message"));
    }

    #[test]
    fn test_build_message_preview_no_colon() {
        let formatted = "short text";
        let result = build_message_preview(formatted, 60);
        assert_eq!(result, "<hcom>short text</hcom>");
    }

    // ---- is_mentioned ----

    #[test]
    fn test_is_mentioned_basic() {
        assert!(is_mentioned("hey @luna fix this", "luna", None));
        assert!(!is_mentioned("hey @nova fix this", "luna", None));
    }

    #[test]
    fn test_is_mentioned_tagged() {
        assert!(is_mentioned("hey @api-luna", "luna", Some("api")));
        assert!(is_mentioned("hey @api-", "luna", Some("api")));
        assert!(is_mentioned("hey @luna", "luna", Some("api")));
    }

    #[test]
    fn test_is_mentioned_underscore_block() {
        assert!(is_mentioned("hey @luna", "luna", None));
        assert!(!is_mentioned("hey @luna", "luna_sub", None));
    }

    // ---- MessageScope / MessageIntent ----

    #[test]
    fn test_message_scope_roundtrip() {
        assert_eq!(
            MessageScope::Broadcast
                .as_str()
                .parse::<MessageScope>()
                .ok(),
            Some(MessageScope::Broadcast)
        );
        assert_eq!(
            MessageScope::Mentions.as_str().parse::<MessageScope>().ok(),
            Some(MessageScope::Mentions)
        );
        assert!("invalid".parse::<MessageScope>().is_err());
    }

    #[test]
    fn test_message_intent_roundtrip() {
        assert_eq!(
            MessageIntent::Request
                .as_str()
                .parse::<MessageIntent>()
                .ok(),
            Some(MessageIntent::Request)
        );
        assert_eq!(
            MessageIntent::Inform.as_str().parse::<MessageIntent>().ok(),
            Some(MessageIntent::Inform)
        );
        assert_eq!(
            MessageIntent::Ack.as_str().parse::<MessageIntent>().ok(),
            Some(MessageIntent::Ack)
        );
        assert!("invalid".parse::<MessageIntent>().is_err());
    }

    // ---- format_hook_messages / format_messages_json ----

    #[test]
    fn test_format_hook_messages_single() {
        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hello there",
            "event_id": 42,
            "delivered_to": ["nova"],
        })];

        let result = format_hook_messages(&msgs, "nova", &|_name| None, &|| String::new(), None);
        assert!(result.contains("luna"));
        assert!(result.contains("nova"));
        assert!(result.contains("hello there"));
        assert!(result.contains("#42"));
    }

    #[test]
    fn test_format_hook_messages_multiple() {
        let msgs = vec![
            serde_json::json!({
                "from": "luna",
                "message": "first",
                "event_id": 1,
                "delivered_to": ["nova"],
            }),
            serde_json::json!({
                "from": "kira",
                "message": "second",
                "event_id": 2,
                "delivered_to": ["nova"],
            }),
        ];

        let result = format_hook_messages(&msgs, "nova", &|_name| None, &|| String::new(), None);
        assert!(result.contains("[2 new messages]"));
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    #[test]
    fn test_format_hook_messages_with_hints() {
        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "event_id": 1,
            "delivered_to": ["nova"],
        })];

        let result = format_hook_messages(
            &msgs,
            "nova",
            &|_name| None,
            &|| "respond with hcom send".to_string(),
            None,
        );
        assert!(result.contains("[respond with hcom send]"));
    }

    #[test]
    fn test_format_messages_json_wraps_in_tags() {
        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "event_id": 1,
            "delivered_to": ["nova"],
        })];

        let result = format_messages_json(&msgs, "nova", &|_name| None, &|| String::new(), None);
        assert!(result.starts_with("<hcom>"));
        assert!(result.ends_with("</hcom>"));
    }

    #[test]
    fn test_format_hook_messages_appends_recv_tip_once() {
        use std::cell::Cell;
        use std::rc::Rc;

        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "event_id": 1,
            "intent": "request",
            "delivered_to": ["nova"],
        })];
        let marks = Rc::new(Cell::new(0));
        let tip_checker = |_: &str, _: &str| -> (bool, Box<dyn Fn()>) {
            let marks = Rc::clone(&marks);
            let mark = Box::new(move || marks.set(marks.get() + 1)) as Box<dyn Fn()>;
            (false, mark)
        };

        let result = format_hook_messages(
            &msgs,
            "nova",
            &|_name| None,
            &|| String::new(),
            Some(&tip_checker),
        );
        assert!(result.contains("[tip] intent=request: Sender expects a response."));
        assert_eq!(marks.get(), 1);
    }

    #[test]
    fn test_format_messages_json_marks_tip_without_duplicate_text() {
        use std::cell::Cell;
        use std::rc::Rc;

        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "event_id": 1,
            "intent": "request",
            "delivered_to": ["nova"],
        })];
        let seen = Rc::new(Cell::new(false));
        let tip_checker = |_: &str, _: &str| -> (bool, Box<dyn Fn()>) {
            let seen = Rc::clone(&seen);
            let already_seen = seen.get();
            let mark = Box::new(move || seen.set(true)) as Box<dyn Fn()>;
            (already_seen, mark)
        };

        let first = format_messages_json(
            &msgs,
            "nova",
            &|_name| None,
            &|| String::new(),
            Some(&tip_checker),
        );
        let second = format_messages_json(
            &msgs,
            "nova",
            &|_name| None,
            &|| String::new(),
            Some(&tip_checker),
        );
        assert!(first.contains("[tip] intent=request: Sender expects a response."));
        assert!(!second.contains("[tip] intent=request: Sender expects a response."));
    }

    #[test]
    fn test_format_hook_messages_with_others() {
        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "event_id": 1,
            "delivered_to": ["nova", "kira", "miso"],
        })];

        let result = format_hook_messages(&msgs, "nova", &|_name| None, &|| String::new(), None);
        // Should show "+2 others" for single message
        assert!(result.contains("+2 others"));
    }

    #[test]
    fn test_format_hook_messages_appends_thread_tip_once() {
        use std::cell::Cell;
        use std::rc::Rc;

        let msgs = vec![serde_json::json!({
            "from": "luna",
            "message": "hi",
            "thread": "debate-1",
            "event_id": 1,
            "delivered_to": ["nova"],
        })];
        let marks = Rc::new(Cell::new(0));
        let tip_checker = |_: &str, tip_key: &str| -> (bool, Box<dyn Fn()>) {
            assert_eq!(tip_key, "recv:thread:debate-1");
            let marks = Rc::clone(&marks);
            let mark = Box::new(move || marks.set(marks.get() + 1)) as Box<dyn Fn()>;
            (false, mark)
        };

        let result = format_hook_messages(
            &msgs,
            "nova",
            &|_name| None,
            &|| String::new(),
            Some(&tip_checker),
        );
        assert!(result.contains("[tip] You joined thread debate-1."));
        assert!(result.contains("hcom events unsub sub-"));
        assert_eq!(marks.get(), 1);
    }

    // ---- compute_read_receipts ----

    #[test]
    fn test_compute_read_receipts_basic() {
        let sent = vec![(
            42_i64,
            "2024-01-01T00:00:00Z".to_string(),
            serde_json::json!({
                "scope": "broadcast",
                "text": "hello world",
                "delivered_to": ["nova", "kira"],
            }),
        )];

        let active: HashMap<String, Value> = HashMap::from([
            (
                "nova".to_string(),
                serde_json::json!({"tag": null, "session_id": "sess-1"}),
            ),
            (
                "kira".to_string(),
                serde_json::json!({"tag": null, "session_id": "sess-2"}),
            ),
        ]);

        let mut deliver_events = HashMap::new();
        let mut delivered = HashSet::new();
        delivered.insert("nova".to_string());
        deliver_events.insert(42_i64, delivered);

        let receipts = compute_read_receipts(
            &sent,
            &active,
            &deliver_events,
            &HashMap::new(),
            50,
            &|secs| format!("{}s", secs as i64),
            100.0,
            &|_ts| Some(0.0),
        );

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].id, 42);
        assert_eq!(receipts[0].read_by, vec!["nova"]);
        assert_eq!(receipts[0].total_recipients, 2);
    }

    #[test]
    fn test_compute_read_receipts_remote() {
        let sent = vec![(
            42_i64,
            "2024-01-01T00:00:00Z".to_string(),
            serde_json::json!({
                "scope": "broadcast",
                "text": "hello",
                "delivered_to": ["luna:BOXE"],
            }),
        )];

        let active: HashMap<String, Value> = HashMap::from([(
            "luna:BOXE".to_string(),
            serde_json::json!({"origin_device_id": "device-1"}),
        )]);

        let remote_ts: HashMap<String, String> = HashMap::from([(
            "luna:BOXE".to_string(),
            "2024-01-02T00:00:00Z".to_string(), // After message
        )]);

        let receipts = compute_read_receipts(
            &sent,
            &active,
            &HashMap::new(),
            &remote_ts,
            50,
            &|_| "1h".to_string(),
            100.0,
            &|_| Some(0.0),
        );

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].read_by, vec!["luna:BOXE"]);
    }

    #[test]
    fn test_compute_read_receipts_external_sender_gating() {
        // External sender (no session_id) should only count as read if @mentioned
        let sent = vec![(
            42_i64,
            "2024-01-01T00:00:00Z".to_string(),
            serde_json::json!({
                "scope": "broadcast",
                "text": "hello everyone",  // No @mention of watcher
                "delivered_to": ["nova", "watcher"],
            }),
        )];

        let active: HashMap<String, Value> = HashMap::from([
            (
                "nova".to_string(),
                serde_json::json!({"tag": null, "session_id": "sess-1"}),
            ),
            // External sender: no session_id → should be gated
            ("watcher".to_string(), serde_json::json!({"tag": null})),
        ]);

        let mut deliver_events = HashMap::new();
        let mut delivered = HashSet::new();
        delivered.insert("nova".to_string());
        delivered.insert("watcher".to_string());
        deliver_events.insert(42_i64, delivered);

        let receipts = compute_read_receipts(
            &sent,
            &active,
            &deliver_events,
            &HashMap::new(),
            50,
            &|secs| format!("{}s", secs as i64),
            100.0,
            &|_ts| Some(0.0),
        );

        assert_eq!(receipts.len(), 1);
        // nova has session_id → counted as read
        // watcher has no session_id (external) and not @mentioned → NOT counted
        assert_eq!(receipts[0].read_by, vec!["nova"]);
        assert_eq!(receipts[0].total_recipients, 2);
    }

    #[test]
    fn test_compute_read_receipts_external_sender_mentioned() {
        // External sender IS @mentioned → should count as read
        let sent = vec![(
            42_i64,
            "2024-01-01T00:00:00Z".to_string(),
            serde_json::json!({
                "scope": "mentions",
                "text": "hey @watcher check this",
                "mentions": ["watcher"],
                "delivered_to": ["watcher"],
            }),
        )];

        let active: HashMap<String, Value> = HashMap::from([
            ("watcher".to_string(), serde_json::json!({"tag": null})), // External
        ]);

        let mut deliver_events = HashMap::new();
        let mut delivered = HashSet::new();
        delivered.insert("watcher".to_string());
        deliver_events.insert(42_i64, delivered);

        let receipts = compute_read_receipts(
            &sent,
            &active,
            &deliver_events,
            &HashMap::new(),
            50,
            &|secs| format!("{}s", secs as i64),
            100.0,
            &|_ts| Some(0.0),
        );

        assert_eq!(receipts.len(), 1);
        // watcher is external but was @mentioned → counted as read
        assert_eq!(receipts[0].read_by, vec!["watcher"]);
    }

    #[test]
    fn test_is_external_sender_data() {
        // Normal instance with session_id → not external
        assert!(!is_external_sender_data(
            &serde_json::json!({"session_id": "sess-1"})
        ));

        // External: no session_id
        assert!(is_external_sender_data(&serde_json::json!({"tag": null})));
        assert!(is_external_sender_data(
            &serde_json::json!({"session_id": ""})
        ));

        // Remote: has origin_device_id → not external
        assert!(!is_external_sender_data(
            &serde_json::json!({"origin_device_id": "dev-1"})
        ));

        // Subagent: has parent_session_id → not external
        assert!(!is_external_sender_data(
            &serde_json::json!({"parent_session_id": "parent-sess"})
        ));
    }
}
