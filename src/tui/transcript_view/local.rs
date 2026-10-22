//! Local conversation reads happen off the UI thread, like `usage.rs`:
//! a transcript is re-parsed only when its file stamp changes.
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use super::convert::turn_from_exchange;
use super::model::{ConversationSnapshot, LocalSource, TurnView};
use super::prompt::{ScreenPrompt, parse_screen};
use crate::accounts::EmailCache;
use crate::tool::Tool;
use crate::transcript::{self, ReadOptions};
use crate::tui::model::{Agent, AgentStatus};

pub const LOCAL_LAST: usize = 10;
const CACHE_LIMIT: usize = 32;

type Stamp = Vec<Option<(u64, Option<SystemTime>)>>;

/// Where a local agent's login lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountDir<'a> {
    /// The launch-time config dir; `None` is the tool's default.
    Known(Option<&'a str>),
    /// A stopped agent: its launch dir went with its row, and the default
    /// account may not be its own.
    Unknown,
}

struct Request {
    source: LocalSource,
    config_dir: Option<String>,
    tool: Option<Tool>,
    force: bool,
    /// The agent waits on its terminal: read the prompt off its screen.
    prompt_of: Option<String>,
}

struct Reply {
    source: LocalSource,
    turns: Result<Vec<TurnView>, String>,
    email: Option<String>,
    prompt: Option<ScreenPrompt>,
}

struct Entry {
    source: LocalSource,
    turns: Option<Vec<TurnView>>,
    last_success: Option<Instant>,
    failure: Option<String>,
    email: Option<String>,
    prompt: Option<ScreenPrompt>,
}

struct Cached {
    source: LocalSource,
    stamp: Stamp,
    turns: Vec<TurnView>,
}

fn stamp_of(source: &LocalSource) -> Stamp {
    let mut paths = vec![source.path.clone()];
    if source.backend.is_shared_database() {
        let mut wal = source.path.as_os_str().to_os_string();
        wal.push("-wal");
        paths.push(PathBuf::from(wal));
    }
    paths
        .iter()
        .map(|p| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        })
        .collect()
}

fn read_turns(req: &Request, cache: &mut Vec<Cached>) -> Result<Vec<TurnView>, String> {
    let stamp = stamp_of(&req.source);
    if !req.force
        && let Some(hit) = cache
            .iter()
            .find(|c| c.source == req.source && c.stamp == stamp)
    {
        return Ok(hit.turns.clone());
    }
    let turns = if !req.source.path.exists() {
        // Not written yet: an empty conversation, not an error.
        Ok(Vec::new())
    } else {
        let source = req.source.clone();
        std::panic::catch_unwind(move || {
            transcript::read(
                &source.path,
                source.backend,
                &ReadOptions {
                    last: LOCAL_LAST,
                    detailed: false,
                    session_id: source.session.clone(),
                    allow_codex_retry: false,
                },
            )
        })
        .unwrap_or_else(|_| Err("parse panicked".to_string()))
        .map(|t| t.exchanges.iter().map(turn_from_exchange).collect())
    };
    if let Ok(t) = &turns {
        cache.retain(|c| c.source != req.source);
        if cache.len() >= CACHE_LIMIT {
            cache.remove(0);
        }
        cache.push(Cached {
            source: req.source.clone(),
            stamp,
            turns: t.clone(),
        });
    }
    turns.map_err(|e| {
        e.lines()
            .next()
            .unwrap_or("read error")
            .chars()
            .take(40)
            .collect()
    })
}

fn collect(
    req: Request,
    cache: &mut Vec<Cached>,
    emails: &mut EmailCache,
    db: &mut Option<crate::db::HcomDb>,
) -> Reply {
    let turns = read_turns(&req, cache);
    let email = req
        .tool
        .and_then(|tool| emails.email(tool, req.config_dir.as_deref().map(Path::new)));
    // A failed open leaves `db` empty, so the next request tries again.
    let prompt = req.prompt_of.as_deref().and_then(|name| {
        if db.is_none() {
            *db = crate::db::HcomDb::open().ok();
        }
        parse_screen(&crate::commands::term::screen_of(db.as_ref()?, name)?)
    });
    Reply {
        source: req.source,
        turns,
        email,
        prompt,
    }
}

/// Transcripts only grow while the agent works; idle rechecks catch late writes.
fn recheck_interval(status: Option<AgentStatus>) -> Duration {
    match status {
        Some(AgentStatus::Active | AgentStatus::Blocked) => Duration::from_secs(2),
        _ => Duration::from_secs(30),
    }
}

#[derive(Default)]
pub struct LocalTranscriptWorker {
    channels: Option<(Sender<Request>, Receiver<Reply>)>,
    pending: bool,
    checked: Option<Instant>,
    selected: Option<LocalSource>,
    /// Status and its time: a new prompt while still Blocked rechecks too.
    status: Option<(AgentStatus, u64)>,
    force: bool,
    entries: Vec<Entry>,
}

