//! `hcom relay` command — cross-device sync via MQTT pub/sub.

use crate::config;
use crate::db::HcomDb;
use crate::relay::{self, DEFAULT_BROKERS};
use crate::shared::CommandContext;
use crate::shared::ansi::{FG_GRAY, FG_GREEN, FG_RED, FG_YELLOW, RESET};

/// Parsed arguments for `hcom relay`.
#[derive(clap::Parser, Debug)]
#[command(name = "relay", about = "Cross-device sync via MQTT")]
pub struct RelayArgs {
    /// Subcommand and arguments (new/connect/off/status + flags)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parse --broker and --password flags from argv.
fn parse_broker_flags(argv: &[String]) -> (Option<String>, Option<String>, Vec<String>) {
    let mut broker = None;
    let mut auth_token = None;
    let mut remaining = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--broker" && i + 1 < argv.len() {
            broker = Some(argv[i + 1].clone());
            i += 2;
        } else if argv[i] == "--password" && i + 1 < argv.len() {
            auth_token = Some(argv[i + 1].clone());
            i += 2;
        } else {
            remaining.push(argv[i].clone());
            i += 1;
        }
    }
    (broker, auth_token, remaining)
}

/// Ping a broker via TCP connect (+ TLS handshake when use_tls=true).
/// Returns round-trip ms or None on failure.
fn ping_broker(host: &str, port: u16, use_tls: bool) -> Option<u32> {
    crate::relay::broker::ping_broker(host, port, use_tls).map(|ms| ms as u32)
}

/// Test all default brokers in parallel. Returns (host, port, ping_ms|None) for each.
fn test_brokers_parallel() -> Vec<(String, u16, Option<u32>)> {
    relay::broker::test_brokers_parallel(DEFAULT_BROKERS)
        .into_iter()
        .map(|(h, p, ms)| (h, p, ms.map(|m| m as u32)))
        .collect()
}

/// Encode relay_id + broker into a join token.
fn encode_join_token(relay_id: &str, broker_url: &str) -> Option<String> {
    relay::token::encode_join_token(relay_id, broker_url)
}

/// Decode a join token back to (relay_id, broker_url).
fn decode_join_token(token: &str) -> Option<(String, String)> {
    relay::token::decode_join_token(token)
}

/// Format a timestamp as relative age.
fn format_time(timestamp: f64) -> String {
    if timestamp == 0.0 {
        return "never".to_string();
    }
    let now = crate::shared::time::now_epoch_f64();
    let age = (now - timestamp) as i64;
    if age <= 0 {
        return "just now".to_string();
    }
    format!("{} ago", crate::instances::format_age(age))
}

/// Get device short ID via FNV-1a hash
/// Auto-creates device_id file if missing (via read_device_uuid).
fn get_device_short_id() -> String {
    crate::relay::device_short_id(&crate::relay::read_device_uuid())
}

