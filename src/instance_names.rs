use anyhow::Result;
use rusqlite::OptionalExtension;
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
        "luna", "nova", "nora", "zara", "kira", "mila", "lola", "lara", "sara", "nina", "mira",
        "tara", "sora", "dora", "gina", "lina", "viva", "risa", "mimi", "koko", "lili", "navi",
        "ravi", "rani", "riko", "niko", "mako", "saki", "maki", "nami", "loki", "rori", "lori",
        "mori", "nori", "tori", "gigi", "hana", "hiro", "tomo", "sumi", "vega", "kobe", "rafa",
        "lana", "lena", "dara", "niro", "rosa", "vera", "rina", "mika", "hera", "bela", "beni",
        "bono", "dani", "gabi", "haru", "kato", "keno", "kino", "levi", "lila", "mara", "mina",
        "mona", "remi", "romi", "rumi", "suki", "tina", "vito", "zeno", "kiki", "kimi", "milo",
        "gino", "fifi", "nene", "fido", "lilo", "nara", "miro", "rita", "kuma", "neko", "kana",
        "kiri", "kano", "sana", "miko", "haka", // Real words
        "miso", "taro", "boba", "kava", "soda", "data", "beta", "sofa", "mono", "moto", "tiki",
        "koda", "kali", "gala", "hula", "kula", "puma", "zola", "zori", "veto", "vivo", "dino",
        "nemo", "hero", "zero", "memo", "demo", "polo", "solo", "logo", "halo", "sumo", "tofu",
        "guru", "vino", "diva", "dodo", "silo", "peso", "lulu", "pita", "feta", "bobo", "fava",
        "duma", "beto", "moku", "bozo", "tuna", "lava", "hobo", "sake", "bali", "kona", "poke",
        "soho", "boho", "nano", "zulu", "deli", "rose", "dosa", "gobi", "kale", "kilo", "limo",
        "momo", "sari", "soba", "tapa", "toga", "toto", "keto", "midi", "mini", "meme", "tutu",
        "tuba", "todo", "meta", "sage", "vase", "tide", "kite", "lime", "vibe", "dune", "maze",
        "rune", "muse", "dove", "vida", "pogo", "magi", "lira", "tipi", "soma", "lobo", "fugu",
        "naga", // Invented but natural-sounding
        "zumi", "reko", "valo", "kazu", "mero", "niru", "piko", "hazu", "toku", "veki", "lumo",
        "melo",
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

    // Strong preference for curated names. Tuned to be ~28x more likely than a
    // baseline generated name under T=30 softmax, while still letting diversity
    // penalties (-80 first letter, -20 trailing vowel) override a clashing gold.
    if gold.contains(name) {
        score += 100;
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

/// Build scored pool of all valid CVCV names, with gold names ranked highest.
pub(crate) fn build_name_pool(limit: usize) -> Vec<ScoredName> {
    let gold = gold_names();
    let banned = banned_names();
    let mut candidates = Vec::new();

    for &c1 in CONSONANTS {
        for &v1 in VOWELS {
            for &c2 in CONSONANTS {
                for &v2 in VOWELS {
                    let name = format!("{}{}{}{}", c1 as char, v1 as char, c2 as char, v2 as char);
                    if banned.contains(name.as_str()) {
                        continue;
                    }
                    let s = score_name(&name, &gold, &banned);
                    candidates.push(ScoredName { score: s, name });
                }
            }
        }
    }

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

    // Diversity penalties relative to currently-alive names: spread first
    // letters across the alphabet and avoid stacking matching trailing vowels.
    let alive_first_letters: HashSet<u8> = alive_names
        .iter()
        .filter_map(|n| n.as_bytes().first().copied())
        .collect();
    let alive_trailing_vowels: HashSet<u8> = alive_names
        .iter()
        .filter_map(|n| n.as_bytes().last().copied())
        .filter(|c| matches!(c, b'a' | b'e' | b'i' | b'o' | b'u'))
        .collect();
    let adjust = |item: &ScoredName| -> i32 {
        let bytes = item.name.as_bytes();
        let mut s = item.score;
        if let Some(&first) = bytes.first()
            && alive_first_letters.contains(&first)
        {
            s -= 80;
        }
        if let Some(&last) = bytes.last()
            && alive_trailing_vowels.contains(&last)
        {
            s -= 20;
        }
        s
    };

    let window_size = top_window.min(pool.len()).max(50);
    let mut window: Vec<(i32, &ScoredName)> = pool[..window_size]
        .iter()
        .map(|item| (adjust(item), item))
        .collect();
    window.sort_by_key(|(s, _)| std::cmp::Reverse(*s));

    // Compute softmax weights (numerically stable)
    let max_score = window.iter().map(|(s, _)| *s).max().unwrap_or(0) as f64;
    let weights: Vec<f64> = window
        .iter()
        .map(|(s, _)| ((*s as f64 - max_score) / temperature).exp())
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
        let choice = &window[chosen_idx].1.name;
        if !is_taken(choice) && !is_too_similar(choice, alive_names) {
            return Ok(choice.clone());
        }
    }

    // Tier 2: Greedy by adjusted score, with similarity check
    let mut sorted_full: Vec<(i32, &ScoredName)> =
        pool.iter().map(|item| (adjust(item), item)).collect();
    sorted_full.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    for (_, item) in &sorted_full {
        if !is_taken(&item.name) && !is_too_similar(&item.name, alive_names) {
            return Ok(item.name.clone());
        }
    }

    // Tier 3: Greedy without similarity (last resort)
    for (_, item) in &sorted_full {
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
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'stopped'
               AND COALESCE(json_extract(data, '$.placeholder'), 0) != 1",
        )?;
        stmt.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect()
    };
    taken_names.extend(stopped);

    Ok((alive_names, taken_names))
}