impl LocalTranscriptWorker {
    /// `agent` is the cursor's local agent while the tab is visible, with
    /// the config dir it was launched with.
    pub fn poll(&mut self, agent: Option<(&Agent, AccountDir<'_>)>, now: Instant) -> bool {
        let selected = agent.and_then(|(a, _)| LocalSource::for_agent(a));
        let mut changed = false;
        if self.selected != selected {
            self.selected = selected;
            self.checked = None;
            self.force = false;
            changed = true;
        }
        let status = agent.map(|(a, _)| (a.status, a.status_time.to_bits()));
        if self.status != status {
            self.status = status;
            self.checked = None;
        }

        let mut gone = false;
        if let Some((_, rx)) = &self.channels {
            loop {
                let reply = match rx.try_recv() {
                    Ok(reply) => reply,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                };
                self.pending = false;
                changed |= self.selected.as_ref() == Some(&reply.source);
                let idx = match self.entries.iter().position(|e| e.source == reply.source) {
                    Some(i) => i,
                    None => {
                        if self.entries.len() >= CACHE_LIMIT {
                            self.entries.remove(0);
                        }
                        self.entries.push(Entry {
                            source: reply.source.clone(),
                            turns: None,
                            last_success: None,
                            failure: None,
                            email: None,
                            prompt: None,
                        });
                        self.entries.len() - 1
                    }
                };
                let entry = &mut self.entries[idx];
                match reply.turns {
                    Ok(turns) => {
                        entry.turns = Some(turns);
                        entry.last_success = Some(now);
                        entry.failure = None;
                    }
                    Err(e) => entry.failure = Some(e),
                }
                entry.email = reply.email;
                entry.prompt = reply.prompt;
            }
        }
        if gone {
            // The worker thread died: start a fresh one on the next request.
            self.channels = None;
            self.pending = false;
            changed = true;
        }

        let due = self.force
            || self.checked.is_none_or(|t| {
                now.saturating_duration_since(t) >= recheck_interval(self.status.map(|(s, _)| s))
            });
        if !self.pending
            && due
            && let Some((agent, account)) = agent
            && let Some(source) = self.selected.clone()
        {
            let (tx, _) = self.channels.get_or_insert_with(|| {
                let (tx, requests) = mpsc::channel::<Request>();
                let (results, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut cache = Vec::new();
                    let mut emails = EmailCache::default();
                    let mut db = None;
                    while let Ok(req) = requests.recv() {
                        let source = req.source.clone();
                        let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            collect(req, &mut cache, &mut emails, &mut db)
                        }))
                        .unwrap_or_else(|_| {
                            // State touched mid-panic is not trusted.
                            cache.clear();
                            db = None;
                            Reply {
                                source,
                                turns: Err("read panicked".to_string()),
                                email: None,
                                prompt: None,
                            }
                        });
                        if results.send(reply).is_err() {
                            break;
                        }
                    }
                });
                (tx, rx)
            });
            self.pending = tx
                .send(Request {
                    source,
                    config_dir: match account {
                        AccountDir::Known(dir) => dir.map(String::from),
                        AccountDir::Unknown => None,
                    },
                    // No tool, no e-mail lookup.
                    tool: agent
                        .tool
                        .canonical()
                        .filter(|_| account != AccountDir::Unknown),
                    force: self.force,
                    prompt_of: (agent.status == AgentStatus::Blocked
                        && agent.tool.canonical() == Some(Tool::Claude))
                    .then(|| agent.name.clone()),
                })
                .is_ok();
            self.checked = Some(now);
            self.force = false;
            changed = true;
        }
        changed
    }

    pub fn snapshot(&self, agent: &Agent) -> ConversationSnapshot<'_> {
        let source = LocalSource::for_agent(agent);
        let loading = self.pending && source.is_some() && self.selected == source;
        match self
            .entries
            .iter()
            .find(|e| Some(&e.source) == source.as_ref())
        {
            Some(e) => ConversationSnapshot {
                turns: e.turns.as_deref(),
                last_success: e.last_success,
                failure: e.failure.as_deref(),
                loading,
                email: e.email.as_deref(),
                prompt: e.prompt.as_ref(),
            },
            None => ConversationSnapshot {
                loading,
                ..Default::default()
            },
        }
    }

    /// Re-read now, ignoring the stamp cache.
    pub fn force_refresh(&mut self, agent: &Agent) {
        if LocalSource::for_agent(agent) == self.selected {
            self.force = true;
            self.checked = None;
        }
    }

    pub fn has_pending_timer(&self) -> bool {
        self.pending
    }

    #[cfg(test)]
    pub fn set_prompt_for_test(&mut self, agent: &Agent, prompt: ScreenPrompt) {
        let source = LocalSource::for_agent(agent).expect("agent has a transcript");
        self.entries.retain(|e| e.source != source);
        self.entries.push(Entry {
            source,
            turns: Some(Vec::new()),
            last_success: None,
            failure: None,
            email: None,
            prompt: Some(prompt),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_helpers::make_test_agent;

    fn claude_line(role: &str, text: &str) -> String {
        serde_json::json!({
            "type": role,
            "timestamp": "2026-10-22T07:00:00Z",
            "message": {"role": role, "content": text},
        })
        .to_string()
    }

    fn write_turns(path: &Path, n: usize) {
        let mut out = String::new();
        for i in 1..=n {
            out.push_str(&claude_line("user", &format!("q{i}")));
            out.push('\n');
            out.push_str(&claude_line("assistant", &format!("a{i}")));
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
    }

    fn agent_at(path: &Path) -> Agent {
        let mut a = make_test_agent("luna", 0.0);
        a.transcript_path = Some(path.to_string_lossy().into_owned());
        a
    }

    fn wait(worker: &mut LocalTranscriptWorker, agent: &Agent, now: Instant) {
        for _ in 0..200 {
            worker.poll(Some((agent, AccountDir::Known(None))), now);
            if !worker.pending {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("worker never answered");
    }

    #[test]
    fn reads_last_turns_and_caches_by_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_turns(&path, 12);
        let agent = agent_at(&path);
        let mut w = LocalTranscriptWorker::default();
        let t0 = Instant::now();
        wait(&mut w, &agent, t0);
        let snap = w.snapshot(&agent);
        let turns = snap.turns.unwrap();
        assert_eq!(turns.len(), LOCAL_LAST);
        assert_eq!(turns.last().unwrap().request_summary, "q12");
        assert!(snap.last_success.is_some());

        // Not due yet: no request.
        w.poll(
            Some((&agent, AccountDir::Known(None))),
            t0 + Duration::from_secs(1),
        );
        assert!(!w.pending);
        // Active agent rechecks after 2s.
        write_turns(&path, 13);
        wait(&mut w, &agent, t0 + Duration::from_secs(3));
        assert_eq!(
            w.snapshot(&agent)
                .turns
                .unwrap()
                .last()
                .unwrap()
                .request_summary,
            "q13"
        );
    }

    #[test]
    fn missing_file_is_empty_and_hidden_tab_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_at(&dir.path().join("none.jsonl"));
        let mut w = LocalTranscriptWorker::default();
        assert!(!w.poll(None, Instant::now()));
        assert!(w.channels.is_none());
        assert!(!w.pending);
        wait(&mut w, &agent, Instant::now());
        assert_eq!(w.snapshot(&agent).turns, Some(&[][..]));
    }

    #[test]
    fn status_change_at_the_same_status_rechecks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_turns(&path, 1);
        let mut agent = agent_at(&path);
        agent.status = AgentStatus::Active;
        let mut w = LocalTranscriptWorker::default();
        let t0 = Instant::now();
        wait(&mut w, &agent, t0);
        w.poll(
            Some((&agent, AccountDir::Known(None))),
            t0 + Duration::from_millis(500),
        );
        assert!(!w.pending, "not due yet");
        // Same status, new status time (a second prompt while Blocked).
        agent.status_time += 1.0;
        w.poll(
            Some((&agent, AccountDir::Known(None))),
            t0 + Duration::from_millis(600),
        );
        assert!(w.pending);
    }

    #[test]
    fn unknown_account_dir_reads_no_email() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_turns(&path, 1);
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"luna@example.com"}}"#,
        )
        .unwrap();
        let config_dir = dir.path().to_string_lossy().into_owned();
        let agent = agent_at(&path);
        let mut w = LocalTranscriptWorker::default();
        let mut read = |account: AccountDir<'_>| {
            w.force = true;
            w.poll(Some((&agent, account)), Instant::now());
            for _ in 0..200 {
                if !w.pending {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
                w.poll(Some((&agent, account)), Instant::now());
            }
            w.snapshot(&agent).email.map(String::from)
        };
        assert_eq!(
            read(AccountDir::Known(Some(&config_dir))).as_deref(),
            Some("luna@example.com")
        );
        assert_eq!(read(AccountDir::Unknown), None);
    }

    #[test]
    fn dead_worker_is_dropped() {
        let (tx, _requests) = mpsc::channel();
        let (results, rx) = mpsc::channel::<Reply>();
        drop(results);
        let mut w = LocalTranscriptWorker {
            channels: Some((tx, rx)),
            pending: true,
            ..Default::default()
        };
        assert!(w.poll(None, Instant::now()));
        assert!(w.channels.is_none());
        assert!(!w.pending);
        assert!(!w.has_pending_timer());
    }

    #[test]
    fn failure_keeps_previous_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_turns(&path, 2);
        let agent = agent_at(&path);
        let mut w = LocalTranscriptWorker::default();
        let t0 = Instant::now();
        wait(&mut w, &agent, t0);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        w.force_refresh(&agent);
        wait(&mut w, &agent, t0);
        let snap = w.snapshot(&agent);
        assert_eq!(snap.turns.unwrap().len(), 2);
        assert!(snap.failure.is_some());
    }
}
