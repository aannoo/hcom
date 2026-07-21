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
}
