//! Remote conversations come over the relay transcript RPC. Nothing polls:
//! a request goes out when the cursor settles on an agent, when that agent's
//! synced status changes, or on `R`. Each refresh is one job of at most three
//! RPCs, shrinking the request when the reply may have been too big.
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::convert::turns_from_remote_content;
use super::model::{ConversationSnapshot, TurnView};
use crate::tui::model::{Agent, AgentStatus};

pub const REMOTE_LAST: usize = 5;
pub const RETRY_LAST: usize = 2;
pub const FINAL_LAST: usize = 1;
pub const MAX_IN_FLIGHT: usize = 2;
pub const CURSOR_SETTLE: Duration = Duration::from_millis(400);
pub const STATUS_DEBOUNCE: Duration = Duration::from_secs(1);
pub const STATUS_MAX_WAIT: Duration = Duration::from_secs(3);
const CACHE_LIMIT: usize = 32;
/// Set by the relay when it cut a reply to fit a publish.
const TRUNCATED_MARKER: &str = "_relay_truncated";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRequest {
    /// `base:DEVICE`
    pub key: String,
    pub device: String,
    pub base_name: String,
    pub last: usize,
    pub detailed: bool,
}

impl RemoteRequest {
    /// Same payload `hcom transcript name:DEVICE --json` sends.
    pub fn params(&self) -> Value {
        json!({
            "target": self.base_name,
            "display_target": self.key,
            "origin_device": self.device,
            "last": self.last,
            "json": true,
            "detailed": self.detailed,
        })
    }
}

pub type Dispatcher = Arc<dyn Fn(&RemoteRequest) -> Result<Value, String> + Send + Sync>;