/// Show relay status.
fn relay_status(db: &HcomDb) -> i32 {
    let config = config::load_config_snapshot().core;

    if config.relay_id.is_empty() {
        println!("{FG_GRAY}Relay: not configured{RESET}");
        println!("Run: hcom relay new");
        return 0;
    }

    if !config.relay_enabled {
        println!("{FG_YELLOW}Relay: disabled{RESET}");
        println!("\nRun: hcom relay connect");
        return 0;
    }

    // Show MQTT connection state from kv store
    let relay_status_val = db.kv_get("relay_status").ok().flatten().unwrap_or_default();
    let relay_error = db
        .kv_get("relay_last_error")
        .ok()
        .flatten()
        .unwrap_or_default();

    match relay_status_val.as_str() {
        "ok" => println!("Status:    {FG_GREEN}connected{RESET}"),
        "error" => {
            println!(
                "Status:    {FG_RED}error{RESET} — {}",
                if relay_error.is_empty() {
                    "unknown"
                } else {
                    &relay_error
                }
            );
            if relay_error.contains("password")
                || relay_error.contains("auth")
                || relay_error.contains("not authorized")
            {
                let is_public = DEFAULT_BROKERS.iter().any(|&(h, p)| {
                    config.relay == format!("mqtts://{h}:{p}")
                        || config.relay == format!("mqtt://{h}:{p}")
                });
                if !is_public && config.relay_token.is_empty() {
                    println!("  Hint: use --password when connecting to private brokers");
                }
            }
        }
        _ => println!("Status:    {FG_YELLOW}waiting{RESET} (daemon may not be running)"),
    }

    // Broker info
    if !config.relay.is_empty() {
        if let Some((host, port, use_tls)) = relay::parse_broker_url(&config.relay) {
            if let Some(ms) = ping_broker(&host, port, use_tls) {
                println!("Broker:    {} ({ms}ms)", config.relay);
            } else {
                println!("Broker:    {} (unreachable)", config.relay);
            }
        } else {
            println!("Broker:    {}", config.relay);
        }
    } else {
        println!("Broker:    auto (public fallback)");
    }

    println!("Device:    {}", get_device_short_id());

    // Queued events
    let last_push_id: i64 = db
        .kv_get("relay_last_push_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let queued: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE id > ?1 AND instance NOT LIKE '%:%'",
            rusqlite::params![last_push_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if queued > 0 {
        println!("Queued:    {queued} events pending");
    } else {
        println!("Queued:    up to date");
    }

    // Last push
    let last_push: f64 = db
        .kv_get("relay_last_push")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    if last_push > 0.0 {
        println!("Last push: {}", format_time(last_push));
    } else {
        println!("Last push: never");
    }

    // Remote devices from KV
    // Uses relay_short_{short_id} → device_id + relay_sync_time_{device_id} freshness.
    let own_device = crate::relay::read_device_uuid();
    let now = crate::shared::time::now_epoch_f64();
    let max_age = 90.0;

    // relay_short_{short_id} → device_id (invert to device_id → short_id)
    let mut device_to_short = std::collections::HashMap::new();
    if let Ok(entries) = db.kv_prefix("relay_short_") {
        for (key, device_id) in entries {
            if device_id == own_device {
                continue;
            }
            let short = key.strip_prefix("relay_short_").unwrap_or(&key).to_string();
            device_to_short.insert(device_id, short);
        }
    }

    // Filter by sync_time freshness
    let sync_map: std::collections::HashMap<String, String> = db
        .kv_prefix("relay_sync_time_")
        .unwrap_or_default()
        .into_iter()
        .collect();

    // Agent counts per remote device
    let mut agent_counts = std::collections::HashMap::new();
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT origin_device_id, COUNT(*) as cnt FROM instances \
         WHERE origin_device_id IS NOT NULL AND origin_device_id != '' \
         GROUP BY origin_device_id",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.filter_map(|r| r.ok()) {
                agent_counts.insert(row.0, row.1);
            }
        }
    }

    let mut remote_parts = Vec::new();
    let mut sorted_devices: Vec<_> = device_to_short.iter().collect();
    sorted_devices.sort_by(|a, b| a.1.cmp(b.1));

    for (device_id, short) in sorted_devices {
        let sync_time: f64 = sync_map
            .get(&format!("relay_sync_time_{device_id}"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if sync_time == 0.0 || (now - sync_time) > max_age {
            continue;
        }

        let agents = agent_counts.get(device_id).copied().unwrap_or(0);
        let mut parts = vec![short.clone()];
        parts.push(format_time(sync_time));
        if agents == 0 {
            parts.push("no agents".to_string());
        }
        if parts.len() > 1 {
            remote_parts.push(format!("{} ({})", parts[0], parts[1..].join(", ")));
        } else {
            remote_parts.push(parts[0].clone());
        }
    }

    if !remote_parts.is_empty() {
        println!("\nRemote devices: {}", remote_parts.join(", "));
    } else {
        println!("\nNo other devices");
    }

    // Show join token
    if !config.relay.is_empty() && !config.relay_id.is_empty() {
        if let Some(token) = encode_join_token(&config.relay_id, &config.relay) {
            println!("\nAdd devices: hcom relay connect {token}");
        }
    }

    0
}

/// Enable or disable relay sync.
fn relay_toggle(db: &HcomDb, enable: bool) -> i32 {
    let config = config::load_config_snapshot().core;

    if config.relay_id.is_empty() {
        eprintln!("No relay configured.");
        eprintln!("Run: hcom relay new");
        return 1;
    }

    // Clear retained MQTT state before disabling so remote devices stop seeing us
    if !enable && config.relay_enabled && crate::relay::client::clear_retained_state(&config) {
        println!("Cleared remote state");
    }

    // Update config file
    let config_path = crate::paths::config_toml_path();
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        let new_content = update_toml_key(
            &content,
            "relay_enabled",
            if enable { "true" } else { "false" },
        );
        if let Err(e) = std::fs::write(&config_path, &new_content) {
            eprintln!("Error: Failed to write config: {e}");
            return 1;
        }
    }

    if enable {
        println!("Relay enabled\n");
        crate::relay::worker::ensure_worker(false);
        relay_status(db)
    } else {
        println!("{FG_YELLOW}Relay: disabled{RESET}");
        println!("\nRun 'hcom relay connect' to reconnect");
        0
    }
}

