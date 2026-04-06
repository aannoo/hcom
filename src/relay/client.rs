//! MQTT client lifecycle — connect, subscribe, LWT, reconnect with backoff.
//!
//! Uses rumqttc v5 blocking Connection iterator in a dedicated thread.
//! Manual exponential backoff on connection errors (rumqttc auto-reconnects
//! with zero delay; we add sleep between retries).

use rumqttc::TlsConfiguration;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::{Client, Connection, Event, MqttOptions};
use rustls::RootCertStore;
use rustls_native_certs::load_native_certs;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::log;

use super::{
    get_broker_from_config, is_relay_enabled, read_device_uuid, set_relay_status, state_topic,
    wildcard_topic,
};

/// Build a TLS config that combines webpki-roots (bundled Mozilla CAs for Android/Termux
/// compatibility) with native system certs (for private broker support).
/// This ensures public brokers work everywhere while preserving user-installed CA support.
fn relay_tls_config() -> TlsConfiguration {
    let mut root_store = RootCertStore::empty();

    // Add webpki-roots as the base — fixes Android/Termux where rustls-native-certs fails
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Also add native system certs if available, for private broker support
    let native_certs = load_native_certs();
    for cert in native_certs.certs {
        let _ = root_store.add(cert);
    }
    if !native_certs.errors.is_empty() {
        log::log_warn(
            "relay",
            "relay.native_certs_partial",
            &format!(
                "failed to load {} native cert(s); continuing with bundled roots",
                native_certs.errors.len()
            ),
        );
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    TlsConfiguration::Rustls(Arc::new(tls_config))
}

/// Commands sent from the main thread to the relay event loop.
pub enum RelayCommand {
    /// Trigger an immediate push cycle.
    Push,
    /// Shut down gracefully.
    Shutdown,
}

/// Exponential backoff state. Doubles on each error up to max, resets on success.
struct Backoff {
    current: Duration,
    max: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            current: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }

    fn wait_duration(&self) -> Duration {
        self.current
    }

    fn increase(&mut self) {
        self.current = (self.current * 2).min(self.max);
    }

    fn reset(&mut self) {
        self.current = Duration::from_secs(1);
    }
}

/// MQTT relay client. Manages connection, subscriptions, push/pull, and lifecycle.
pub struct MqttRelay {
    client: Client,
    relay_id: String,
    device_uuid: String,
    /// Channel to receive commands (push, shutdown) from external callers.
    cmd_rx: mpsc::Receiver<RelayCommand>,
    /// Push interval (seconds between automatic push cycles).
    push_interval: Duration,
}

impl MqttRelay {
    /// Create and connect the MQTT relay client.
    ///
    /// Returns (MqttRelay, Connection, command_sender). The Connection must be
    /// polled in a loop (its iterator drives the network I/O). The command_sender
    /// lets external code trigger pushes or shutdown.
    pub fn connect(
        config: &HcomConfig,
    ) -> Result<(Self, Connection, mpsc::Sender<RelayCommand>), String> {
        if !is_relay_enabled(config) {
            return Err("relay not configured or disabled".into());
        }

        let (host, port, use_tls) = get_broker_from_config(config).ok_or("no broker configured")?;

        let relay_id = config.relay_id.clone();
        let device_uuid = read_device_uuid();
        let client_id = format!("hcom-{}", &device_uuid[..8.min(device_uuid.len())]);

        let mut mqttoptions = MqttOptions::new(&client_id, &host, port);
        mqttoptions.set_keep_alive(Duration::from_secs(30));
        mqttoptions.set_clean_start(true);
        mqttoptions.set_max_packet_size(Some(128 * 1024));

        // TLS
        if use_tls {
            mqttoptions.set_transport(rumqttc::Transport::tls_with_config(relay_tls_config()));
        }

        // Auth
        if !config.relay_token.is_empty() {
            mqttoptions.set_credentials("hcom", &config.relay_token);
        }

        // LWT: publish empty retained payload on ungraceful disconnect so remote
        // devices detect our absence and clean up our instances.
        let lwt_topic = state_topic(&relay_id, &device_uuid);
        let lwt = rumqttc::v5::mqttbytes::v5::LastWill {
            topic: lwt_topic.clone().into(),
            message: bytes::Bytes::new(),
            qos: QoS::AtLeastOnce,
            retain: true,
            properties: None,
        };
        mqttoptions.set_last_will(lwt);

        // Create client + connection (cap=10 for outgoing message buffer)
        let (client, connection) = Client::new(mqttoptions, 10);

        let (cmd_tx, cmd_rx) = mpsc::channel();

        let relay = MqttRelay {
            client,
            relay_id,
            device_uuid,
            cmd_rx,
            push_interval: Duration::from_secs(5),
        };

        log::log_info(
            "relay",
            "relay.connect",
            &format!("connecting to {}:{}", host, port),
        );

        Ok((relay, connection, cmd_tx))
    }