fn real_dispatch(req: &RemoteRequest) -> Result<Value, String> {
    use crate::relay::control::{RPC_DEFAULT_TIMEOUT, dispatch_remote, rpc_action};
    let db = crate::db::HcomDb::open().map_err(|e| e.to_string())?;
    dispatch_remote(
        &db,
        &req.device,
        Some(&req.key),
        rpc_action::TRANSCRIPT,
        &req.params(),
        RPC_DEFAULT_TIMEOUT,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    Timeout,
    Offline,
    Truncated,
    Format,
    Remote(String),
}

impl FailureReason {
    pub fn label(&self) -> String {
        match self {
            Self::Timeout => "timeout".into(),
            Self::Offline => "offline".into(),
            Self::Truncated => "truncated".into(),
            Self::Format => "bad format".into(),
            Self::Remote(m) => m.clone(),
        }
    }

    /// A reply the broker drops for size shows up as a timeout.
    fn size_suspect(&self) -> bool {
        matches!(self, Self::Timeout | Self::Truncated)
    }
}

fn classify(result: &Result<Value, String>) -> Result<Vec<TurnView>, FailureReason> {
    match result {
        Err(e) if e.starts_with("timed out waiting for rpc_result") => Err(FailureReason::Timeout),
        Err(e) if e.contains(" is offline ") => Err(FailureReason::Offline),
        // The relay cut the result to fit a publish (`relay::push`).
        Err(e) if e.contains("too large to relay") => Err(FailureReason::Truncated),
        Err(e) => Err(FailureReason::Remote(
            e.lines()
                .next()
                .unwrap_or("remote error")
                .chars()
                .take(40)
                .collect(),
        )),
        Ok(v) if v.get(TRUNCATED_MARKER).is_some() => Err(FailureReason::Truncated),
        Ok(v) => {
            let content = v
                .get("content")
                .and_then(Value::as_str)
                .ok_or(FailureReason::Format)?;
            turns_from_remote_content(content).map_err(|_| FailureReason::Format)
        }
    }
}

struct Job {
    attempt: u8,
    reasons: Vec<FailureReason>,
    request_id: u64,
    /// A send held back by the in-flight cap: (last, detailed).
    held: Option<(usize, bool)>,
}

struct Entry {
    key: String,
    device: String,
    base_name: String,
    turns: Option<Vec<TurnView>>,
    last_success: Option<Instant>,
    failure: Option<String>,
    job: Option<Job>,
    /// A trigger arrived while a job ran; run one more after it.
    waiting: bool,
    baseline: Option<(AgentStatus, u64)>,
    /// (first, latest) unhandled status change.
    status_change: Option<(Instant, Instant)>,
}

struct Reply {
    key: String,
    request_id: u64,
    result: Result<Value, String>,
}

pub struct RemoteTranscriptWorker {
    dispatcher: Dispatcher,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    in_flight: usize,
    next_request_id: u64,
    entries: Vec<Entry>,
    cursor: Option<(String, Instant)>,
    settled: bool,
}

impl Default for RemoteTranscriptWorker {
    fn default() -> Self {
        Self::with_dispatcher(Arc::new(real_dispatch))
    }
}

fn key_of(agent: &Agent) -> String {
    format!(
        "{}:{}",
        agent.name,
        agent.device_name.as_deref().unwrap_or("")
    )
}

fn status_key(agent: &Agent) -> (AgentStatus, u64) {
    (agent.status, agent.status_time.to_bits())
}

impl RemoteTranscriptWorker {
    pub fn with_dispatcher(dispatcher: Dispatcher) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            dispatcher,
            tx,
            rx,
            in_flight: 0,
            next_request_id: 0,
            entries: Vec::new(),
            cursor: None,
            settled: false,
        }
    }

    fn entry_index(&mut self, agent: &Agent) -> usize {
        let key = key_of(agent);
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            return i;
        }
        if self.entries.len() >= CACHE_LIMIT
            && let Some(old) = self.entries.iter().position(|e| e.job.is_none())
        {
            self.entries.remove(old);
        }
        self.entries.push(Entry {
            key,
            device: agent.device_name.clone().unwrap_or_default(),
            base_name: agent.name.clone(),
            turns: None,
            last_success: None,
            failure: None,
            job: None,
            waiting: false,
            baseline: None,
            status_change: None,
        });
        self.entries.len() - 1
    }

    fn trigger(&mut self, idx: usize) {
        if self.entries[idx].job.is_some() {
            self.entries[idx].waiting = true;
            return;
        }
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.entries[idx].job = Some(Job {
            attempt: 1,
            reasons: Vec::new(),
            request_id,
            held: None,
        });
        self.send(idx, REMOTE_LAST, true);
    }

    fn send(&mut self, idx: usize, last: usize, detailed: bool) {
        let entry = &mut self.entries[idx];
        let Some(job) = entry.job.as_mut() else {
            return;
        };
        if self.in_flight >= MAX_IN_FLIGHT {
            job.held = Some((last, detailed));
            return;
        }
        job.held = None;
        self.in_flight += 1;
        let req = RemoteRequest {
            key: entry.key.clone(),
            device: entry.device.clone(),
            base_name: entry.base_name.clone(),
            last,
            detailed,
        };
        let request_id = job.request_id;
        let dispatcher = self.dispatcher.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatcher(&req)))
                    .unwrap_or_else(|_| Err("dispatch panicked".into()));
            let _ = tx.send(Reply {
                key: req.key,
                request_id,
                result,
            });
        });
    }

    fn finish(&mut self, idx: usize, cursor_key: Option<&str>) {
        let entry = &mut self.entries[idx];
        entry.job = None;
        if entry.waiting {
            entry.waiting = false;
            if cursor_key == Some(entry.key.as_str()) {
                self.trigger(idx);
            }
        }
    }

    fn handle_reply(&mut self, reply: Reply, now: Instant, cursor_key: Option<&str>) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        let Some(idx) = self.entries.iter().position(|e| e.key == reply.key) else {
            return false;
        };
        let entry = &mut self.entries[idx];
        if entry.job.as_ref().map(|j| j.request_id) != Some(reply.request_id) {
            return false;
        }
        match classify(&reply.result) {
            Ok(turns) => {
                entry.turns = Some(turns);
                entry.last_success = Some(now);
                entry.failure = None;
                self.finish(idx, cursor_key);
            }
            Err(reason) => {
                let job = entry.job.as_mut().expect("checked above");
                job.reasons.push(reason.clone());
                let next = match job.attempt {
                    1 => Some((RETRY_LAST, true)),
                    2 if job.reasons.iter().all(FailureReason::size_suspect) => {
                        Some((FINAL_LAST, false))
                    }
                    _ => None,
                };
                match next {
                    Some((last, detailed)) => {
                        job.attempt += 1;
                        self.send(idx, last, detailed);
                    }
                    None => {
                        entry.failure = Some(reason.label());
                        self.finish(idx, cursor_key);
                    }
                }
            }
        }
        true
    }

    /// `cursor` is the remote agent under the cursor while the tab is visible.
    pub fn poll(&mut self, cursor: Option<&Agent>, now: Instant) -> bool {
        let cursor_key = cursor.map(key_of);
        let mut changed = false;

        while let Ok(reply) = self.rx.try_recv() {
            changed |= self.handle_reply(reply, now, cursor_key.as_deref());
        }

        // Sends held by the in-flight cap. New jobs only for the cursor's agent.
        for idx in 0..self.entries.len() {
            if self.in_flight >= MAX_IN_FLIGHT {
                break;
            }
            let entry = &mut self.entries[idx];
            let Some(job) = entry.job.as_mut() else {
                continue;
            };
            let Some((last, detailed)) = job.held else {
                continue;
            };
            if job.attempt == 1 && cursor_key.as_deref() != Some(entry.key.as_str()) {
                entry.job = None;
                entry.waiting = false;
                continue;
            }
            self.send(idx, last, detailed);
        }

        match cursor {
            None => {
                self.cursor = None;
                for e in &mut self.entries {
                    e.status_change = None;
                }
            }
            Some(agent) => {
                let key = cursor_key.clone().expect("cursor is some");
                let idx = self.entry_index(agent);
                if self.cursor.as_ref().map(|c| &c.0) != Some(&key) {
                    for e in &mut self.entries {
                        e.status_change = None;
                    }
                    self.cursor = Some((key, now));
                    self.settled = false;
                    self.entries[idx].baseline = Some(status_key(agent));
                    changed = true;
                } else {
                    let since = self.cursor.as_ref().map(|c| c.1).unwrap_or(now);
                    if !self.settled && now.saturating_duration_since(since) >= CURSOR_SETTLE {
                        self.settled = true;
                        self.trigger(idx);
                        changed = true;
                    }
                    let entry = &mut self.entries[idx];
                    let current = status_key(agent);
                    if entry.baseline != Some(current) {
                        entry.baseline = Some(current);
                        let first = entry.status_change.map(|s| s.0).unwrap_or(now);
                        entry.status_change = Some((first, now));
                    }
                    if let Some((first, latest)) = entry.status_change
                        && (now.saturating_duration_since(latest) >= STATUS_DEBOUNCE
                            || now.saturating_duration_since(first) >= STATUS_MAX_WAIT)
                    {
                        entry.status_change = None;
                        if self.settled {
                            self.trigger(idx);
                            changed = true;
                        }
                    }
                }
            }
        }
        changed
    }

    pub fn snapshot(&self, agent: &Agent) -> ConversationSnapshot<'_> {
        let key = key_of(agent);
        match self.entries.iter().find(|e| e.key == key) {
            Some(e) => ConversationSnapshot {
                turns: e.turns.as_deref(),
                last_success: e.last_success,
                failure: e.failure.as_deref(),
                loading: e.job.is_some(),
                email: None,
                prompt: None,
            },
            None => ConversationSnapshot::default(),
        }
    }

    /// Refresh now, skipping the debounce. Before the cursor settles this is
    /// the settle's request too, so the agent gets one RPC, not two.
    pub fn force_refresh(&mut self, agent: &Agent, now: Instant) {
        let idx = self.entry_index(agent);
        let key = key_of(agent);
        if self.cursor.as_ref().map(|c| &c.0) != Some(&key) {
            for e in &mut self.entries {
                e.status_change = None;
            }
            self.cursor = Some((key, now));
            self.entries[idx].baseline = Some(status_key(agent));
        }
        self.settled = true;
        self.trigger(idx);
    }

    pub fn has_pending_timer(&self) -> bool {
        self.in_flight > 0
            || (self.cursor.is_some() && !self.settled)
            || self.entries.iter().any(|e| {
                e.status_change.is_some() || e.job.as_ref().is_some_and(|j| j.held.is_some())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_helpers::make_test_agent;
    use std::sync::Mutex;

    type Log = Arc<Mutex<Vec<RemoteRequest>>>;

    fn worker(replies: Vec<Result<Value, String>>) -> (RemoteTranscriptWorker, Log) {
        let log: Log = Arc::default();
        let queue = Arc::new(Mutex::new(replies));
        let l = log.clone();
        let w = RemoteTranscriptWorker::with_dispatcher(Arc::new(move |req: &RemoteRequest| {
            l.lock().unwrap().push(req.clone());
            let mut q = queue.lock().unwrap();
            if q.is_empty() {
                Ok(ok_reply())
            } else {
                q.remove(0)
            }
        }));
        (w, log)
    }

    fn ok_reply() -> Value {
        json!({"target": "luna", "content": r#"[{"position":1,"user":"q","action":"a","tools":[]}]"#})
    }

    fn remote(name: &str) -> Agent {
        let mut a = make_test_agent(name, 0.0);
        a.device_name = Some("ABCD".into());
        a
    }

    fn settle(w: &mut RemoteTranscriptWorker, a: &Agent, now: Instant) {
        for _ in 0..300 {
            w.poll(Some(a), now);
            if w.in_flight == 0 && w.entries.iter().all(|e| e.job.is_none()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("jobs never finished");
    }

    #[test]
    fn request_after_cursor_settles() {
        let (mut w, log) = worker(vec![]);
        let a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.poll(Some(&a), t0 + Duration::from_millis(399));
        assert!(log.lock().unwrap().is_empty());
        w.poll(Some(&a), t0 + Duration::from_millis(400));
        settle(&mut w, &a, t0 + Duration::from_millis(400));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].params(),
            json!({"target":"luna","display_target":"luna:ABCD","origin_device":"ABCD",
                   "last":5,"json":true,"detailed":true})
        );
        assert_eq!(w.snapshot(&a).turns.unwrap().len(), 1);
    }

    #[test]
    fn passing_cursor_sends_nothing() {
        let (mut w, log) = worker(vec![]);
        let t0 = Instant::now();
        for (i, n) in ["a", "b", "c", "d"].iter().enumerate() {
            w.poll(Some(&remote(n)), t0 + Duration::from_millis(100 * i as u64));
        }
        let d = remote("d");
        w.poll(Some(&d), t0 + Duration::from_millis(700));
        settle(&mut w, &d, t0 + Duration::from_millis(700));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].base_name, "d");
    }

    #[test]
    fn timeouts_shrink_then_fail_keeps_cache() {
        let timeout = || Err::<Value, _>("timed out waiting for rpc_result (10s)".to_string());
        let (mut w, log) = worker(vec![Ok(ok_reply()), timeout(), timeout(), timeout()]);
        let a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.poll(Some(&a), t0 + CURSOR_SETTLE);
        settle(&mut w, &a, t0 + CURSOR_SETTLE);
        w.force_refresh(&a, t0);
        settle(&mut w, &a, t0 + CURSOR_SETTLE);
        let lasts: Vec<(usize, bool)> = log
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.last, r.detailed))
            .collect();
        assert_eq!(lasts, vec![(5, true), (5, true), (2, true), (1, false)]);
        let snap = w.snapshot(&a);
        assert_eq!(snap.failure, Some("timeout"));
        assert_eq!(snap.turns.unwrap().len(), 1);
    }

    #[test]
    fn offline_stops_after_one_retry() {
        let off = || Err::<Value, _>("device ABCD is offline (last seen 5m ago)".to_string());
        let (mut w, log) = worker(vec![off(), off()]);
        let a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.poll(Some(&a), t0 + CURSOR_SETTLE);
        settle(&mut w, &a, t0 + CURSOR_SETTLE);
        assert_eq!(log.lock().unwrap().len(), 2);
        assert_eq!(w.snapshot(&a).failure, Some("offline"));
        assert!(w.snapshot(&a).turns.is_none());
    }

    #[test]
    fn status_change_triggers_after_debounce() {
        let (mut w, log) = worker(vec![]);
        let mut a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.poll(Some(&a), t0 + CURSOR_SETTLE);
        settle(&mut w, &a, t0 + CURSOR_SETTLE);
        a.status_time += 1.0;
        let t1 = t0 + Duration::from_secs(10);
        w.poll(Some(&a), t1);
        w.poll(Some(&a), t1 + Duration::from_millis(900));
        assert_eq!(log.lock().unwrap().len(), 1);
        w.poll(Some(&a), t1 + STATUS_DEBOUNCE);
        settle(&mut w, &a, t1 + STATUS_DEBOUNCE);
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[test]
    fn refresh_before_settle_is_the_settle_request() {
        let (mut w, log) = worker(vec![]);
        let a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.force_refresh(&a, t0 + Duration::from_millis(100));
        settle(&mut w, &a, t0 + Duration::from_millis(100));
        assert_eq!(log.lock().unwrap().len(), 1);
        w.poll(Some(&a), t0 + CURSOR_SETTLE);
        settle(&mut w, &a, t0 + CURSOR_SETTLE * 2);
        assert_eq!(log.lock().unwrap().len(), 1, "no second request at settle");
        assert!(!w.has_pending_timer());
    }

    #[test]
    fn oversized_reply_shrinks_to_one_turn() {
        let big = || {
            Err::<Value, _>(
                "transcript failed: rpc result too large to relay (300000 bytes); the relayed copy was cut"
                    .to_string(),
            )
        };
        let (mut w, log) = worker(vec![big(), big()]);
        let a = remote("luna");
        let t0 = Instant::now();
        w.poll(Some(&a), t0);
        w.poll(Some(&a), t0 + CURSOR_SETTLE);
        settle(&mut w, &a, t0 + CURSOR_SETTLE);
        let lasts: Vec<(usize, bool)> = log
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.last, r.detailed))
            .collect();
        assert_eq!(lasts, vec![(5, true), (2, true), (1, false)]);
        assert_eq!(w.snapshot(&a).turns.unwrap().len(), 1);
    }

    #[test]
    fn classify_replies() {
        assert_eq!(
            classify(&Ok(json!({"content": "nope"}))),
            Err(FailureReason::Format)
        );
        assert_eq!(
            classify(&Ok(json!({"content": "[]", "_relay_truncated": true}))),
            Err(FailureReason::Truncated)
        );
        let long = "transcript failed: ".to_string() + &"x".repeat(80);
        match classify(&Err(long)) {
            Err(FailureReason::Remote(m)) => assert_eq!(m.chars().count(), 40),
            other => panic!("{other:?}"),
        }
    }
}
