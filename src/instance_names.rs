use anyhow::Result;
use std::collections::HashSet;

use crate::db::HcomDb;
use crate::shared::time::now_epoch_i64;

// Names are 4-letter CVCV (consonant-vowel-consonant-vowel) patterns.
// Curated "gold" names score highest, generated names fill the pool.

const CONSONANTS: &[u8] = b"bdfghklmnprstvz";
const VOWELS: &[u8] = b"aeiou";

/// Curated gold names (high recognition, pleasant).
pub(crate) fn gold_names() -> HashSet<&'static str> {
    [
        // Real/common names
        "luna", "nova", "nora", "zara", "kira", "mila", "lola", "lara", "sara", "rhea", "nina",
        "mira", "tara", "sora", "cora", "dora", "gina", "lina", "viva", "risa", "mimi", "coco",
        "koko", "lili", "navi", "ravi", "rani", "riko", "niko", "mako", "saki", "maki", "nami",
        "loki", "rori", "lori", "mori", "nori", "tori", "gigi", "hana", "hiro", "tomo", "sumi",
        "vega", "kobe", "rafa", "lana", "lena", "dara", "niro", "yuki", "yuri", "maya", "juno",
        "nico", "rosa", "vera", "rina", "mika", "yoko", "yumi", "ruby", "lily", "cici", "hera",
        // Real words
        "miso", "taro", "boba", "kava", "soda", "cola", "coda", "data", "beta", "sofa", "mono",
        "moto", "tiki", "koda", "kali", "gala", "hula", "kula", "puma", "yoga", "zola", "zori",
        "veto", "vivo", "dino", "nemo", "hero", "zero", "memo", "demo", "polo", "solo", "logo",
        "halo", "dojo", "judo", "sumo", "tofu", "guru", "vino", "diva", "dodo", "silo", "peso",
        "lulu", "pita", "feta", "bobo", "brie", "fava", "duma", "beto", "moku", "bozo", "tuna",
        "lava", "hobo", "kiwi", "mojo", "yoyo", "sake", "wiki", "fiji", "bali", "kona", "poke",
        "cafe", "soho", "boho", "nano", "zulu", "deli", "rose", "jedi", "yoda",
        // Invented but natural-sounding
        "zumi", "reko", "valo", "kazu", "mero", "niru", "piko", "hazu", "toku", "veki",
    ]
    .into_iter()
    .collect()
}

pub(crate) fn banned_names() -> HashSet<&'static str> {
    [
        "help", "exit", "quit", "sudo", "bash", "curl", "grep", "init", "list", "send", "stop",
        "test", "meta",
    ]
    .into_iter()
    .collect()
}

pub(crate) fn score_name(name: &str, gold: &HashSet<&str>, banned: &HashSet<&str>) -> i32 {
    if banned.contains(name) {
        return i32::MIN / 2;
    }

    let mut score: i32 = 0;
    let bytes = name.as_bytes();

    // Strong preference for curated names
    if gold.contains(name) {
        score += 4000;
    }

    // Friendly flow letters
    if bytes
        .iter()
        .any(|&c| matches!(c, b'l' | b'r' | b'n' | b'm'))
    {
        score += 40;
    }

    // Slight spice: prefer exactly one v/z
    let vz_count = bytes.iter().filter(|&&c| c == b'v' || c == b'z').count();
    if vz_count == 1 {
        score += 12;
    } else if vz_count >= 2 {
        score -= 15;
    }

    // Avoid doubled vowels (e.g., "mama" pattern)
    if bytes.len() >= 4 && bytes[1] == bytes[3] {
        score -= 8;
    }

    // Name-like endings (a, e, o)
    if bytes.len() >= 4 && matches!(bytes[3], b'a' | b'e' | b'o') {
        score += 6;
    }

    score
}

#[derive(Clone)]
pub(crate) struct ScoredName {
    pub(crate) score: i32,
    pub(crate) name: String,
}