    /// Subscribe to relay topics. Called on initial connect and after every reconnect.
    pub fn subscribe(&self) -> Result<(), String> {
        let topic = wildcard_topic(&self.relay_id);
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .map_err(|e| format!("subscribe failed: {}", e))?;
        log::log_info(
            "relay",
            "relay.subscribe",
            &format!("subscribed to {}", topic),
        );
        Ok(())
    }

    /// Run the main relay event loop. Blocks until shutdown.
    ///
    /// Spawns a thread for the Connection iterator (which drives network I/O)
    /// and processes events + commands in the main thread with timeout-based polling.
    /// Uses manual exponential backoff on connection errors.
    pub fn run(self, connection: Connection) {
        // Forward MQTT events from connection thread to main thread via channel.
        // Connection::iter() blocks, so it must run in a dedicated thread.
        let (event_tx, event_rx) = mpsc::channel();

        thread::spawn(move || {
            let mut connection = connection;
            for notification in connection.iter() {
                if event_tx.send(notification).is_err() {
                    break; // Main thread dropped receiver
                }
            }
        });

        let mut backoff = Backoff::new();
        let mut backoff_until = Instant::now();
        let mut last_push = Instant::now();
        let mut connected = false;

        // Initial subscribe
        if let Err(e) = self.subscribe() {
            log::log_warn("relay", "relay.subscribe_err", &e);
        }

        loop {
            // Check for commands (non-blocking, always responsive)
            match self.cmd_rx.try_recv() {
                Ok(RelayCommand::Shutdown) => {
                    log::log_info("relay", "relay.shutdown", "shutdown requested");
                    self.shutdown_graceful(&event_rx);
                    return;
                }
                Ok(RelayCommand::Push) => {
                    self.do_push_cycle();
                    last_push = Instant::now();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::log_info("relay", "relay.shutdown", "command channel closed");
                    self.shutdown_graceful(&event_rx);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            // Periodic push
            if connected && last_push.elapsed() >= self.push_interval {
                self.do_push_cycle();
                last_push = Instant::now();
            }

            // During backoff, skip event processing and just sleep
            if Instant::now() < backoff_until {
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Poll MQTT events with timeout (allows command checks between polls)
            match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    backoff.reset();
                    self.handle_event(event, &mut connected);
                }
                Ok(Err(conn_err)) => {
                    if connected {
                        connected = false;
                        let err_msg = format!("{:?}", conn_err);
                        log::log_warn("relay", "relay.disconnected", &err_msg);
                        if let Ok(db) = HcomDb::open() {
                            set_relay_status(&db, "error", Some(&err_msg), true);
                        }
                    }
                    backoff_until = Instant::now() + backoff.wait_duration();
                    backoff.increase();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No events — loop back to check commands and push timer
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Connection thread died
                    log::log_info("relay", "relay.shutdown", "connection thread ended");
                    self.shutdown_graceful(&event_rx);
                    return;
                }
            }
        }
    }

    /// Handle a single MQTT event from the Connection iterator.
    fn handle_event(&self, event: Event, connected: &mut bool) {
        match event {
            Event::Incoming(incoming) => match incoming {
                Packet::ConnAck(_connack) => {
                    *connected = true;
                    log::log_info("relay", "relay.connected", "MQTT connected");
                    if let Ok(db) = HcomDb::open() {
                        set_relay_status(&db, "ok", None, true);
                    }
                    // Re-subscribe after reconnect
                    if let Err(e) = self.subscribe() {
                        log::log_warn("relay", "relay.subscribe_err", &e);
                    }
                    // Push immediately on connect to sync state
                    self.do_push_cycle();
                }
                Packet::Publish(publish) => {
                    let topic = String::from_utf8_lossy(&publish.topic).to_string();
                    let payload = publish.payload.to_vec();
                    self.handle_incoming_message(&topic, &payload);
                }
                Packet::Disconnect(_) => {
                    *connected = false;
                    log::log_info("relay", "relay.disconnected", "server disconnect");
                }
                _ => {} // PingResp, SubAck, PubAck — ignore
            },
            Event::Outgoing(_) => {} // Outgoing events — ignore
        }
    }

    /// Handle an incoming MQTT publish message.
    ///
    /// Topic layout: `{relay_id}/{device_uuid}` for state snapshots,
    /// `{relay_id}/control` for control events. Empty payload on a state topic
    /// means "device gone" (LWT or graceful cleanup).
    fn handle_incoming_message(&self, topic: &str, payload: &[u8]) {
        let prefix = format!("{}/", self.relay_id);
        if !topic.starts_with(&prefix) {
            return; // Not our relay group
        }
        let suffix = &topic[prefix.len()..];

        let db = match HcomDb::open() {
            Ok(db) => db,
            Err(e) => {
                log::log_error("relay", "relay.db_err", &format!("{}", e));
                return;
            }
        };

        if payload.is_empty() {
            // Empty payload = device disconnected (LWT or graceful cleanup)
            if !suffix.is_empty() && suffix != "control" {
                super::pull::handle_device_gone(&db, suffix);
            }
            return;
        }

        if suffix == "control" {
            super::pull::handle_control_message(&db, payload, &self.device_uuid);
        } else {
            // State message from a remote device
            let device_id = suffix;
            if device_id == self.device_uuid {
                return; // Ignore own messages
            }
            super::pull::handle_state_message(&db, device_id, payload, &self.device_uuid);
        }
    }

    /// Execute a push cycle: build state + events, publish to MQTT.
    fn do_push_cycle(&self) {
        let db = match HcomDb::open() {
            Ok(db) => db,
            Err(e) => {
                log::log_error("relay", "relay.db_err", &format!("{}", e));
                return;
            }
        };

        // Drain loop with 10s budget
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match super::push::push(&db, &self.client, &self.relay_id, &self.device_uuid, true) {
                Ok((true, has_more)) => {
                    if has_more && Instant::now() < deadline {
                        continue; // More events to drain
                    }
                    break;
                }
                Ok((false, _)) => break,
                Err(e) => {
                    log::log_warn("relay", "relay.push_err", &e);
                    if let Ok(db) = HcomDb::open() {
                        set_relay_status(&db, "error", Some(&e), true);
                    }
                    break;
                }
            }
        }
    }

