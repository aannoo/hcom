//! `hcom relay` command — cross-device sync via MQTT pub/sub.

use crate::config;
use crate::db::HcomDb;
use crate::relay::token::DecodedToken;
use crate::relay::{self, DEFAULT_BROKERS};
use crate::shared::CommandContext;
use crate::shared::ansi::{FG_GRAY, FG_GREEN, FG_RED, FG_YELLOW, RESET};
use crate::shared::time::format_age;

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

/// Encode relay_id + broker into a join token. Always passes the active PSK so
/// new tokens are always v0x04 (PSK-bearing). Legacy v0x01/v0x02 tokens are only
/// produced by tests via the underlying `relay::token::encode_join_token`.
fn encode_join_token(relay_id: &str, broker_url: &str, psk: &[u8; 32]) -> Option<String> {
    relay::token::encode_join_token(relay_id, broker_url, Some(psk))
}

/// Decode a join token. Returns the parsed structure including the optional
/// PSK; legacy tokens have `psk = None` and are rejected by `relay_connect`.
fn decode_join_token(token: &str) -> Option<DecodedToken> {
    relay::token::decode_join_token(token)
}

/// Build the "Add devices" / "current group" join token from current config,
/// or `None` if the relay isn't fully configured.
fn current_join_token(config: &crate::config::HcomConfig) -> Option<String> {
    if config.relay_id.is_empty() || config.relay.is_empty() {
        return None;
    }
    let psk = relay::load_psk(config).ok()?;
    encode_join_token(&config.relay_id, &config.relay, &psk)
}

/// Show the join token (separate command to avoid leaking in status output).
fn relay_show_token() -> i32 {
    let config = config::load_config_snapshot().core;
    match current_join_token(&config) {
        Some(token) => {
            println!("hcom relay connect {token}");
            if !config.relay_token.is_empty() {
                println!("  (also needs: --password <secret>)");
            }
            0
        }
        None => {
            eprintln!("No relay configured. Run: hcom relay new");
            1
        }
    }
}

