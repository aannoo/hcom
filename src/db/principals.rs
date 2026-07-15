//! Durable principal binding primitives.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use super::HcomDb;
use crate::shared::time::now_epoch_f64;

/// Exact principal lookup result. `Unresolved` retains only the recorded target;
/// it never guesses another instance from name or recency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalLookup {
    Resolved {
        instance_name: String,
        session_id: Option<String>,
    },
    Unresolved {
        instance_name: String,
    },
    /// One or more instance rows claim the principal, but the authoritative
    /// binding row is absent. Claims are diagnostic only and never routable.
    MissingBinding {
        claiming_instances: Vec<String>,
    },
    Unknown,
}

impl PrincipalLookup {
    pub fn instance_name(&self) -> Option<&str> {
        match self {
            Self::Resolved { instance_name, .. } | Self::Unresolved { instance_name } => {
                Some(instance_name)
            }
            Self::MissingBinding { .. } => None,
            Self::Unknown => None,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    pub fn is_unresolved(&self) -> bool {
        matches!(self, Self::Unresolved { .. } | Self::MissingBinding { .. })
    }
}

impl HcomDb {
    /// Atomically attach one immutable principal to one existing instance.
    /// Replaying the same pair is a no-op; every attempted reassignment fails.
    pub fn create_principal_binding(&self, principal: &str, instance_name: &str) -> Result<bool> {
        if principal.is_empty() || instance_name.is_empty() {
            bail!("principal and instance_name must not be empty");
        }

        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<bool> {
            let binding: Option<String> = self
                .conn
                .query_row(
                    "SELECT instance_name FROM principal_bindings WHERE principal=?",
                    params![principal],
                    |row| row.get(0),
                )
                .optional()?;
            let column: Option<Option<String>> = self
                .conn
                .query_row(
                    "SELECT principal FROM instances WHERE name=?",
                    params![instance_name],
                    |row| row.get(0),
                )
                .optional()?;

            let Some(column) = column else {
                bail!("cannot bind principal to missing instance '{instance_name}'");
            };
            let foreign_claim: Option<String> = self
                .conn
                .query_row(
                    "SELECT name FROM instances WHERE principal=? AND name!=? LIMIT 1",
                    params![principal, instance_name],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(claiming_instance) = foreign_claim {
                bail!(
                    "principal '{principal}' is already claimed by instance '{claiming_instance}' without a consistent binding"
                );
            }
            if let Some(bound) = binding {
                if bound == instance_name && column.as_deref() == Some(principal) {
                    return Ok(false);
                }
                bail!("principal '{principal}' is already bound to '{bound}'");
            }
            if let Some(existing) = column.filter(|value| !value.is_empty()) {
                bail!("instance '{instance_name}' already carries principal '{existing}'");
            }

            let now = now_epoch_f64();
            self.conn.execute(
                "INSERT INTO principal_bindings (principal, instance_name, epoch, created_at, updated_at)
                 VALUES (?, ?, 0, ?, ?)",
                params![principal, instance_name, now, now],
            )?;
            let updated = self.conn.execute(
                "UPDATE instances SET principal=? WHERE name=? AND principal IS NULL",
                params![principal, instance_name],
            )?;
            if updated != 1 {
                bail!("instance '{instance_name}' changed while assigning principal");
            }
            Ok(true)
        })();

        match result {
            Ok(created) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(created)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn principal_for_instance(&self, instance_name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT principal FROM instances WHERE name=?",
                params![instance_name],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .filter(|value| !value.is_empty()))
    }

    /// Resolve the principal eligible for a tracked `hcom resume <name>`.
    ///
    /// A current instance row is authoritative only when its principal column
    /// and durable binding agree. After the row has been removed, only the
    /// uniquely newest binding for the name is eligible. Ambiguous or damaged
    /// state fails closed instead of guessing an older lifecycle.
    pub(crate) fn principal_for_tracked_resume(&self, instance_name: &str) -> Result<String> {
        let current: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT principal FROM instances WHERE name=?",
                params![instance_name],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(column) = current {
            let principal = column.filter(|value| !value.is_empty()).ok_or_else(|| {
                anyhow::anyhow!(
                    "tracked resume identity for '{instance_name}' is missing its principal"
                )
            })?;
            let bound_name: Option<String> = self
                .conn
                .query_row(
                    "SELECT instance_name FROM principal_bindings WHERE principal=?",
                    params![principal],
                    |row| row.get(0),
                )
                .optional()?;
            if bound_name.as_deref() != Some(instance_name) {
                bail!(
                    "tracked resume identity for '{instance_name}' has an inconsistent principal binding"
                );
            }
            let foreign_claim: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM instances WHERE principal=? AND name!=?)",
                params![principal, instance_name],
                |row| row.get(0),
            )?;
            if foreign_claim {
                bail!(
                    "tracked resume identity for '{instance_name}' is claimed by another instance"
                );
            }
            return Ok(principal);
        }

        let mut stmt = self.conn.prepare(
            "SELECT principal, created_at FROM principal_bindings
             WHERE instance_name=?
             ORDER BY created_at DESC, principal ASC LIMIT 2",
        )?;
        let rows: Vec<(String, f64)> = stmt
            .query_map(params![instance_name], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let Some((principal, newest_at)) = rows.first() else {
            bail!("tracked resume identity for '{instance_name}' has no durable binding");
        };
        if principal.is_empty() {
            bail!("tracked resume identity for '{instance_name}' has an empty principal");
        }
        if rows.get(1).is_some_and(|(_, next_at)| next_at == newest_at) {
            bail!(
                "tracked resume identity for '{instance_name}' is ambiguous: newest bindings are tied"
            );
        }
        let lifecycle: Option<(Option<String>, Option<f64>, Option<f64>)> = self
            .conn
            .query_row(
                "SELECT json_extract(data, '$.snapshot.principal'),
                        json_extract(data, '$.snapshot.created_at'),
                        (julianday(timestamp) - 2440587.5) * 86400.0
                 FROM events
                 WHERE type='life'
                   AND instance=?
                   AND json_extract(data, '$.action')='stopped'
                 ORDER BY id DESC LIMIT 1",
                params![instance_name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((snapshot_principal, Some(lifecycle_created_at), Some(stopped_at))) = lifecycle
        else {
            bail!(
                "tracked resume identity for '{instance_name}' has no complete stopped lifecycle evidence"
            );
        };
        if let Some(snapshot_principal) = snapshot_principal.filter(|value| !value.is_empty()) {
            if snapshot_principal != *principal {
                bail!(
                    "tracked resume identity for '{instance_name}' does not match its latest stopped lifecycle"
                );
            }
        } else {
            // Pre-P2 stopped snapshots do not carry the principal. Bound the
            // newest binding to that lifecycle's creation/stop interval so a
            // later crashed reclaim cannot be paired with an older transcript.
            // SQLite's julianday conversion is millisecond-granular on some
            // versions, hence the representation-only tolerance.
            const TIMESTAMP_EPSILON_SECS: f64 = 0.001;
            if *newest_at + TIMESTAMP_EPSILON_SECS < lifecycle_created_at
                || *newest_at > stopped_at + TIMESTAMP_EPSILON_SECS
            {
                bail!(
                    "tracked resume identity for '{instance_name}' does not belong to its latest stopped lifecycle"
                );
            }
        }
        if stopped_at + 0.001 < lifecycle_created_at {
            bail!(
                "tracked resume identity for '{instance_name}' has invalid stopped lifecycle timing"
            );
        }
        let foreign_claim: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM instances WHERE principal=? AND name!=?)",
            params![principal, instance_name],
            |row| row.get(0),
        )?;
        if foreign_claim {
            bail!("tracked resume identity for '{instance_name}' is claimed by another instance");
        }
        Ok(principal.clone())
    }

    /// Atomically recheck and reserve a tracked resume lifecycle.
    ///
    /// The pending row prevents a concurrent name reclaim after the eligibility
    /// check. The durable binding is reused, not rewritten, so a later launch
    /// failure can remove only the reservation while keeping resume evidence.
    pub(crate) fn reserve_tracked_resume_principal(
        &self,
        instance_name: &str,
        expected_principal: &str,
        reservation_id: &str,
    ) -> Result<()> {
        if reservation_id.is_empty() {
            bail!("tracked resume reservation id must not be empty");
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let current = self.principal_for_tracked_resume(instance_name)?;
            if current != expected_principal {
                bail!("tracked resume identity for '{instance_name}' changed before launch");
            }

            let status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM instances WHERE name=?",
                    params![instance_name],
                    |row| row.get(0),
                )
                .optional()?;
            if status.as_deref().is_some_and(|value| value != "inactive") {
                bail!("tracked resume identity for '{instance_name}' changed before launch");
            }

            self.conn
                .execute("DELETE FROM instances WHERE name=?", params![instance_name])?;
            self.conn.execute(
                "INSERT INTO instances
                 (name, principal, status, status_context, created_at, launch_context)
                 VALUES (?, ?, 'pending', 'new', ?, ?)",
                params![
                    instance_name,
                    expected_principal,
                    now_epoch_f64(),
                    serde_json::json!({"resume_reservation_id": reservation_id}).to_string()
                ],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Claim exactly the tracked-resume reservation created for this process.
    ///
    /// Rechecking the row, durable binding, and reservation token in one write
    /// transaction prevents an older launch attempt from attaching to a newer
    /// retry that retained the same principal.
    pub(crate) fn claim_tracked_resume_principal(
        &self,
        instance_name: &str,
        expected_principal: &str,
        reservation_id: &str,
    ) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let reserved: Option<(Option<String>, String, Option<String>)> = self
                .conn
                .query_row(
                    "SELECT principal, status,
                            CASE WHEN json_valid(launch_context)
                                 THEN json_extract(launch_context, '$.resume_reservation_id')
                                 ELSE NULL END
                     FROM instances WHERE name=?",
                    params![instance_name],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((principal, status, token)) = reserved else {
                bail!("tracked resume reservation for '{instance_name}' disappeared before launch");
            };
            if principal.as_deref() != Some(expected_principal)
                || status != "pending"
                || token.as_deref() != Some(reservation_id)
            {
                bail!("tracked resume reservation for '{instance_name}' changed before launch");
            }

            let bound_name: Option<String> = self
                .conn
                .query_row(
                    "SELECT instance_name FROM principal_bindings WHERE principal=?",
                    params![expected_principal],
                    |row| row.get(0),
                )
                .optional()?;
            if bound_name.as_deref() != Some(instance_name) {
                bail!(
                    "tracked resume identity for '{instance_name}' has an inconsistent principal binding"
                );
            }
            let foreign_claim: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM instances WHERE principal=? AND name!=?)",
                params![expected_principal, instance_name],
                |row| row.get(0),
            )?;
            if foreign_claim {
                bail!(
                    "tracked resume identity for '{instance_name}' is claimed by another instance"
                );
            }

            self.set_process_binding(reservation_id, "", instance_name)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Remove only the reservation owned by this failed tracked-resume launch.
    pub(crate) fn cleanup_tracked_resume_reservation(
        &self,
        instance_name: &str,
        expected_principal: &str,
        reservation_id: &str,
    ) -> Result<bool> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<bool> {
            let deleted = self.conn.execute(
                "DELETE FROM instances
                 WHERE name=? AND principal=?
                   AND CASE WHEN json_valid(launch_context)
                            THEN json_extract(launch_context, '$.resume_reservation_id')
                            ELSE NULL END = ?",
                params![instance_name, expected_principal, reservation_id],
            )?;
            self.conn.execute(
                "DELETE FROM process_bindings WHERE process_id=?",
                params![reservation_id],
            )?;
            Ok(deleted == 1)
        })();

        match result {
            Ok(deleted) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Roll back an identity minted for a launch that never became live.
    /// Normal stop paths deliberately do not call this: once a launch succeeds,
    /// its durable binding survives instance-row deletion for exact lookup.
    pub(crate) fn rollback_provisional_principal_binding(
        &self,
        principal: &str,
        instance_name: &str,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let deleted = tx.execute(
            "DELETE FROM principal_bindings WHERE principal=? AND instance_name=?",
            params![principal, instance_name],
        )?;
        let cleared = tx.execute(
            "UPDATE instances SET principal=NULL WHERE name=? AND principal=?",
            params![instance_name, principal],
        )?;
        tx.commit()?;
        Ok(deleted > 0 || cleared > 0)
    }

    /// Resolve only the stored principal row and its exact claimed instance.
    pub fn lookup_principal(&self, principal: &str) -> Result<PrincipalLookup> {
        let row: Option<(String, Option<String>, Option<String>, bool)> = self
            .conn
            .query_row(
                "SELECT b.instance_name, i.principal, i.session_id,
                        EXISTS(SELECT 1 FROM instances other
                               WHERE other.principal=b.principal AND other.name!=b.instance_name)
                 FROM principal_bindings b LEFT JOIN instances i ON i.name=b.instance_name
                 WHERE b.principal=?",
                params![principal],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if row.is_none() {
            let claiming_instances: Vec<String> = self
                .conn
                .prepare("SELECT name FROM instances WHERE principal=? ORDER BY name")?
                .query_map(params![principal], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            return Ok(if claiming_instances.is_empty() {
                PrincipalLookup::Unknown
            } else {
                PrincipalLookup::MissingBinding { claiming_instances }
            });
        }
        Ok(match row {
            Some((instance_name, Some(column), session_id, false)) if column == principal => {
                PrincipalLookup::Resolved {
                    instance_name,
                    session_id: session_id.filter(|value| !value.is_empty()),
                }
            }
            Some((instance_name, _, _, _)) => PrincipalLookup::Unresolved { instance_name },
            None => unreachable!("missing binding handled above"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{cleanup_test_db, setup_full_test_db};
    use rusqlite::params;

    fn insert_instance(db: &super::super::HcomDb, name: &str) {
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES (?, 1.0)",
                params![name],
            )
            .unwrap();
    }

    fn insert_stopped_lifecycle(
        db: &super::super::HcomDb,
        name: &str,
        created_at: f64,
        stopped_at: &str,
    ) {
        let data = serde_json::json!({
            "action": "stopped",
            "snapshot": {"created_at": created_at}
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data)
                 VALUES (?, 'life', ?, ?)",
                params![stopped_at, name, data.to_string()],
            )
            .unwrap();
    }

    #[test]
    fn create_binding_is_atomic_idempotent_and_conflict_safe() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        insert_instance(&db, "nova");

        assert!(db.create_principal_binding("p-1", "luna").unwrap());
        assert!(!db.create_principal_binding("p-1", "luna").unwrap());
        assert!(db.create_principal_binding("p-1", "nova").is_err());
        assert!(db.create_principal_binding("p-2", "luna").is_err());

        let row: (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT principal, instance_name, epoch FROM principal_bindings",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("p-1".into(), "luna".into(), 0));
        let column: Option<String> = db
            .conn()
            .query_row(
                "SELECT principal FROM instances WHERE name='luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(column.as_deref(), Some("p-1"));

        cleanup_test_db(path);
    }

    #[test]
    fn create_binding_rolls_back_when_second_write_fails() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_principal BEFORE UPDATE OF principal ON instances
                 BEGIN SELECT RAISE(ABORT, 'reject principal'); END;",
            )
            .unwrap();

        assert!(db.create_principal_binding("p-1", "luna").is_err());
        let bindings: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM principal_bindings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(bindings, 0);

        cleanup_test_db(path);
    }

    #[test]
    fn principal_lookup_is_exact_and_reports_unresolved_state() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.create_principal_binding("p-1", "luna").unwrap();

        let found = db.lookup_principal("p-1").unwrap();
        assert_eq!(found.instance_name(), Some("luna"));
        assert!(db.lookup_principal("p-missing").unwrap().is_unknown());

        db.conn()
            .execute("DELETE FROM instances WHERE name='luna'", [])
            .unwrap();
        let unresolved = db.lookup_principal("p-1").unwrap();
        assert_eq!(unresolved.instance_name(), Some("luna"));
        assert!(unresolved.is_unresolved());

        cleanup_test_db(path);
    }

    #[test]
    fn principal_lookup_reports_missing_binding_as_unresolved_without_guessing() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.create_principal_binding("p-1", "luna").unwrap();
        db.conn()
            .execute("DELETE FROM principal_bindings WHERE principal='p-1'", [])
            .unwrap();

        let missing_binding = db.lookup_principal("p-1").unwrap();
        assert!(missing_binding.is_unresolved());
        assert_eq!(
            missing_binding.instance_name(),
            None,
            "an instance-side claim is diagnostic evidence, not a routing target"
        );

        cleanup_test_db(path);
    }

    #[test]
    fn create_binding_rejects_foreign_instance_claim_and_preserves_state() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        insert_instance(&db, "nova");
        db.conn()
            .execute("UPDATE instances SET principal='p-1' WHERE name='luna'", [])
            .unwrap();

        assert!(db.create_principal_binding("p-1", "nova").is_err());
        let bindings: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM principal_bindings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(bindings, 0);
        assert_eq!(
            db.principal_for_instance("luna").unwrap().as_deref(),
            Some("p-1")
        );
        assert_eq!(db.principal_for_instance("nova").unwrap(), None);

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_uses_consistent_live_principal() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.create_principal_binding("p-live", "luna").unwrap();

        assert_eq!(db.principal_for_tracked_resume("luna").unwrap(), "p-live");

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_uses_only_uniquely_newest_stopped_binding() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.create_principal_binding("p-old", "luna").unwrap();
        db.delete_instance("luna").unwrap();
        db.conn()
            .execute(
                "INSERT INTO principal_bindings
                 (principal, instance_name, epoch, created_at, updated_at)
                 VALUES ('p-new', 'luna', 0, 2.0, 2.0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE principal_bindings SET created_at=1.0, updated_at=1.0
                 WHERE principal='p-old'",
                [],
            )
            .unwrap();
        insert_stopped_lifecycle(&db, "luna", 1.5, "1970-01-01T00:00:04Z");

        assert_eq!(db.principal_for_tracked_resume("luna").unwrap(), "p-new");

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_rejects_missing_corrupt_and_tied_identity() {
        let (db, path) = setup_full_test_db();
        assert!(db.principal_for_tracked_resume("missing").is_err());

        insert_instance(&db, "luna");
        db.conn()
            .execute(
                "UPDATE instances SET principal='p-without-binding' WHERE name='luna'",
                [],
            )
            .unwrap();
        assert!(db.principal_for_tracked_resume("luna").is_err());

        db.delete_instance("luna").unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO principal_bindings
                 (principal, instance_name, epoch, created_at, updated_at)
                 VALUES ('p-a', 'luna', 0, 3.0, 3.0),
                        ('p-b', 'luna', 0, 3.0, 3.0);",
            )
            .unwrap();
        assert!(db.principal_for_tracked_resume("luna").is_err());

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_reservation_rechecks_reclaim_and_keeps_binding() {
        let (db, path) = setup_full_test_db();
        insert_instance(&db, "luna");
        db.create_principal_binding("p-old", "luna").unwrap();
        db.delete_instance("luna").unwrap();

        insert_instance(&db, "luna");
        db.create_principal_binding("p-reclaimed", "luna").unwrap();

        let error = db
            .reserve_tracked_resume_principal("luna", "p-old", "proc-stale")
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed"), "unexpected error: {error}");
        assert_eq!(
            db.principal_for_instance("luna").unwrap().as_deref(),
            Some("p-reclaimed")
        );
        assert_eq!(
            db.lookup_principal("p-old").unwrap().instance_name(),
            Some("luna")
        );

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_never_pairs_old_snapshot_with_newer_unstopped_binding() {
        let (db, path) = setup_full_test_db();
        db.conn()
            .execute_batch(
                "INSERT INTO principal_bindings
                 (principal, instance_name, epoch, created_at, updated_at)
                 VALUES ('p-old', 'luna', 0, 1.0, 1.0),
                        ('p-crashed-reclaim', 'luna', 0, 3.0, 3.0);",
            )
            .unwrap();
        insert_stopped_lifecycle(&db, "luna", 0.5, "1970-01-01T00:00:02Z");

        let error = db
            .principal_for_tracked_resume("luna")
            .unwrap_err()
            .to_string();
        assert!(error.contains("lifecycle"), "unexpected error: {error}");

        cleanup_test_db(path);
    }

    #[test]
    fn tracked_resume_after_stopped_reclaim_keeps_reclaimed_principal() {
        let (db, path) = setup_full_test_db();
        db.conn()
            .execute_batch(
                "INSERT INTO principal_bindings
                 (principal, instance_name, epoch, created_at, updated_at)
                 VALUES ('p-old', 'luna', 0, 1.0, 1.0),
                        ('p-reclaimed', 'luna', 0, 3.0, 3.0);",
            )
            .unwrap();
        insert_stopped_lifecycle(&db, "luna", 2.5, "1970-01-01T00:00:04Z");

        assert_eq!(
            db.principal_for_tracked_resume("luna").unwrap(),
            "p-reclaimed"
        );

        cleanup_test_db(path);
    }
}
