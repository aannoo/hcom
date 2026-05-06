//! Wake notification — TCP connect-and-close pings that unblock poll loops
//! in target processes.
//!
//! - [`server::NotifyServer`] — receive side. Polls a localhost listener for
//!   wake connections and reports back to the delivery / listen / events_wait
//!   loops that own it.
//! - [`wake`] — send side. Looks up registered ports in `notify_endpoints`
//!   and connect-drops to wake them.
//! - [`WakeKind`] — typed enum for the kinds of wake endpoint. Excludes
//!   `inject`, which lives in the same DB table but speaks a bidirectional
//!   protocol (see `commands::term`).

pub mod server;
pub mod wake;

pub use server::NotifyServer;
pub use wake::{WAKE_TARGETED_MS, snapshot_wake_ports, wake, wake_all, wake_ports};

/// Kinds of wake endpoint stored in the `notify_endpoints` table.
///
/// Each kind corresponds to a poll loop in some process; `wake_*` opens a
/// short-lived TCP connection to its registered port to fire the wake.
///
/// `inject` is intentionally absent — it shares the `notify_endpoints` table
/// for `(instance, port)` lookup but uses a request/response protocol, not
/// connect-drop wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeKind {
    /// PTY-managed delivery thread (`crate::delivery`).
    Pty,
    /// Post-tool hook polling loop (`crate::hooks`).
    Hook,
    /// `hcom listen` blocking poll.
    Listen,
    /// `hcom listen --filter` blocking poll.
    ListenFilter,
    /// `hcom events --wait` blocking poll.
    EventsWait,
    /// OpenCode plugin runtime.
    Plugin,
}

impl WakeKind {
    /// All wake kinds — used for "wake everything for this instance" and for
    /// the kind filter in `wake_all`.
    pub const ALL: &'static [WakeKind] = &[
        WakeKind::Pty,
        WakeKind::Hook,
        WakeKind::Listen,
        WakeKind::ListenFilter,
        WakeKind::EventsWait,
        WakeKind::Plugin,
    ];

    /// Kinds woken when an instance's status changes — the delivery-loop
    /// listeners (PTY thread + the two `hcom listen` variants).
    pub const DELIVERY_LOOPS: &'static [WakeKind] =
        &[WakeKind::Pty, WakeKind::Listen, WakeKind::ListenFilter];

    /// String form stored in the `notify_endpoints.kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            WakeKind::Pty => "pty",
            WakeKind::Hook => "hook",
            WakeKind::Listen => "listen",
            WakeKind::ListenFilter => "listen_filter",
            WakeKind::EventsWait => "events_wait",
            WakeKind::Plugin => "plugin",
        }
    }
}