/// Build scored pool of all valid CVCV names plus curated gold names.
pub(crate) fn build_name_pool(limit: usize) -> Vec<ScoredName> {
    let gold = gold_names();
    let banned = banned_names();
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    // Generate all CVCV combinations
    for &c1 in CONSONANTS {
        for &v1 in VOWELS {
            for &c2 in CONSONANTS {
                for &v2 in VOWELS {
                    let name = format!("{}{}{}{}", c1 as char, v1 as char, c2 as char, v2 as char);
                    if banned.contains(name.as_str()) {
                        continue;
                    }
                    let s = score_name(&name, &gold, &banned);
                    seen.insert(name.clone());
                    candidates.push(ScoredName { score: s, name });
                }
            }
        }
    }

    // Inject gold names that don't match CVCV pattern (e.g., coco, juno, maya)
    for &name in &gold {
        if !seen.contains(name) && !banned.contains(name) {
            let s = score_name(name, &gold, &banned);
            seen.insert(name.to_string());
            candidates.push(ScoredName {
                score: s,
                name: name.to_string(),
            });
        }
    }

    // Sort by score descending
    candidates.sort_by_key(|b| std::cmp::Reverse(b.score));
    candidates.truncate(limit);
    candidates
}

/// Pre-built name pool (lazily initialized).
pub(crate) fn name_pool() -> &'static Vec<ScoredName> {
    use std::sync::OnceLock;
    static POOL: OnceLock<Vec<ScoredName>> = OnceLock::new();
    POOL.get_or_init(|| build_name_pool(5000))
}

/// Check if name is too similar to alive instances (Hamming distance <= 2).
pub(crate) fn is_too_similar(name: &str, alive_names: &HashSet<String>) -> bool {
    let name_bytes = name.as_bytes();
    for other in alive_names {
        if other.len() != name.len() {
            continue;
        }
        let diff = name_bytes
            .iter()
            .zip(other.as_bytes())
            .filter(|(a, b)| a != b)
            .count();
        if diff <= 2 {
            return true;
        }
    }
    false
}

/// Allocate a name with bias toward high-scoring names.
/// Three tiers: (1) weighted sampling + similarity, (2) greedy + similarity,
/// (3) greedy without similarity (last resort).
pub(crate) fn allocate_name(
    is_taken: &dyn Fn(&str) -> bool,
    alive_names: &HashSet<String>,
    attempts: usize,
    top_window: usize,
    temperature: f64,
) -> Result<String> {
    use rand::RngExt;
    let pool = name_pool();
    let mut rng = rand::rng();

    let window_size = top_window.min(pool.len()).max(50);
    let window = &pool[..window_size];

    // Compute softmax weights (numerically stable)
    let max_score = window.iter().map(|x| x.score).max().unwrap_or(0) as f64;
    let weights: Vec<f64> = window
        .iter()
        .map(|x| ((x.score as f64 - max_score) / temperature).exp())
        .collect();
    let total_weight: f64 = weights.iter().sum();

    // Tier 1: Weighted sampling with similarity check
    for _ in 0..attempts {
        let r: f64 = rng.random::<f64>() * total_weight;
        let mut cumulative = 0.0;
        let mut chosen_idx = 0;
        for (i, w) in weights.iter().enumerate() {
            cumulative += w;
            if cumulative >= r {
                chosen_idx = i;
                break;
            }
        }
        let choice = &window[chosen_idx].name;
        if !is_taken(choice) && !is_too_similar(choice, alive_names) {
            return Ok(choice.clone());
        }
    }

    // Tier 2: Greedy with similarity check
    for item in pool.iter() {
        if !is_taken(&item.name) && !is_too_similar(&item.name, alive_names) {
            return Ok(item.name.clone());
        }
    }

    // Tier 3: Greedy without similarity (last resort)
    for item in pool.iter() {
        if !is_taken(&item.name) {
            return Ok(item.name.clone());
        }
    }

    Err(anyhow::anyhow!("No available names left in pool"))
}

pub(crate) fn collect_taken_names(db: &HcomDb) -> Result<(HashSet<String>, HashSet<String>)> {
    let instances = db.iter_instances_full()?;
    let alive_names: HashSet<String> = instances.iter().map(|r| r.name.clone()).collect();
    let mut taken_names = alive_names.clone();

    let stopped: Vec<String> = {
        let mut stmt = db.conn().prepare(
            "SELECT DISTINCT instance FROM events
             WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'",
        )?;
        stmt.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect()
    };
    taken_names.extend(stopped);

    Ok((alive_names, taken_names))
}