/// Total CVCV outputs: 15 consonants × 5 vowels × 15 × 5.
pub const CVCV_SPACE: usize = 15 * 5 * 15 * 5;

/// Hash any string to a 4-letter CVCV name. Used for device short IDs.
///
/// Picks an initial index from the FNV-1a hash of `input`, then linearly
/// walks the CVCV space by `collision_attempt`. Probing with
/// `attempt = 0..CVCV_SPACE` is guaranteed to visit every distinct CVCV
/// output exactly once. Output depends only on the CVCV alphabets, not on
/// the curated agent-name pool, so it is stable across releases.
pub fn hash_to_name(input: &str, collision_attempt: u32) -> String {
    let mut h: u32 = 2166136261;
    for c in input.bytes() {
        h ^= c as u32;
        h = h.wrapping_mul(16777619);
    }
    let idx = (h as usize).wrapping_add(collision_attempt as usize) % CVCV_SPACE;
    let bytes = [
        CONSONANTS[idx % 15],
        VOWELS[(idx / 15) % 5],
        CONSONANTS[(idx / 75) % 15],
        VOWELS[(idx / 1125) % 5],
    ];
    String::from_utf8(bytes.to_vec()).unwrap()
}

pub const PLACEHOLDER_STATUS: &str = "pending";
pub const PLACEHOLDER_CONTEXT: &str = "new";

pub(crate) fn allocate_unreserved_name(db: &HcomDb) -> Result<String> {
    let (alive_names, taken_names) = collect_taken_names(db)?;

    allocate_name(
        &|n| taken_names.contains(n) || db.get_instance_full(n).ok().flatten().is_some(),
        &alive_names,
        200,
        1200,
        30.0,
    )
}

/// Generate a unique instance name with flock-based reservation.
/// Creates a placeholder row in DB to prevent TOCTOU races.
pub fn generate_unique_name(db: &HcomDb) -> Result<String> {
    reserve_generated_name(db)
}