fn validate_existing_relay_config(config: &crate::config::HcomConfig) -> Result<(), String> {
    if config.relay_id.is_empty() {
        return Err("no relay configured".to_string());
    }
    if config.relay.trim().is_empty() {
        return Err("relay broker URL is missing".to_string());
    }
    if relay::parse_broker_url(&config.relay).is_none() {
        return Err(format!("relay broker URL is invalid: {}", config.relay));
    }
    relay::load_psk(config).map(|_| ())
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
    format!("{} ago", format_age(age))
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

    let own_device = crate::relay::read_device_uuid();
    let now = crate::shared::time::now_epoch_f64();

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

    let sync_map: std::collections::HashMap<String, String> = db
        .kv_prefix("relay_sync_time_")
        .unwrap_or_default()
        .into_iter()
        .collect();

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

    let mut online_parts = Vec::new();
    let mut offline_parts = Vec::new();
    let mut sorted_devices: Vec<_> = device_to_short.iter().collect();
    sorted_devices.sort_by(|a, b| a.1.cmp(b.1));

    for (device_id, short) in sorted_devices {
        let sync_time: f64 = sync_map
            .get(&format!("relay_sync_time_{device_id}"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let is_online = sync_time > 0.0 && (now - sync_time) <= relay::DEVICE_STALE_SECS;

        let agents = agent_counts.get(device_id).copied().unwrap_or(0);
        let mut details = Vec::new();
        if sync_time > 0.0 {
            details.push(format_time(sync_time));
        }
        if is_online && agents == 0 {
            details.push("no agents".to_string());
        }
        let label = if details.is_empty() {
            short.clone()
        } else {
            format!("{} ({})", short, details.join(", "))
        };
        if is_online {
            online_parts.push(label);
        } else {
            offline_parts.push(label);
        }
    }

    let total = online_parts.len() + offline_parts.len();
    if total > 0 {
        println!("\nDevices:   {} known", total + 1); // +1 for self
        if !online_parts.is_empty() {
            println!("  online:  {}", online_parts.join(", "));
        }
        if !offline_parts.is_empty() {
            println!("  seen:    {}", offline_parts.join(", "));
        }
    } else {
        println!("\nNo other devices");
    }

    // Show key fingerprint so two devices can visually verify they share the
    // same PSK without anyone leaking material.
    if let Ok(psk) = relay::load_psk(&config) {
        println!("Key:       {}", relay::crypto::fingerprint(&psk));
    } else {
        println!("Key:       {FG_RED}missing{RESET} (run `hcom relay new`)");
    }

    println!("\nShow token: hcom relay token");

    0
}

/// Internal fast-path: wake the relay worker so queued events publish immediately.
fn relay_push() -> i32 {
    crate::relay::trigger_push();
    0
}

fn stop_relay_worker_quiet() {
    let Some(pid) = crate::relay::worker::relay_worker_pid() else {
        return;
    };

    let _ = crate::relay::worker::stop_relay_worker();
    for _ in 0..50 {
        if !crate::pidtrack::is_alive(pid) {
            crate::relay::worker::remove_relay_pid_file();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Last resort: a stale worker pinned to the old namespace must not survive
    // a relay reset or shutdown.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    crate::relay::worker::remove_relay_pid_file();
}

fn restart_relay_worker_for_config_change() -> bool {
    if crate::relay::worker::is_relay_worker_running() {
        stop_relay_worker_quiet();
    }
    crate::relay::worker::ensure_worker(false)
}

fn ensure_relay_worker_running_for_cli() -> bool {
    if crate::relay::worker::ensure_worker(false) {
        return true;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if crate::relay::worker::is_relay_worker_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        if crate::relay::worker::ensure_worker(false) {
            return true;
        }
    }

    crate::relay::worker::is_relay_worker_running()
}

fn known_remote_device_shorts(db: &HcomDb) -> Vec<String> {
    let own_device = crate::relay::read_device_uuid();
    let mut shorts = Vec::new();
    if let Ok(entries) = db.kv_prefix("relay_short_") {
        for (key, device_id) in entries {
            if device_id == own_device {
                continue;
            }
            shorts.push(key.strip_prefix("relay_short_").unwrap_or(&key).to_string());
        }
    }
    shorts.sort();
    shorts
}

fn relay_notify_off_all(db: &HcomDb, config: &crate::config::HcomConfig) {
    let peers = known_remote_device_shorts(db);
    if peers.is_empty() {
        println!("No known remote peers to notify.");
        return;
    }

    let mut sent = 0usize;
    for short in &peers {
        if relay::control::send_one_way_control_ephemeral(
            config,
            "relay_off",
            short,
            &serde_json::json!({}),
        ) {
            sent += 1;
        }
    }

    println!(
        "Best-effort relay_off sent to {sent}/{} known remote peer(s).",
        peers.len()
    );
}

fn relay_off(db: &HcomDb, argv: &[String]) -> i32 {
    let all = argv.iter().any(|a| a == "--all");
    if argv.iter().any(|a| a != "--all") {
        eprintln!("Usage: hcom relay off [--all]");
        return 1;
    }

    let config = config::load_config_snapshot().core;
    if config.relay_id.is_empty() {
        eprintln!("No relay configured.");
        eprintln!("Run: hcom relay new");
        return 1;
    }

    if all {
        if config.relay_enabled {
            relay_notify_off_all(db, &config);
        } else {
            println!("Relay already disabled locally; skipping remote shutdown broadcast.");
        }
    }

    match relay::control::disable_local_relay(&config) {
        Ok(cleared_remote_state) => {
            if cleared_remote_state {
                println!("Cleared remote state");
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    }

    stop_relay_worker_quiet();
    println!("{FG_YELLOW}Relay: disabled{RESET}");
    println!("\nRun 'hcom relay connect' to reconnect");
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

    if enable {
        if let Err(e) = validate_existing_relay_config(&config) {
            eprintln!("Error: {e}");
            return 1;
        }
        let config_path = crate::paths::config_toml_path();
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            let new_content = update_toml_key(&content, "relay_enabled", "true");
            if let Err(e) = crate::config::write_config_toml_path(&config_path, &new_content) {
                eprintln!("Error: Failed to write config: {e}");
                return 1;
            }
        }
        println!("Relay enabled\n");
        if !ensure_relay_worker_running_for_cli() {
            eprintln!("Error: relay daemon could not be started");
            return 1;
        }
        relay_status(db)
    } else {
        relay_off(db, &[])
    }
}

/// Persist relay settings to config.toml.
/// Relay auth is cleared when no password is provided so stale credentials
/// don't poison future joins/reconfigurations.
fn render_relay_config_content(
    content: &str,
    relay_id: &str,
    broker: &str,
    auth_token: Option<&str>,
    psk_b64: &str,
) -> String {
    let mut content = update_toml_key(content, "relay_id", &format!("\"{relay_id}\""));
    content = update_toml_key(&content, "relay", &format!("\"{broker}\""));
    content = update_toml_key(&content, "relay_enabled", "true");
    content = update_toml_key(&content, "relay_psk", &format!("\"{psk_b64}\""));
    update_toml_key(
        &content,
        "relay_token",
        &format!("\"{}\"", auth_token.unwrap_or("")),
    )
}

fn persist_relay_config(
    relay_id: &str,
    broker: &str,
    auth_token: Option<&str>,
    psk: &[u8; 32],
) -> Result<(), String> {
    let config_path = crate::paths::config_toml_path();
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let psk_b64 = relay::encode_psk(psk);
    let content = render_relay_config_content(&content, relay_id, broker, auth_token, &psk_b64);
    crate::config::write_config_toml_path(&config_path, &content)
        .map_err(|e| format!("Failed to write config: {e}"))
}

/// Create a new relay group.
fn relay_new(db: &HcomDb, argv: &[String]) -> i32 {
    let (broker_url, auth_token, _) = parse_broker_flags(argv);

    // Ensure device_id file exists before spawning the daemon worker,
    // so both CLI and daemon use the same UUID (avoids TOCTOU race).
    relay::read_device_uuid();

    let config = config::load_config_snapshot().core;

    // Show previous group token if switching
    if let Some(old_token) = current_join_token(&config) {
        println!("Current group: hcom relay connect {old_token}\n");
    }

    // Clear stale device state from the previous relay group so that
    // relay_short_* / relay_caps_* only reflect the new token's peers.
    relay::clear_relay_device_state(db);

    let relay_id = uuid::Uuid::new_v4().to_string();
    let psk = match relay::crypto::generate_psk() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: failed to generate relay key: {e}");
            return 1;
        }
    };
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
    if let Err(e) = persist_relay_config(&relay_id, &pinned_broker, auth_token.as_deref(), &psk) {
        eprintln!("Error: {e}");
        return 1;
    }

    // Generate join token (always v0x04 — includes relay PSK)
    if let Some(token) = encode_join_token(&relay_id, &pinned_broker, &psk) {
        println!("\nBroker: {pinned_broker}");
        println!(
            "Key:    {} (XChaCha20-Poly1305)",
            relay::crypto::fingerprint(&psk)
        );
        if auth_token.is_some() {
            println!("Password: set");
        }
        println!("\nOn other devices: hcom relay connect {token}");
        if auth_token.is_some() {
            println!("  (they will also need: --password <secret>)");
        }
    }

    if restart_relay_worker_for_config_change() {
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

    // Ensure device_id file exists before spawning the daemon worker,
    // so both CLI and daemon use the same UUID (avoids TOCTOU race).
    relay::read_device_uuid();

    let token_str = remaining.first().filter(|s| !s.starts_with("-")).cloned();

    if token_str.is_none() {
        // Re-enable mode
        let config = config::load_config_snapshot().core;
        if config.relay_id.is_empty() {
            eprintln!("No relay configured.");
            eprintln!("Run: hcom relay new");
            return 1;
        }
        if let Err(e) = validate_existing_relay_config(&config) {
            eprintln!("Error: {e}");
            return 1;
        }
        if config.relay_enabled {
            println!("Relay already enabled.\n");
            if !ensure_relay_worker_running_for_cli() {
                eprintln!("Error: relay is enabled but the daemon could not be started");
                return 1;
            }
            return relay_status(db);
        }
        return relay_toggle(db, true);
    }

    let token_str = token_str.unwrap();

    // Decode token
    let decoded = match decode_join_token(&token_str) {
        Some(r) => r,
        None => {
            eprintln!("Invalid token.");
            return 1;
        }
    };

    let psk = match decoded.psk {
        Some(p) => p,
        None => {
            eprintln!(
                "Legacy token (v0x01/v0x02) — rejected.\n\
                 \n\
                 This hcom build requires a token that carries the relay key. Ask the source\n  \
                 device to upgrade hcom and run\n  \
                 hcom relay new\n\
                 then re-share the new token. The new token format is v0x04 (~67 chars)."
            );
            return 1;
        }
    };

    let relay_id = decoded.relay_id;
    let token_broker = decoded.broker_url;

    let effective_broker = broker_url.unwrap_or(token_broker);

    // Test broker connectivity
    let ping_ms = relay::parse_broker_url(&effective_broker)
        .and_then(|(host, port, use_tls)| ping_broker(&host, port, use_tls));

    let config = config::load_config_snapshot().core;

    // Clear stale device state when switching groups
    if config.relay_id != relay_id {
        relay::clear_relay_device_state(db);
    }

    // Save config
    if let Err(e) = persist_relay_config(&relay_id, &effective_broker, auth_token.as_deref(), &psk)
    {
        eprintln!("Error: {e}");
        return 1;
    }

    if let Some(ms) = ping_ms {
        println!("Broker: {effective_broker} ({ms}ms)");
    } else {
        println!("Broker: {effective_broker}");
        eprintln!("  Warning: broker unreachable — check network or token");
    }
    println!("Key:    {}", relay::crypto::fingerprint(&psk));

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

    if restart_relay_worker_for_config_change() {
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
        "relay_psk" => ("relay", "psk"),
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
             hcom relay new              Create new relay group (generates fresh key)\n  \
             hcom relay token            Show join token\n  \
             hcom relay connect          Re-enable existing relay\n  \
             hcom relay connect <token>  Join relay from another device\n  \
             hcom relay off              Disable relay sync\n  \
             hcom relay off --all        Ask all known peers to disable this relay too, then disable locally\n  \
             hcom relay disconnect       Disable relay sync\n  \
             hcom relay push             Trigger an immediate relay push\n  \
             \n\
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
        "off" | "disconnect" => relay_off(db, &argv[1..]),
        "on" => relay_connect(db, &Vec::new()),
        "status" => relay_status(db),
        "token" => relay_show_token(),
        "push" => relay_push(),
        "daemon" => crate::commands::daemon::cmd_daemon(&argv[1..]),
        _ => {
            // Could be a token passed directly
            if argv[0].len() > 20 && !argv[0].starts_with('-') {
                relay_connect(db, argv)
            } else {
                eprintln!("Error: Unknown subcommand: {first}");
                eprintln!("Usage: hcom relay [new|connect|disconnect|status|push]");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::isolated_test_env;
    use serial_test::serial;

    fn fake_psk() -> [u8; 32] {
        [0x44; 32]
    }

    #[test]
    fn test_encode_decode_public_broker_token() {
        let relay_id = uuid::Uuid::new_v4().to_string();
        let broker = format!("mqtts://{}:{}", DEFAULT_BROKERS[0].0, DEFAULT_BROKERS[0].1);
        let psk = fake_psk();
        let token = encode_join_token(&relay_id, &broker, &psk).unwrap();
        let decoded = decode_join_token(&token).unwrap();
        assert_eq!(decoded.relay_id, relay_id);
        assert_eq!(decoded.broker_url, broker);
        assert_eq!(decoded.psk, Some(psk));
    }

    #[test]
    fn test_encode_decode_private_broker_token() {
        let relay_id = uuid::Uuid::new_v4().to_string();
        let broker = "mqtts://my-broker.example.com:8883";
        let psk = fake_psk();
        let token = encode_join_token(&relay_id, broker, &psk).unwrap();
        let decoded = decode_join_token(&token).unwrap();
        assert_eq!(decoded.relay_id, relay_id);
        assert_eq!(decoded.broker_url, broker);
        assert_eq!(decoded.psk, Some(psk));
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

    #[test]
    #[serial]
    fn test_persist_relay_config_clears_stale_token_when_password_omitted() {
        let _ = isolated_test_env();
        let psk_b64 = relay::encode_psk(&fake_psk());
        let contents = render_relay_config_content(
            "[relay]\nurl = \"mqtt://old:1883\"\nid = \"old-id\"\ntoken = \"stale-secret\"\npsk = \"old-psk\"\nenabled = true\n",
            "new-id",
            "mqtt://127.0.0.1:1",
            None,
            &psk_b64,
        );
        let doc: toml_edit::DocumentMut = contents.parse().unwrap();
        assert_eq!(doc["relay"]["id"].as_str(), Some("new-id"));
        assert_eq!(doc["relay"]["url"].as_str(), Some("mqtt://127.0.0.1:1"));
        assert_eq!(doc["relay"]["token"].as_str(), Some(""));
        assert_eq!(doc["relay"]["psk"].as_str(), Some(psk_b64.as_str()));
        assert_eq!(doc["relay"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn test_legacy_token_rejected_in_connect_decode() {
        // A v0x01 (plaintext) token decodes to psk=None; relay_connect refuses
        // it instead of writing config.
        let relay_id = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let broker = format!("mqtts://{}:{}", DEFAULT_BROKERS[0].0, DEFAULT_BROKERS[0].1);
        let legacy = relay::token::encode_join_token(relay_id, &broker, None).unwrap();
        let decoded = decode_join_token(&legacy).unwrap();
        assert!(decoded.psk.is_none());
    }

    #[test]
    #[serial]
    fn test_relay_off_all_disables_local_relay_without_peers() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let mut cfg = crate::config::HcomConfig::default();
        cfg.relay = "mqtts://broker.emqx.io:8883".to_string();
        cfg.relay_id = "relay-1".to_string();
        cfg.relay_psk = relay::encode_psk(&fake_psk());
        cfg.relay_enabled = true;
        crate::config::save_toml_config(&cfg, None).unwrap();

        let db = HcomDb::open().unwrap();
        let args = RelayArgs {
            args: vec!["off".to_string(), "--all".to_string()],
        };
        assert_eq!(cmd_relay(&db, &args, None), 0);

        let updated = crate::config::HcomConfig::load(None).unwrap();
        assert!(!updated.relay_enabled);
    }

    #[test]
    #[serial]
    fn test_relay_push_subcommand_exists() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let args = RelayArgs {
            args: vec!["push".to_string()],
        };
        assert_eq!(cmd_relay(&db, &args, None), 0);
    }

    #[test]
    #[serial]
    fn test_relay_on_rejects_invalid_stored_config_without_enabling() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let mut content = String::new();
        content = update_toml_key(&content, "relay_id", "\"relay-1\"");
        content = update_toml_key(&content, "relay", "\"\"");
        content = update_toml_key(&content, "relay_psk", "\"not-valid-base64\"");
        content = update_toml_key(&content, "relay_enabled", "false");
        std::fs::write(crate::paths::config_toml_path(), content).unwrap();

        let db = HcomDb::open().unwrap();
        let args = RelayArgs {
            args: vec!["on".to_string()],
        };
        assert_eq!(cmd_relay(&db, &args, None), 1);

        let updated = crate::config::HcomConfig::load(None).unwrap();
        assert!(!updated.relay_enabled);
    }
}