/// Hash any string to a memorable 4-char name.
/// Used for device short IDs. Uses FNV-1a hash for distribution.
pub fn hash_to_name(input: &str, collision_attempt: u32) -> String {
    let pool = name_pool();
    let hash_words = &pool[..pool.len().min(500)];

    // FNV-1a hash (32-bit)
    let mut h: u32 = 2166136261;
    for c in input.bytes() {
        h ^= c as u32;
        h = h.wrapping_mul(16777619);
    }
    h = h.wrapping_add(collision_attempt.wrapping_mul(31337));

    let idx = (h as usize) % hash_words.len();
    hash_words[idx].name.clone()
}

/// Generate a unique instance name with flock-based reservation.
/// Creates a placeholder row in DB to prevent TOCTOU races.
pub fn generate_unique_name(db: &HcomDb) -> Result<String> {
    use std::fs::{File, create_dir_all};

    let lock_path = db
        .path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".tmp")
        .join("name_gen.lock");
    if let Some(parent) = lock_path.parent() {
        create_dir_all(parent)?;
    }

    let lock_file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    // Acquire exclusive file lock
    use nix::fcntl::{Flock, FlockArg};
    let flock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow::anyhow!("flock failed: {}", e))?;

    let result = (|| -> Result<String> {
        let (alive_names, taken_names) = collect_taken_names(db)?;

        let name = allocate_name(
            &|n| taken_names.contains(n) || db.get_instance_full(n).ok().flatten().is_some(),
            &alive_names,
            200,
            1200,
            900.0,
        )?;

        // Reserve with placeholder row
        let now = now_epoch_i64();
        let last_event_id = db.get_last_event_id();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!(name));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("last_event_id".into(), serde_json::json!(last_event_id));
        db.save_instance_named(&name, &data)?;

        Ok(name)
    })();

    // Unlock (drop the flock guard)
    let _file = Flock::unlock(flock);

    result
}

/// Sanitize agent_type for use in a structured subagent name:
/// lowercase, keep `[a-z0-9_]`, collapse leading/trailing underscores,
/// fall back to "task" if empty.
pub fn sanitize_subagent_type(raw: &str) -> String {
    let lowered: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = lowered.trim_matches('_');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parameters for allocating a structured subagent instance row.
pub struct SubagentAllocation<'a> {
    pub agent_id: &'a str,
    pub agent_type: &'a str,
    pub parent_name: &'a str,
    pub parent_session_id: Option<&'a str>,
    pub parent_tag: Option<&'a str>,
    /// Initial value for the `status` column (e.g. `"active"` or `"inactive"`).
    pub status: &'a str,
    /// Optional `status_context` column value.
    pub status_context: Option<&'a str>,
}

/// Allocate a structured subagent instance row `{parent}_{type}_{N}`.
///
/// If an instance row already exists for `agent_id`, returns its name without
/// re-inserting (so SubagentStart can run before `hcom start --name` without
/// creating duplicates, and vice versa). Otherwise computes the next free
/// suffix, INSERTs the row with `SQLite UNIQUE(name)` as the collision guard,
/// and retries once with `max_n + 2` on constraint violation.
pub fn allocate_subagent_instance(db: &HcomDb, info: &SubagentAllocation) -> Result<String> {
    // Return early if a row already exists for this agent_id.
    let existing: Option<String> = db
        .conn()
        .query_row(
            "SELECT name FROM instances WHERE agent_id = ?",
            rusqlite::params![info.agent_id],
            |row| row.get(0),
        )
        .ok();
    if let Some(name) = existing {
        return Ok(name);
    }

    let sanitized = sanitize_subagent_type(info.agent_type);
    let pattern = format!("{}_{}_", info.parent_name, sanitized);
    let like_pattern = format!("{pattern}%");
    let names: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT name FROM instances WHERE name LIKE ?")?;
        stmt.query_map(rusqlite::params![like_pattern], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect()
    };

    let mut max_n: u32 = 0;
    for name in &names {
        if let Some(suffix) = name.strip_prefix(&pattern) {
            if let Ok(n) = suffix.parse::<u32>() {
                max_n = max_n.max(n);
            }
        }
    }

    let candidate = format!("{pattern}{}", max_n + 1);
    let initial_event_id = db.get_last_event_id();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let now = crate::shared::time::now_epoch_f64();

    let insert_sql = "INSERT INTO instances \
         (name, session_id, parent_session_id, parent_name, tag, agent_id, \
          created_at, last_event_id, directory, last_stop, status, status_context) \
         VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)";

    let do_insert = |name: &str| -> rusqlite::Result<usize> {
        db.conn().execute(
            insert_sql,
            rusqlite::params![
                name,
                info.parent_session_id,
                info.parent_name,
                info.parent_tag,
                info.agent_id,
                now,
                initial_event_id,
                cwd,
                info.status,
                info.status_context,
            ],
        )
    };

    match do_insert(&candidate) {
        Ok(_) => Ok(candidate),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let retry = format!("{pattern}{}", max_n + 2);
            do_insert(&retry).map_err(|e| {
                anyhow::anyhow!("Failed to create unique subagent name after retry: {e}")
            })?;
            Ok(retry)
        }
        Err(e) => Err(anyhow::anyhow!("Failed to insert subagent instance: {e}")),
    }
}

