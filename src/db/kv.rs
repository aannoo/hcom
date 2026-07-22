//! Key-value table helpers.

use anyhow::Result;
use rusqlite::params;

use super::HcomDb;

fn prefix_like_pattern(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

impl HcomDb {
    /// Get value from kv table.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        match self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?", params![key], |row| {
                row.get::<_, Option<String>>(0)
            }) {
            Ok(val) => Ok(val),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set or delete value in kv table. Pass None to delete.
    pub fn kv_set(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            None => {
                self.conn
                    .execute("DELETE FROM kv WHERE key = ?", params![key])?;
            }
            Some(v) => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
                    params![key, v],
                )?;
            }
        }
        Ok(())
    }

    /// Get all kv entries whose key starts with prefix. Returns Vec<(key, value)>.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let pattern = prefix_like_pattern(prefix);
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, value FROM kv WHERE key LIKE ? ESCAPE '\\'")?;
        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Delete all kv entries whose key starts with prefix. Returns count deleted.
    pub fn kv_delete_prefix(&self, prefix: &str) -> Result<usize> {
        let pattern = prefix_like_pattern(prefix);
        let n = self.conn.execute(
            "DELETE FROM kv WHERE key LIKE ? ESCAPE '\\'",
            params![pattern],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{cleanup_test_db, setup_full_test_db};

    #[test]
    fn test_kv_get_set() {
        let (db, db_path) = setup_full_test_db();

        // Get non-existent key
        assert!(db.kv_get("foo").unwrap().is_none());

        // Set and get
        db.kv_set("foo", Some("bar")).unwrap();
        assert_eq!(db.kv_get("foo").unwrap(), Some("bar".to_string()));

        // Overwrite
        db.kv_set("foo", Some("baz")).unwrap();
        assert_eq!(db.kv_get("foo").unwrap(), Some("baz".to_string()));

        // Delete
        db.kv_set("foo", None).unwrap();
        assert!(db.kv_get("foo").unwrap().is_none());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_kv_prefix() {
        let (db, db_path) = setup_full_test_db();

        db.kv_set("events_sub:1", Some("val1")).unwrap();
        db.kv_set("events_sub:2", Some("val2")).unwrap();
        db.kv_set("other:1", Some("val3")).unwrap();

        let results = db.kv_prefix("events_sub:").unwrap();
        assert_eq!(results.len(), 2);

        // Wildcards in prefix should be escaped — not treated as LIKE patterns
        db.kv_set("100%_done", Some("yes")).unwrap();
        db.kv_set("100x_done", Some("no")).unwrap();
        let results = db.kv_prefix("100%").unwrap();
        assert_eq!(results.len(), 1, "% in prefix must be escaped");
        assert_eq!(results[0].0, "100%_done");

        let results = db.kv_prefix("events_sub").unwrap();
        // underscore in "events_sub" should match literally, not as single-char wildcard
        assert_eq!(results.len(), 2, "_ in prefix must be escaped");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_kv_delete_prefix() {
        let (db, db_path) = setup_full_test_db();

        db.kv_set("claim:100%_done:1", Some("one")).unwrap();
        db.kv_set("claim:100%_done:2", Some("two")).unwrap();
        db.kv_set("claim:100x_done:1", Some("other")).unwrap();

        assert_eq!(db.kv_delete_prefix("claim:100%_done:").unwrap(), 2);
        assert!(db.kv_get("claim:100%_done:1").unwrap().is_none());
        assert!(db.kv_get("claim:100%_done:2").unwrap().is_none());
        assert_eq!(
            db.kv_get("claim:100x_done:1").unwrap(),
            Some("other".to_string())
        );

        cleanup_test_db(db_path);
    }
}
