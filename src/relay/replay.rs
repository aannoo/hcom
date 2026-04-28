//! Replay protection for relay envelopes.
//!
//! Two cheap layers stacked on top of the AEAD:
//!
//! 1. **Clock skew window** — drop envelopes whose `ts_secs` deviates from
//!    local time by more than `MAX_SKEW_SECS`. The timestamp is bound into the
//!    AAD, so an attacker cannot forge a fresher one without invalidating the
//!    AEAD tag.
//! 2. **Nonce LRU** — bounded set of `(device_short, nonce)` pairs. Drops exact
//!    nonce replays inside the window. Capped so a chatty fleet cannot exhaust
//!    memory; oldest entries are evicted first.
//!
//! The two layers together close the "replay later" and "replay-now-with-forged-
//! timestamp" cases. The id-based dedup in `pull.rs` still runs for in-session
//! ordering and remote-DB regression detection.

use std::num::NonZeroUsize;

use crate::relay::crypto::NONCE_LEN;
use lru::LruCache;

pub const MAX_SKEW_SECS: i64 = 60;
pub const NONCE_TTL_SECS: u64 = 600;
pub const NONCE_LRU_CAP: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// Envelope timestamp is too far from `now`.
    ClockSkew { delta_secs: i64 },
    /// Retained state envelope predates the newest state we've already accepted.
    OlderThanWatermark { ts_secs: u64, min_secs: u64 },
    /// Nonce already seen for this sender within the window.
    DuplicateNonce,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClockSkew { delta_secs } => write!(f, "clock skew {}s", delta_secs),
            Self::OlderThanWatermark { ts_secs, min_secs } => {
                write!(
                    f,
                    "retained rollback ts={} < watermark={}",
                    ts_secs, min_secs
                )
            }
            Self::DuplicateNonce => write!(f, "duplicate nonce"),
        }
    }
}

impl std::error::Error for ReplayError {}

type NonceKey = (String, [u8; NONCE_LEN]);

/// Bounded LRU of `(sender, nonce) -> first-seen-secs`. Old entries are dropped
/// lazily during checks/inserts once their TTL expires.
pub struct ReplayGuard {
    seen: LruCache<NonceKey, u64>,
    ttl_secs: u64,
    skew_secs: i64,
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new(NONCE_LRU_CAP, NONCE_TTL_SECS, MAX_SKEW_SECS)
    }
}

impl ReplayGuard {
    pub fn new(cap: usize, ttl_secs: u64, skew_secs: i64) -> Self {
        Self {
            seen: LruCache::new(NonZeroUsize::new(cap.max(1)).unwrap()),
            ttl_secs,
            skew_secs,
        }
    }

    /// Validate envelope timestamp and reject duplicates already present in the
    /// replay window. Does not record the nonce; callers should only do that
    /// after the envelope has successfully authenticated.
    pub fn check(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        ts_secs: u64,
        now_secs: u64,
    ) -> Result<(), ReplayError> {
        self.check_inner(sender, nonce, ts_secs, now_secs, true)
    }

    /// Retained MQTT state snapshots may legitimately be older than the live
    /// skew window, but they must never roll back behind the newest state
    /// already accepted for that sender.
    pub fn check_retained(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        ts_secs: u64,
        now_secs: u64,
        min_accepted_ts: Option<u64>,
    ) -> Result<(), ReplayError> {
        if let Some(min_secs) = min_accepted_ts {
            // Strict `<` preserves reconnect behavior: if the broker re-delivers
            // the exact retained snapshot we already accepted, equal timestamps
            // must still pass instead of looking like a rollback.
            if ts_secs < min_secs {
                return Err(ReplayError::OlderThanWatermark { ts_secs, min_secs });
            }
        }
        self.check_inner(sender, nonce, ts_secs, now_secs, false)
    }

    /// Record a nonce after the envelope has authenticated successfully.
    pub fn record_nonce(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        now_secs: u64,
    ) -> Result<(), ReplayError> {
        self.evict_expired(now_secs);

        let key = (sender.to_string(), nonce);
        if self.seen.peek(&key).is_some() {
            return Err(ReplayError::DuplicateNonce);
        }

        self.seen.put(key, now_secs);
        Ok(())
    }

    /// Backwards-compatible helper for tests/callers that want the old
    /// check-and-insert behavior in one call.
    pub fn check_and_record(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        ts_secs: u64,
        now_secs: u64,
    ) -> Result<(), ReplayError> {
        self.check(sender, nonce, ts_secs, now_secs)?;
        self.record_nonce(sender, nonce, now_secs)
    }

    /// Backwards-compatible helper for retained snapshots.
    pub fn check_and_record_retained(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        ts_secs: u64,
        now_secs: u64,
        min_accepted_ts: Option<u64>,
    ) -> Result<(), ReplayError> {
        self.check_retained(sender, nonce, ts_secs, now_secs, min_accepted_ts)?;
        self.record_nonce(sender, nonce, now_secs)
    }