pub(crate) fn reserve_generated_name(db: &HcomDb) -> Result<String> {
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

    // Acquire exclusive file lock; released when `lock_file` drops at scope end.
    crate::sys::fs::lock_exclusive(&lock_file)
        .map_err(|e| anyhow::anyhow!("flock failed: {}", e))?;

    let result = (|| -> Result<String> {
        let name = allocate_unreserved_name(db)?;

        // Reserve with placeholder row
        let now = now_epoch_i64();
        let last_event_id = db.get_last_event_id();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!(name));
        data.insert("status".into(), serde_json::json!(PLACEHOLDER_STATUS));
        data.insert(
            "status_context".into(),
            serde_json::json!(PLACEHOLDER_CONTEXT),
        );
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("last_event_id".into(), serde_json::json!(last_event_id));
        data.insert(
            "wait_timeout".into(),
            serde_json::json!(crate::config::HcomConfig::effective_timeout()),
        );
        db.save_instance_reservation(&name, &data)?;

        Ok(name)
    })();

    // Lock released when `lock_file` is dropped at function scope end.
    drop(lock_file);

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
/// suffix and INSERTs the row.
///
/// The idempotency check, suffix scan, and INSERT share one `BEGIN IMMEDIATE`
/// transaction so concurrent sibling hooks cannot choose the same suffix.
pub fn allocate_subagent_instance(db: &HcomDb, info: &SubagentAllocation) -> Result<String> {
    let sanitized = sanitize_subagent_type(info.agent_type);
    let pattern = format!("{}_{}_", info.parent_name, sanitized);
    let like_pattern = format!("{pattern}%");
    let initial_event_id = db.get_last_event_id();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let now = crate::shared::time::now_epoch_f64();

    db.with_immediate_transaction(|txn| {
        // Idempotency check must live inside the transaction too: otherwise a
        // concurrent SubagentStart and `hcom start --name <agent_id>` for the
        // same agent_id could both miss it and insert two rows.
        let existing: Option<String> = txn
            .query_row(
                "SELECT name FROM instances WHERE agent_id = ?",
                rusqlite::params![info.agent_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(name) = existing {
            txn.execute(
                "UPDATE instances
                 SET parent_session_id = ?, parent_name = ?, tag = ?, directory = ?,
                     status = ?, status_context = ?, status_time = ?, last_seen = ?
                 WHERE agent_id = ?",
                rusqlite::params![
                    info.parent_session_id,
                    info.parent_name,
                    info.parent_tag,
                    cwd,
                    info.status,
                    info.status_context,
                    crate::shared::time::now_epoch_i64(),
                    crate::shared::time::now_epoch_i64(),
                    info.agent_id,
                ],
            )?;
            return Ok(name);
        }

        // Resume the same hcom identity when Claude reuses an agent_id after a
        // definitive SubagentStop removed the live row.
        let stopped: Option<(String, i64)> = txn
            .query_row(
                "SELECT instance,
                        COALESCE(json_extract(data, '$.snapshot.name_announced'), 0)
                 FROM events
                 WHERE type = 'life'
                   AND json_extract(data, '$.action') = 'stopped'
                   AND json_extract(data, '$.snapshot.agent_id') = ?
                   AND COALESCE(json_extract(data, '$.snapshot.parent_session_id'), '') = COALESCE(?, '')
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![info.agent_id, info.parent_session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((candidate, name_announced)) = stopped
            && txn
                .query_row(
                    "SELECT 1 FROM instances WHERE name = ?",
                    rusqlite::params![candidate],
                    |_| Ok(()),
                )
                .optional()?
                .is_none()
        {
            txn.execute(
                "INSERT INTO instances
                 (name, session_id, parent_session_id, parent_name, tag, agent_id,
                  created_at, last_event_id, directory, last_stop, status,
                  status_context, status_time, last_seen, name_announced)
                 VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    candidate,
                    info.parent_session_id,
                    info.parent_name,
                    info.parent_tag,
                    info.agent_id,
                    now,
                    initial_event_id,
                    cwd,
                    info.status,
                    info.status_context,
                    crate::shared::time::now_epoch_i64(),
                    crate::shared::time::now_epoch_i64(),
                    name_announced,
                ],
            )?;
            return Ok(candidate);
        }

        let names: Vec<String> = {
            let mut stmt = txn.prepare("SELECT name FROM instances WHERE name LIKE ?")?;
            stmt.query_map(rusqlite::params![like_pattern], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };

        let mut max_n: u32 = 0;
        for name in &names {
            if let Some(suffix) = name.strip_prefix(&pattern)
                && let Ok(n) = suffix.parse::<u32>()
            {
                max_n = max_n.max(n);
            }
        }

        let candidate = format!("{pattern}{}", max_n + 1);
        txn.execute(
            "INSERT INTO instances \
             (name, session_id, parent_session_id, parent_name, tag, agent_id, \
              created_at, last_event_id, directory, last_stop, status, status_context, status_time, last_seen) \
             VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?)",
            rusqlite::params![
                candidate,
                info.parent_session_id,
                info.parent_name,
                info.parent_tag,
                info.agent_id,
                now,
                initial_event_id,
                cwd,
                info.status,
                info.status_context,
                crate::shared::time::now_epoch_i64(),
                crate::shared::time::now_epoch_i64(),
            ],
        )?;
        Ok(candidate)
    })
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
    fn allocate_resume_reuses_stopped_identity_and_announcement_state() {
        let (_tmp, db) = setup_db();
        let original = allocate_subagent_instance(&db, &alloc("aid-1", "reviewer")).unwrap();
        db.conn()
            .execute(
                "UPDATE instances SET name_announced = 1 WHERE name = ?",
                rusqlite::params![original],
            )
            .unwrap();
        let snapshot = serde_json::json!({
            "action": "stopped",
            "snapshot": {
                "agent_id": "aid-1",
                "parent_name": "luna",
                "parent_session_id": null,
                "name_announced": 1
            }
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data)
                 VALUES ('2026-01-01T00:00:00Z', 'life', ?, ?)",
                rusqlite::params![original, snapshot.to_string()],
            )
            .unwrap();
        db.delete_instance(&original).unwrap();

        let resumed = allocate_subagent_instance(&db, &alloc("aid-1", "reviewer")).unwrap();
        assert_eq!(resumed, original);
        let row = db.get_instance_full(&resumed).unwrap().unwrap();
        assert_eq!(row.name_announced, 1);
    }

    #[test]
    fn allocate_resume_survives_root_name_rebind() {
        let (_tmp, db) = setup_db();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('luna', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        let original = allocate_subagent_instance(
            &db,
            &SubagentAllocation {
                agent_id: "aid-1",
                agent_type: "reviewer",
                parent_name: "luna",
                parent_session_id: Some("sess-1"),
                parent_tag: None,
                status: "inactive",
                status_context: Some("subagent:dormant"),
            },
        )
        .unwrap();
        let snapshot = serde_json::json!({
            "action": "stopped",
            "snapshot": {
                "agent_id": "aid-1",
                "parent_name": "luna",
                "parent_session_id": "sess-1",
                "name_announced": 1
            }
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data)
                 VALUES ('2026-01-01T00:00:00Z', 'life', ?, ?)",
                rusqlite::params![original, snapshot.to_string()],
            )
            .unwrap();
        db.delete_instance(&original).unwrap();

        let resumed = allocate_subagent_instance(
            &db,
            &SubagentAllocation {
                agent_id: "aid-1",
                agent_type: "reviewer",
                parent_name: "sol",
                parent_session_id: Some("sess-1"),
                parent_tag: None,
                status: "inactive",
                status_context: Some("subagent:dormant"),
            },
        )
        .unwrap();
        assert_eq!(resumed, original);
        let row = db.get_instance_full(&resumed).unwrap().unwrap();
        assert_eq!(row.parent_name.as_deref(), Some("sol"));
        assert_eq!(row.name_announced, 1);
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
    fn allocate_concurrent_siblings_all_get_rows() {
        // Separate connections mirror concurrent hook processes.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("race.db");
        {
            let db = HcomDb::open_raw(&path).unwrap();
            db.init_db().unwrap();
        }

        const N: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = HcomDb::open_raw(&path).unwrap();
                    let agent_id = format!("aid-{i}");
                    barrier.wait();
                    allocate_subagent_instance(
                        &db,
                        &SubagentAllocation {
                            agent_id: &agent_id,
                            agent_type: "reviewer",
                            parent_name: "luna",
                            parent_session_id: None,
                            parent_tag: None,
                            status: "inactive",
                            status_context: Some("subagent:dormant"),
                        },
                    )
                })
            })
            .collect();

        let mut ok = 0;
        for h in handles {
            if h.join().unwrap().is_ok() {
                ok += 1;
            }
        }

        let db = HcomDb::open_raw(&path).unwrap();
        let rows: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE parent_name = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ok, N, "every concurrent allocation must succeed");
        assert_eq!(rows, N as i64, "every subagent must get its own row");
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

#[cfg(test)]
mod reservation_tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: tests using this guard are marked #[serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: tests using this guard are marked #[serial].
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard are marked #[serial].
            unsafe {
                match &self.previous {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn setup_db() -> (TempDir, HcomDb) {
        let tmp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&tmp.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        (tmp, db)
    }

    #[test]
    #[serial]
    fn placeholder_row_honors_configured_hcom_timeout() {
        // Regression test for issue #71: the generated-name placeholder row
        // must carry the effective HCOM_TIMEOUT, not silently fall back to
        // the old always-86400 schema default.
        let _env = EnvVarGuard::set("HCOM_TIMEOUT", "45");
        let (_tmp, db) = setup_db();

        let name = reserve_generated_name(&db).unwrap();

        let wait_timeout: Option<i64> = db
            .conn()
            .query_row(
                "SELECT wait_timeout FROM instances WHERE name = ?",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wait_timeout, Some(45));
    }

    #[test]
    #[serial]
    fn placeholder_row_falls_back_to_120_without_config() {
        let _env = EnvVarGuard::unset("HCOM_TIMEOUT");
        let (_tmp, db) = setup_db();

        let name = reserve_generated_name(&db).unwrap();

        let wait_timeout: Option<i64> = db
            .conn()
            .query_row(
                "SELECT wait_timeout FROM instances WHERE name = ?",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .unwrap();
        // Default HcomConfig::timeout is 86400 (schema-equivalent default),
        // preserved for anyone who hasn't set HCOM_TIMEOUT.
        assert_eq!(wait_timeout, Some(86400));
    }
}