/// Create a new relay group.
fn relay_new(_db: &HcomDb, argv: &[String]) -> i32 {
    let (broker_url, auth_token, _) = parse_broker_flags(argv);

    let config = config::load_config_snapshot().core;

    // Show previous group token if switching
    if !config.relay_id.is_empty() && !config.relay.is_empty() {
        if let Some(old_token) = encode_join_token(&config.relay_id, &config.relay) {
            println!("Current group: hcom relay connect {old_token}\n");
        }
    }

    let relay_id = uuid::Uuid::new_v4().to_string();

    let pinned_broker = if let Some(broker) = &broker_url {
        // Private broker — test connectivity
        if let Some((host, port, use_tls)) = relay::parse_broker_url(broker) {
            println!("Testing {host}:{port}...");
            match ping_broker(&host, port, use_tls) {
                Some(ms) => {
                    println!("  {host}:{port} — {ms}ms");
                    broker.clone()
                }
                None => {
                    eprintln!("  {host}:{port} — failed");
                    eprintln!("\nBroker unreachable. Check host, port, and network.");
                    return 1;
                }
            }
        } else {
            eprintln!("Invalid broker URL: {broker}");
            return 1;
        }
    } else {
        // Public broker — test all in parallel
        println!("Testing brokers...");
        let results = test_brokers_parallel();
        let mut best = None;
        for (host, port, ms) in &results {
            if let Some(ms) = ms {
                println!("  {host}:{port} — {ms}ms");
                if best.is_none() {
                    best = Some(format!("mqtts://{host}:{port}"));
                }
            } else {
                println!("  {host}:{port} — failed");
            }
        }
        match best {
            Some(b) => b,
            None => {
                eprintln!("\nNo broker reachable. Check your network.");
                eprintln!("Or use a private broker: hcom relay new --broker mqtts://host:port");
                return 1;
            }
        }
    };

    // Save config
    let config_path = crate::paths::config_toml_path();
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut content = update_toml_key(&content, "relay_id", &format!("\"{relay_id}\""));
    content = update_toml_key(&content, "relay", &format!("\"{pinned_broker}\""));
    content = update_toml_key(&content, "relay_enabled", "true");
    if let Some(ref token) = auth_token {
        content = update_toml_key(&content, "relay_token", &format!("\"{token}\""));
    }
    if let Err(e) = std::fs::write(&config_path, &content) {
        eprintln!("Error: Failed to write config: {e}");
        return 1;
    }

    // Generate join token
    if let Some(token) = encode_join_token(&relay_id, &pinned_broker) {
        println!("\nBroker: {pinned_broker}");
        if auth_token.is_some() {
            println!("Password: set");
        }
        println!("\nOn other devices: hcom relay connect {token}");
        if auth_token.is_some() {
            println!("  (they will also need: --password <secret>)");
        }
    }

    if crate::relay::worker::ensure_worker(false) {
        println!("\nConnected.");
    } else if crate::relay::worker::is_relay_worker_running() {
        println!("\nDaemon started (not yet ready). Run 'hcom relay status' to confirm.");
    } else {
        println!("\nCould not start daemon automatically. Run 'hcom relay daemon start'.");
    }
    0
}