    fn check_inner(
        &mut self,
        sender: &str,
        nonce: [u8; NONCE_LEN],
        ts_secs: u64,
        now_secs: u64,
        enforce_skew: bool,
    ) -> Result<(), ReplayError> {
        if enforce_skew {
            let delta = now_secs as i64 - ts_secs as i64;
            if delta.abs() > self.skew_secs {
                return Err(ReplayError::ClockSkew { delta_secs: delta });
            }
        }

        self.evict_expired(now_secs);
        let key = (sender.to_string(), nonce);
        if self.seen.peek(&key).is_some() {
            return Err(ReplayError::DuplicateNonce);
        }
        Ok(())
    }

    fn evict_expired(&mut self, now_secs: u64) {
        let cutoff = now_secs.saturating_sub(self.ttl_secs);
        while self.seen.peek_lru().is_some_and(|(_, ts)| *ts < cutoff) {
            self.seen.pop_lru();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce(byte: u8) -> [u8; NONCE_LEN] {
        [byte; NONCE_LEN]
    }

    #[test]
    fn fresh_envelope_accepted() {
        let mut g = ReplayGuard::default();
        assert!(g.check_and_record("a", nonce(1), 1000, 1000).is_ok());
    }

    #[test]
    fn skew_in_past_rejected() {
        let mut g = ReplayGuard::default();
        let err = g
            .check_and_record("a", nonce(1), 1000, 1000 + (MAX_SKEW_SECS as u64) + 1)
            .unwrap_err();
        assert!(matches!(err, ReplayError::ClockSkew { .. }));
    }

    #[test]
    fn skew_in_future_rejected() {
        let mut g = ReplayGuard::default();
        let err = g
            .check_and_record("a", nonce(1), 1000 + (MAX_SKEW_SECS as u64) + 1, 1000)
            .unwrap_err();
        assert!(matches!(err, ReplayError::ClockSkew { .. }));
    }

    #[test]
    fn duplicate_nonce_rejected() {
        let mut g = ReplayGuard::default();
        g.check_and_record("a", nonce(7), 1000, 1000).unwrap();
        assert_eq!(
            g.check_and_record("a", nonce(7), 1000, 1000),
            Err(ReplayError::DuplicateNonce)
        );
    }

    #[test]
    fn same_nonce_different_sender_allowed() {
        let mut g = ReplayGuard::default();
        g.check_and_record("a", nonce(7), 1000, 1000).unwrap();
        g.check_and_record("b", nonce(7), 1000, 1000).unwrap();
    }

    #[test]
    fn ttl_eviction_allows_reuse_later() {
        let mut g = ReplayGuard::new(8, 60, MAX_SKEW_SECS);
        g.check_and_record("a", nonce(7), 1000, 1000).unwrap();
        // Advance well past TTL but stay within skew window of new ts.
        let later = 1000 + 200;
        let dup = g.check_and_record("a", nonce(7), later, later);
        // Within new freshness window the previously-recorded entry was already
        // evicted by ttl, so the same nonce is accepted again.
        assert!(dup.is_ok());
    }

    #[test]
    fn cap_evicts_oldest() {
        let mut g = ReplayGuard::new(2, 600, MAX_SKEW_SECS);
        g.check_and_record("a", nonce(1), 1000, 1000).unwrap();
        g.check_and_record("a", nonce(2), 1001, 1001).unwrap();
        g.check_and_record("a", nonce(3), 1002, 1002).unwrap();
        assert_eq!(g.len(), 2);
        // The oldest (nonce 1) should have been evicted, so re-inserting it now
        // succeeds within the same freshness window.
        assert!(g.check_and_record("a", nonce(1), 1003, 1003).is_ok());
    }

    #[test]
    fn retained_accepts_old_snapshot_without_watermark() {
        let mut g = ReplayGuard::default();
        let stale_now = 1000 + (MAX_SKEW_SECS as u64) + 120;
        assert!(
            g.check_and_record_retained("a", nonce(9), 1000, stale_now, None)
                .is_ok()
        );
    }

    #[test]
    fn retained_rejects_rollback_behind_watermark() {
        let mut g = ReplayGuard::default();
        let stale_now = 2000;
        let err = g
            .check_and_record_retained("a", nonce(9), 1000, stale_now, Some(1500))
            .unwrap_err();
        assert!(matches!(err, ReplayError::OlderThanWatermark { .. }));
    }

    #[test]
    fn failed_auth_probe_does_not_consume_slot() {
        let mut g = ReplayGuard::new(1, 600, MAX_SKEW_SECS);
        let n1 = nonce(1);
        let n2 = nonce(2);

        g.check("a", n1, 1000, 1000).unwrap();
        assert_eq!(g.len(), 0);

        g.check_and_record("a", n2, 1001, 1001).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g.check_and_record("a", n1, 1002, 1002).is_ok());
    }
}