    /// Graceful shutdown: publish empty retained message to clear device state,
    /// wait for PUBACK, then disconnect.
    fn shutdown_graceful(
        &self,
        event_rx: &mpsc::Receiver<Result<Event, rumqttc::v5::ConnectionError>>,
    ) {
        let topic = state_topic(&self.relay_id, &self.device_uuid);
        log::log_info(
            "relay",
            "relay.shutdown_graceful",
            "clearing retained state",
        );

        // Publish empty retained to clear our state from broker
        if let Err(e) = self.client.publish(
            &topic,
            QoS::AtLeastOnce,
            true, // retain
            vec![],
        ) {
            log::log_warn("relay", "relay.shutdown_publish_err", &format!("{}", e));
        } else {
            // Wait for PUBACK (up to 5s) by draining the event channel
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match event_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => break,
                    Ok(Err(_)) => break, // Connection error
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => continue, // Other events or timeout — keep waiting
                }
            }
        }

        if let Err(e) = self.client.disconnect() {
            log::log_warn("relay", "relay.disconnect_err", &format!("{}", e));
        }

        // Update status in DB
        if let Ok(db) = HcomDb::open() {
            set_relay_status(&db, "disconnected", None, true);
        }
    }

    /// Get relay_id.
    pub fn relay_id(&self) -> &str {
        &self.relay_id
    }

    /// Get device_uuid.
    pub fn device_uuid(&self) -> &str {
        &self.device_uuid
    }
}

/// Tracks PUBACK or connection error for an ephemeral publish.
#[derive(Default)]
struct PubResult {
    acked: bool,
    errored: bool,
}

/// Ephemeral MQTT client for one-shot publishes (CLI callers like stop/kill).
/// Wraps a rumqttc Client with PUBACK tracking so callers can wait for
/// delivery confirmation instead of blindly sleeping.
pub struct EphemeralClient {
    client: Client,
    /// Signaled on PubAck (acked=true) or connection error (errored=true).
    pub_result: Arc<(Mutex<PubResult>, Condvar)>,
}