/// Connect to relay — re-enable or join with token.
fn relay_connect(db: &HcomDb, argv: &[String]) -> i32 {
    let (broker_url, auth_token, remaining) = parse_broker_flags(argv);

    let token_str = remaining.first().filter(|s| !s.starts_with("-")).cloned();

    if token_str.is_none() {
        // Re-enable mode
        let config = config::load_config_snapshot().core;
        if config.relay_id.is_empty() {
            eprintln!("No relay configured.");
            eprintln!("Run: hcom relay new");
            return 1;
        }
        if config.relay_enabled {
            println!("Relay already enabled.\n");
            return relay_status(db);
        }
        return relay_toggle(db, true);
    }

    let token_str = token_str.unwrap();

    // Decode token
    let (relay_id, token_broker) = match decode_join_token(&token_str) {
        Some(r) => r,
        None => {
            eprintln!("Invalid token.");
            return 1;
        }
    };

    let effective_broker = broker_url.unwrap_or(token_broker);

    // Test broker connectivity
    let ping_ms = relay::parse_broker_url(&effective_broker)
        .and_then(|(host, port, use_tls)| ping_broker(&host, port, use_tls));

    let config = config::load_config_snapshot().core;

    // Show previous group if switching
    if !config.relay_id.is_empty() && !config.relay.is_empty() && config.relay_id != relay_id {
        if let Some(old_token) = encode_join_token(&config.relay_id, &config.relay) {
            println!("Current group: hcom relay connect {old_token}\n");
        }
    }

    // Save config
    let config_path = crate::paths::config_toml_path();
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut content = update_toml_key(&content, "relay_id", &format!("\"{relay_id}\""));
    content = update_toml_key(&content, "relay", &format!("\"{effective_broker}\""));
    content = update_toml_key(&content, "relay_enabled", "true");
    if let Some(ref token) = auth_token {
        content = update_toml_key(&content, "relay_token", &format!("\"{token}\""));
    }
    if let Err(e) = std::fs::write(&config_path, &content) {
        eprintln!("Error: Failed to write config: {e}");
        return 1;
    }

    if let Some(ms) = ping_ms {
        println!("Broker: {effective_broker} ({ms}ms)");
    } else {
        println!("Broker: {effective_broker}");
        eprintln!("  Warning: broker unreachable — check network or token");
    }

    if auth_token.is_some() {
        println!("Password: set");
    } else {
        let is_public = DEFAULT_BROKERS.iter().any(|&(h, p)| {
            effective_broker == format!("mqtts://{h}:{p}")
                || effective_broker == format!("mqtt://{h}:{p}")
        });
        if !is_public {
            println!("Password: not set (use --password if broker requires auth)");
        }
    }

    if crate::relay::worker::ensure_worker(false) {
        println!("\nConnected.");
    } else if crate::relay::worker::is_relay_worker_running() {
        println!("\nDaemon started (not yet ready). Run 'hcom relay status' to confirm.");
    } else {
        println!("\nCould not start daemon automatically. Run 'hcom relay daemon start'.");
    }
    0
}

/// Update or add a key in TOML content (simple line-level editing).
/// Map flat relay field names to TOML section paths under [relay].
fn relay_toml_key(field: &str) -> (&str, &str) {
    match field {
        "relay" => ("relay", "url"),
        "relay_id" => ("relay", "id"),
        "relay_token" => ("relay", "token"),
        "relay_enabled" => ("relay", "enabled"),
        _ => panic!("unknown relay field: {field}"),
    }
}

/// Update a relay config field in config.toml using toml_edit for proper section handling.
fn update_toml_key(content: &str, field: &str, value: &str) -> String {
    let (section, key) = relay_toml_key(field);

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    // Ensure section exists
    if !doc.contains_table(section) {
        doc[section] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    // Parse the value appropriately
    if value == "true" || value == "false" {
        doc[section][key] = toml_edit::value(value == "true");
    } else if let Some(stripped) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        doc[section][key] = toml_edit::value(stripped);
    } else {
        doc[section][key] = toml_edit::value(value);
    }

    doc.to_string()
}