#[cfg(test)]
mod subagent_alloc_tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_db() -> (TempDir, HcomDb) {
        let tmp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&tmp.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        (tmp, db)
    }

    fn alloc<'a>(agent_id: &'a str, agent_type: &'a str) -> SubagentAllocation<'a> {
        // parent_session_id=None to skip the FK to instances(session_id) — we
        // don't insert a real parent row, and the FK isn't relevant to what
        // these tests cover.
        SubagentAllocation {
            agent_id,
            agent_type,
            parent_name: "luna",
            parent_session_id: None,
            parent_tag: None,
            status: "inactive",
            status_context: Some("subagent:dormant"),
        }
    }

    #[test]
    fn sanitize_lowercases_and_substitutes() {
        assert_eq!(sanitize_subagent_type("Code-Reviewer"), "code_reviewer");
        assert_eq!(sanitize_subagent_type("MY.Agent/v2"), "my_agent_v2");
    }

    #[test]
    fn sanitize_trims_underscore_runs() {
        assert_eq!(sanitize_subagent_type("__weird__"), "weird");
        assert_eq!(sanitize_subagent_type("--//.."), "task");
        assert_eq!(sanitize_subagent_type(""), "task");
    }

    #[test]
    fn allocate_assigns_sequential_suffixes_per_parent_and_type() {
        let (_tmp, db) = setup_db();
        let n1 = allocate_subagent_instance(&db, &alloc("aid-1", "reviewer")).unwrap();
        let n2 = allocate_subagent_instance(&db, &alloc("aid-2", "reviewer")).unwrap();
        let n3 = allocate_subagent_instance(&db, &alloc("aid-3", "explorer")).unwrap();
        assert_eq!(n1, "luna_reviewer_1");
        assert_eq!(n2, "luna_reviewer_2");
        assert_eq!(n3, "luna_explorer_1");
    }

    #[test]
    fn allocate_is_idempotent_on_agent_id() {
        let (_tmp, db) = setup_db();
        let first = allocate_subagent_instance(&db, &alloc("aid-1", "reviewer")).unwrap();
        // Same agent_id, different type — must return the original row's name,
        // not insert a new one.
        let second = allocate_subagent_instance(&db, &alloc("aid-1", "explorer")).unwrap();
        assert_eq!(first, second);
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE agent_id = ?",
                rusqlite::params!["aid-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn allocate_retries_on_name_collision() {
        let (_tmp, db) = setup_db();
        // Pre-seed `luna_reviewer_1` directly so the natural pick collides
        // (max_n=0 → candidate=_1 already taken → retry with _2).
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, status_time, created_at, last_stop) \
                 VALUES ('luna_reviewer_1', 'active', 0, 0.0, 0)",
                [],
            )
            .unwrap();
        // Wipe agent_id so the LIKE-scan finds the collider but the agent_id
        // shortcut doesn't fire.
        let name = allocate_subagent_instance(&db, &alloc("aid-x", "reviewer")).unwrap();
        // The seeded row has no suffix-N parsable from agent_id lookup, but
        // it does match the LIKE pattern and parses as N=1 → next is _2.
        assert_eq!(name, "luna_reviewer_2");
    }

    #[test]
    fn allocate_writes_status_and_context_columns() {
        let (_tmp, db) = setup_db();
        let name = allocate_subagent_instance(&db, &alloc("aid-1", "reviewer")).unwrap();
        let (status, ctx, parent): (String, String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT status, status_context, parent_name FROM instances WHERE name = ?",
                rusqlite::params![name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "inactive");
        assert_eq!(ctx, "subagent:dormant");
        assert_eq!(parent.as_deref(), Some("luna"));
    }
}