impl EphemeralClient {
    /// Publish a message and wait for PUBACK (up to `timeout`).
    /// Returns true if the broker acknowledged delivery within the timeout.
    /// Returns false immediately on connection error (no 5s wait).
    pub fn publish_and_wait(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> bool {
        if self.client.publish(topic, qos, retain, payload).is_err() {
            return false;
        }

        let (lock, cvar) = &*self.pub_result;
        let guard = match lock.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };

        // Exit wait on either ack or error
        let (result, _) = cvar
            .wait_timeout_while(guard, timeout, |r| !r.acked && !r.errored)
            .ok()
            .unzip();

        result.map(|r| r.acked).unwrap_or(false)
    }

    /// Get a reference to the underlying rumqttc Client.
    pub fn client_ref(&self) -> &Client {
        &self.client
    }

    /// Disconnect the ephemeral client.
    pub fn disconnect(self) {
        let _ = self.client.disconnect();
    }
}

/// Create an ephemeral MQTT client for one-shot publishes (CLI callers like stop/kill).
/// Connects, waits for CONNACK (up to 5s), disconnects on failure. Returns None on failure.
/// The returned EphemeralClient tracks PUBACK so callers can wait for delivery confirmation.
pub fn create_ephemeral_client(config: &HcomConfig) -> Option<EphemeralClient> {
    let (host, port, use_tls) = super::get_broker_from_config(config)?;

    let client_id = format!("hcom-ephemeral-{}", std::process::id());
    let mut mqttoptions = MqttOptions::new(&client_id, &host, port);
    mqttoptions.set_keep_alive(Duration::from_secs(10));
    mqttoptions.set_clean_start(true);

    if use_tls {
        mqttoptions.set_transport(rumqttc::Transport::tls_with_config(relay_tls_config()));
    }

    if !config.relay_token.is_empty() {
        mqttoptions.set_credentials("hcom", &config.relay_token);
    }

    let (client, connection) = Client::new(mqttoptions, 10);

    // Shared state for CONNACK wait
    let connected = Arc::new((Mutex::new(false), Condvar::new()));
    let connected_clone = connected.clone();

    // Shared state for PUBACK tracking (single-shot: any PubAck means our publish was confirmed)
    let pub_result = Arc::new((Mutex::new(PubResult::default()), Condvar::new()));
    let pub_result_clone = pub_result.clone();

    // Spawn a background thread to drive the connection event loop.
    thread::spawn(move || {
        let mut connection = connection;
        for event in connection.iter() {
            match &event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    let (lock, cvar) = &*connected_clone;
                    if let Ok(mut flag) = lock.lock() {
                        *flag = true;
                        cvar.notify_one();
                    }
                }
                Ok(Event::Incoming(Packet::PubAck(_))) => {
                    let (lock, cvar) = &*pub_result_clone;
                    if let Ok(mut r) = lock.lock() {
                        r.acked = true;
                        cvar.notify_one();
                    }
                }
                Err(_) => {
                    // Signal failure so waiters don't block forever.
                    // Must hold mutex when notifying to avoid lost-wakeup race.
                    // Leave flag=false so waiter knows connection failed.
                    let (lock, cvar) = &*connected_clone;
                    if let Ok(_g) = lock.lock() {
                        cvar.notify_one();
                    }
                    let (lock, cvar) = &*pub_result_clone;
                    if let Ok(mut r) = lock.lock() {
                        r.errored = true;
                        cvar.notify_one();
                    }
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait for CONNACK with 5s timeout
    let (lock, cvar) = &*connected;
    let guard = lock.lock().ok()?;
    let (flag, _) = cvar.wait_timeout(guard, Duration::from_secs(5)).ok()?;

    if !*flag {
        let _ = client.disconnect();
        return None;
    }

    Some(EphemeralClient { client, pub_result })
}

/// Publish empty retained to clear device state and disconnect an ephemeral client.
pub fn clear_retained_state(config: &HcomConfig) -> bool {
    if config.relay_id.is_empty() {
        return false;
    }
    let relay_id = &config.relay_id;

    let device_uuid = read_device_uuid();
    let topic = state_topic(relay_id, &device_uuid);

    let client = match create_ephemeral_client(config) {
        Some(c) => c,
        None => return false,
    };

    let result = client.publish_and_wait(
        &topic,
        QoS::AtLeastOnce,
        true,
        vec![],
        Duration::from_secs(5),
    );

    client.disconnect();
    result
}