pub fn cmd_relay(db: &HcomDb, args: &RelayArgs, _ctx: Option<&CommandContext>) -> i32 {
    // --name already stripped by router's extract_global_flags_full()
    let argv = &args.args;

    if argv.is_empty() {
        return relay_status(db);
    }

    let first = argv[0].as_str();

    if first == "--help" || first == "-h" {
        println!(
            "hcom relay - Cross-device sync via MQTT pub/sub\n\n\
             Usage:\n  \
             hcom relay                  Show relay status\n  \
             hcom relay status           Same as above\n  \
             hcom relay new              Create new relay group\n  \
             hcom relay connect          Re-enable existing relay\n  \
             hcom relay connect <token>  Join relay from another device\n  \
             hcom relay off              Disable relay sync\n  \
             hcom relay disconnect       Disable relay sync\n\n\
             Daemon:\n  \
             hcom relay daemon           Show daemon status\n  \
             hcom relay daemon start     Start the relay daemon\n  \
             hcom relay daemon stop      Stop the relay daemon\n  \
             hcom relay daemon restart   Restart the relay daemon\n\n\
             Private broker:\n  \
             hcom relay new --broker mqtts://host:port [--password secret]\n  \
             hcom relay connect <token> --broker mqtts://host:port [--password secret]"
        );
        return 0;
    }

    match first {
        "new" => relay_new(db, &argv[1..]),
        "connect" => relay_connect(db, &argv[1..]),
        "off" | "disconnect" => relay_toggle(db, false),
        "on" => relay_connect(db, &Vec::new()),
        "status" => relay_status(db),
        "daemon" => crate::commands::daemon::cmd_daemon(&argv[1..]),
        _ => {
            // Could be a token passed directly
            if argv[0].len() > 20 && !argv[0].starts_with('-') {
                relay_connect(db, argv)
            } else {
                eprintln!("Error: Unknown subcommand: {first}");
                eprintln!("Usage: hcom relay [new|connect|disconnect|status]");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_public_broker_token() {
        let relay_id = uuid::Uuid::new_v4().to_string();
        let broker = format!("mqtts://{}:{}", DEFAULT_BROKERS[0].0, DEFAULT_BROKERS[0].1);
        let token = encode_join_token(&relay_id, &broker).unwrap();
        let (decoded_id, decoded_broker) = decode_join_token(&token).unwrap();
        assert_eq!(decoded_id, relay_id);
        assert_eq!(decoded_broker, broker);
    }

    #[test]
    fn test_encode_decode_private_broker_token() {
        let relay_id = uuid::Uuid::new_v4().to_string();
        let broker = "mqtts://my-broker.example.com:8883";
        let token = encode_join_token(&relay_id, broker).unwrap();
        let (decoded_id, decoded_broker) = decode_join_token(&token).unwrap();
        assert_eq!(decoded_id, relay_id);
        assert_eq!(decoded_broker, broker);
    }

    #[test]
    fn test_decode_invalid_token() {
        assert!(decode_join_token("not-a-token").is_none());
        assert!(decode_join_token("").is_none());
    }

    #[test]
    fn test_update_toml_key_existing() {
        let content = "[relay]\nurl = \"\"\nid = \"\"\nenabled = false\n[other]\nfoo = 1\n";
        let result = update_toml_key(content, "relay_enabled", "true");
        assert!(result.contains("enabled = true"));
        assert!(result.contains("foo = 1"));
    }

    #[test]
    fn test_update_toml_key_new() {
        let content = "[other]\nfoo = 1\n";
        let result = update_toml_key(content, "relay_enabled", "true");
        // Should create [relay] section with enabled = true
        let doc: toml_edit::DocumentMut = result.parse().unwrap();
        assert_eq!(doc["relay"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["other"]["foo"].as_integer(), Some(1));
    }

    #[test]
    fn test_parse_broker_flags() {
        let argv: Vec<String> = vec![
            "--broker".into(),
            "mqtts://host:8883".into(),
            "--password".into(),
            "secret".into(),
            "other".into(),
        ];
        let (broker, auth, remaining) = parse_broker_flags(&argv);
        assert_eq!(broker.as_deref(), Some("mqtts://host:8883"));
        assert_eq!(auth.as_deref(), Some("secret"));
        assert_eq!(remaining, vec!["other"]);
    }
}
