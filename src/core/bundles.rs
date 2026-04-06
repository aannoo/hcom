//! Bundle helpers for creating and validating bundle events.
//!
//! packages with event references, file lists, and transcript ranges.
//! Used by `hcom bundle` and `hcom send --title`.

use rand::RngExt;
use serde_json::Value;
use std::path::Path;

use super::detail_levels::validate_detail_level;
use crate::shared::errors::HcomError;
use crate::shared::{SenderIdentity, SenderKind};

/// Parse comma-separated list into list of non-empty trimmed strings.
pub fn parse_csv_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => vec![],
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
    }
}

/// Get bundle instance name from identity.
pub fn get_bundle_instance_name(identity: &SenderIdentity) -> String {
    match identity.kind {
        SenderKind::External => format!("ext_{}", identity.name),
        SenderKind::System => format!("sys_{}", identity.name),
        SenderKind::Instance => identity.name.clone(),
    }
}

/// Generate a short random bundle id.
pub fn generate_bundle_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 4] = rng.random();
    format!("bundle:{}", hex::encode(&bytes))
}

// Inline hex encoding (avoids adding `hex` crate).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

/// Parse a transcript reference into normalized format.
///
/// Accepts string "range:detail" (e.g., "3-14:normal", "6:full")
/// or object {"range": "6", "detail": "full", "note": "..."}.
pub fn parse_transcript_ref(ref_val: &Value) -> Result<Value, String> {
    match ref_val {
        Value::Object(map) => {
            let _range = map
                .get("range")
                .and_then(|v| v.as_str())
                .ok_or("Transcript ref object must have 'range' field")?;
            let detail = map
                .get("detail")
                .and_then(|v| v.as_str())
                .ok_or("Transcript ref object must have 'detail' field")?;
            validate_detail_level(detail)?;
            // Return as-is (already normalized)
            Ok(ref_val.clone())
        }
        Value::String(s) => {
            if !s.contains(':') {
                return Err(format!(
                    "Transcript ref must include detail level. Got: '{}'\n\
                     Format: \"range:detail\" (e.g., \"3-14:normal\", \"10:full\", \"20-25:detailed\")",
                    s
                ));
            }
            let (range_part, detail) = s.split_once(':').unwrap();
            let range_trimmed = range_part.trim();
            let detail_trimmed = detail.trim();

            if range_trimmed.is_empty() {
                return Err(format!("Empty range in transcript ref: '{}'", s));
            }
            if detail_trimmed.is_empty() {
                return Err(format!("Empty detail level in transcript ref: '{}'", s));
            }

            validate_detail_level(detail_trimmed)?;

            let mut obj = serde_json::Map::new();
            obj.insert("range".into(), Value::String(range_trimmed.into()));
            obj.insert("detail".into(), Value::String(detail_trimmed.into()));
            Ok(Value::Object(obj))
        }
        _ => Err(format!(
            "Transcript ref must be a string or object, got {:?}",
            ref_val
        )),
    }
}

/// Maximum estimated lines for bundle output to prevent massive dumps.
const MAX_ESTIMATED_LINES: usize = 15_000;

/// Validate bundle payload fields and types.
pub fn validate_bundle(bundle: &mut Value) -> Result<(), String> {
    let obj = bundle
        .as_object_mut()
        .ok_or("bundle must be a JSON object")?;

    // Required fields
    let missing: Vec<&str> = ["title", "description", "refs"]
        .iter()
        .filter(|k| !obj.contains_key(**k))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!("Missing required fields: {}", missing.join(", ")));
    }

    // Estimate bundle size
    if let Some(Value::Object(refs)) = obj.get("refs") {
        let files_len = refs
            .get("files")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let events_len = refs
            .get("events")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let transcript_len = refs
            .get("transcript")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        let estimated = files_len + events_len * 50 + transcript_len * 500;
        if estimated > MAX_ESTIMATED_LINES {
            return Err(format!(
                "Bundle too large (estimated {} lines of output). \
                 Limit is {} lines. Split into multiple smaller bundles.",
                estimated, MAX_ESTIMATED_LINES
            ));
        }
    }

    // Type checks
    if !obj.get("title").is_some_and(|v| v.is_string()) {
        return Err("title must be a string".into());
    }
    if !obj.get("description").is_some_and(|v| v.is_string()) {
        return Err("description must be a string".into());
    }

    let refs = obj.get("refs").ok_or("refs must be an object")?;
    if !refs.is_object() {
        return Err("refs must be an object".into());
    }
    let refs_obj = refs.as_object().unwrap();

    for key in &["events", "files", "transcript"] {
        if !refs_obj.contains_key(*key) {
            return Err(format!("refs.{} is required", key));
        }
        if !refs_obj[*key].is_array() {
            return Err(format!("refs.{} must be a list", key));
        }
    }

    // Non-empty refs
    if refs_obj["transcript"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        return Err("refs.transcript is required\n\
             Find ranges: hcom transcript <agent> [--last N]\n\
             Format: \"1-5:normal,10:full\""
            .into());
    }
    if refs_obj["events"].as_array().is_some_and(|a| a.is_empty()) {
        return Err("refs.events is required\n\
             Find events: hcom events [--last N]\n\
             Format: \"123,124\" or \"100-105\""
            .into());
    }
    if refs_obj["files"].as_array().is_some_and(|a| a.is_empty()) {
        return Err("refs.files is required\n\
             Include files you created, modified, discussed, or are relevant\n\
             Format: \"src/main.py,tests/test.py\""
            .into());
    }

    // Parse and normalize transcript refs
    let transcript_arr = refs_obj["transcript"].as_array().unwrap().clone();
    let mut normalized = Vec::with_capacity(transcript_arr.len());
    for ref_val in &transcript_arr {
        let parsed =
            parse_transcript_ref(ref_val).map_err(|e| format!("Invalid transcript ref: {}", e))?;
        normalized.push(parsed);
    }

    // Write normalized transcript back
    let refs_mut = obj.get_mut("refs").unwrap().as_object_mut().unwrap();
    refs_mut.insert("transcript".into(), Value::Array(normalized));

    // Check file existence (warn but don't error)
    if let Some(files) = refs_mut.get("files").and_then(|v| v.as_array()) {
        let missing_files: Vec<&str> = files
            .iter()
            .filter_map(|f| f.as_str())
            .filter(|path| !Path::new(path).exists())
            .collect();
        if !missing_files.is_empty() {
            eprintln!(
                "Warning: {} file(s) not found locally:",
                missing_files.len()
            );
            for f in missing_files.iter().take(5) {
                eprintln!("  - {}", f);
            }
            if missing_files.len() > 5 {
                eprintln!("  ... and {} more", missing_files.len() - 5);
            }
        }
    }

    // Validate extends
    if let Some(extends) = obj.get("extends") {
        if !extends.is_string() {
            return Err("extends must be a string".into());
        }
        // Note: parent bundle existence check requires DB access.
        // Call validate_extends_reference() separately when DB is available.
    }

    // Validate bundle_id
    if let Some(bid) = obj.get("bundle_id") {
        if !bid.is_string() {
            return Err("bundle_id must be a string".into());
        }
    }

    Ok(())
}

/// Validate extends reference against DB (checks parent bundle exists).
///
/// Warns to stderr if parent not found (non-fatal).
/// Call after validate_bundle when a DB handle is available.
pub fn validate_extends_reference(bundle: &Value, db: &crate::db::HcomDb) {
    let extends_val = match bundle.get("extends").and_then(|v| v.as_str()) {
        Some(v) if !v.is_empty() => v,
        _ => return,
    };

    let search_id = if extends_val.starts_with("bundle:") {
        extends_val.to_string()
    } else {
        format!("bundle:{}", extends_val)
    };

    match db.conn().prepare(
        "SELECT id FROM events WHERE type = 'bundle' AND json_extract(data, '$.bundle_id') = ?1",
    ) {
        Ok(mut stmt) => {
            match stmt.query_row(rusqlite::params![search_id], |_| Ok(())) {
                Ok(()) => {} // Found
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    eprintln!("Warning: Parent bundle not found: {}", extends_val);
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Could not validate parent bundle '{}': {}",
                        extends_val, e
                    );
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: Could not validate parent bundle '{}': {}",
                extends_val, e
            );
        }
    }
}

/// Create a bundle event and return its bundle_id.
pub fn create_bundle_event(
    bundle: &mut Value,
    instance: &str,
    created_by: Option<&str>,
    db: &crate::db::HcomDb,
) -> Result<String, HcomError> {
    validate_bundle(bundle).map_err(HcomError::InvalidInput)?;
    validate_extends_reference(bundle, db);

    let obj = bundle.as_object_mut().unwrap();

    let bundle_id = obj
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(generate_bundle_id);

    obj.insert("bundle_id".into(), Value::String(bundle_id.clone()));

    if let Some(by) = created_by {
        obj.insert("created_by".into(), Value::String(by.into()));
    }

    db.log_event("bundle", instance, &bundle.clone())
        .map_err(|e| HcomError::DatabaseError(format!("Failed to persist bundle event: {e}")))?;

    Ok(bundle_id)
}

/// Parse inline bundle creation flags from argv.
///
/// Returns (bundle_json, remaining_argv) if --title present, (None, argv) otherwise.
pub fn parse_inline_bundle_flags(argv: &[String]) -> Result<(Option<Value>, Vec<String>), String> {
    let bundle_flags = &[
        "--title",
        "--description",
        "--events",
        "--files",
        "--transcript",
        "--extends",
    ];

    let has_any = bundle_flags.iter().any(|f| argv.contains(&f.to_string()));

    // Check for duplicate flags
    for flag in bundle_flags {
        let count = argv.iter().filter(|a| a.as_str() == *flag).count();
        if count > 1 {
            return Err(format!("Duplicate flag {} (found {} times)", flag, count));
        }
    }

    // If bundle flags present but no --title, error
    if has_any && !argv.contains(&"--title".to_string()) {
        let present: Vec<&&str> = bundle_flags
            .iter()
            .filter(|f| argv.contains(&f.to_string()))
            .collect();
        return Err(format!(
            "Bundle flags require --title: found {} without --title",
            present.iter().map(|f| **f).collect::<Vec<_>>().join(", ")
        ));
    }

    if !argv.contains(&"--title".to_string()) {
        return Ok((None, argv.to_vec()));
    }

    // Extract flag values
    let mut remaining = Vec::new();
    let mut flag_values: std::collections::HashMap<&str, Option<String>> =
        std::collections::HashMap::new();

    let mut i = 0;
    while i < argv.len() {
        let is_bundle_flag = bundle_flags.contains(&argv[i].as_str());
        if is_bundle_flag {
            let flag = argv[i].as_str();
            if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                flag_values.insert(flag, Some(argv[i + 1].clone()));
                i += 2;
            } else {
                return Err(format!("Flag {} requires a value", flag));
            }
        } else {
            remaining.push(argv[i].clone());
            i += 1;
        }
    }

    let title = flag_values
        .get("--title")
        .and_then(|v| v.clone())
        .ok_or("--title is required for inline bundle creation")?;

    let description = flag_values
        .get("--description")
        .and_then(|v| v.clone())
        .ok_or("--description is required when --title is present")?;

    let events = parse_csv_list(flag_values.get("--events").and_then(|v| v.as_deref()));
    let files = parse_csv_list(flag_values.get("--files").and_then(|v| v.as_deref()));
    let transcript = parse_csv_list(flag_values.get("--transcript").and_then(|v| v.as_deref()));

    let mut bundle = serde_json::json!({
        "title": title,
        "description": description,
        "refs": {
            "events": events,
            "files": files,
            "transcript": transcript,
        }
    });

    if let Some(extends) = flag_values.get("--extends").and_then(|v| v.clone()) {
        bundle
            .as_object_mut()
            .unwrap()
            .insert("extends".into(), Value::String(extends));
    }

    Ok((Some(bundle), remaining))
}

/// Categorize an event by its file operation context.
pub fn is_file_op_context(context: &str) -> bool {
    super::filters::FILE_OP_CONTEXTS.contains(&context)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_csv_list =====

    #[test]
    fn test_parse_csv_list_basic() {
        assert_eq!(parse_csv_list(Some("a,b,c")), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_csv_list_trim() {
        assert_eq!(parse_csv_list(Some(" a , b , c ")), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_csv_list_empty() {
        let empty: Vec<String> = vec![];
        assert_eq!(parse_csv_list(None), empty);
        assert_eq!(parse_csv_list(Some("")), empty);
        assert_eq!(parse_csv_list(Some(",,,")), empty);
    }

    // ===== get_bundle_instance_name =====

    #[test]
    fn test_bundle_name_instance() {
        let id = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        assert_eq!(get_bundle_instance_name(&id), "luna");
    }

    #[test]
    fn test_bundle_name_external() {
        let id = SenderIdentity {
            kind: SenderKind::External,
            name: "user".into(),
            instance_data: None,
            session_id: None,
        };
        assert_eq!(get_bundle_instance_name(&id), "ext_user");
    }

    // ===== generate_bundle_id =====

    #[test]
    fn test_bundle_id_format() {
        let id = generate_bundle_id();
        assert!(id.starts_with("bundle:"));
        assert_eq!(id.len(), "bundle:".len() + 8); // 4 bytes = 8 hex chars
    }

    #[test]
    fn test_bundle_id_unique() {
        let a = generate_bundle_id();
        let b = generate_bundle_id();
        assert_ne!(a, b);
    }

    // ===== parse_transcript_ref =====

    #[test]
    fn test_parse_ref_string() {
        let val = serde_json::json!("3-14:normal");
        let parsed = parse_transcript_ref(&val).unwrap();
        assert_eq!(parsed["range"], "3-14");
        assert_eq!(parsed["detail"], "normal");
    }

    #[test]
    fn test_parse_ref_object() {
        let val = serde_json::json!({"range": "6", "detail": "full", "note": "design"});
        let parsed = parse_transcript_ref(&val).unwrap();
        assert_eq!(parsed["range"], "6");
        assert_eq!(parsed["detail"], "full");
        assert_eq!(parsed["note"], "design");
    }

    #[test]
    fn test_parse_ref_no_colon() {
        let val = serde_json::json!("3-14");
        let err = parse_transcript_ref(&val).unwrap_err();
        assert!(err.contains("must include detail level"));
    }

    #[test]
    fn test_parse_ref_invalid_detail() {
        let val = serde_json::json!("3-14:verbose");
        let err = parse_transcript_ref(&val).unwrap_err();
        assert!(err.contains("Invalid detail level"));
    }

    #[test]
    fn test_parse_ref_empty_range() {
        let val = serde_json::json!(":normal");
        let err = parse_transcript_ref(&val).unwrap_err();
        assert!(err.contains("Empty range"));
    }

    // ===== validate_bundle =====

    #[test]
    fn test_validate_bundle_valid() {
        let mut bundle = serde_json::json!({
            "title": "Test bundle",
            "description": "Testing",
            "refs": {
                "events": ["123"],
                "files": ["/tmp/test.rs"],
                "transcript": ["1-5:normal"]
            }
        });
        assert!(validate_bundle(&mut bundle).is_ok());
        // Check transcript was normalized
        let refs = bundle["refs"]["transcript"].as_array().unwrap();
        assert_eq!(refs[0]["range"], "1-5");
        assert_eq!(refs[0]["detail"], "normal");
    }

    #[test]
    fn test_validate_bundle_missing_title() {
        let mut bundle = serde_json::json!({
            "description": "Testing",
            "refs": {"events": [], "files": [], "transcript": []}
        });
        let err = validate_bundle(&mut bundle).unwrap_err();
        assert!(err.contains("Missing required fields"));
    }

    #[test]
    fn test_validate_bundle_empty_transcript() {
        let mut bundle = serde_json::json!({
            "title": "Test",
            "description": "Testing",
            "refs": {"events": ["1"], "files": ["a.py"], "transcript": []}
        });
        let err = validate_bundle(&mut bundle).unwrap_err();
        assert!(err.contains("refs.transcript is required"));
    }

    #[test]
    fn test_validate_bundle_empty_events() {
        let mut bundle = serde_json::json!({
            "title": "Test",
            "description": "Testing",
            "refs": {"events": [], "files": ["a.py"], "transcript": ["1:normal"]}
        });
        let err = validate_bundle(&mut bundle).unwrap_err();
        assert!(err.contains("refs.events is required"));
    }

    #[test]
    fn test_validate_bundle_empty_files() {
        let mut bundle = serde_json::json!({
            "title": "Test",
            "description": "Testing",
            "refs": {"events": ["1"], "files": [], "transcript": ["1:normal"]}
        });
        let err = validate_bundle(&mut bundle).unwrap_err();
        assert!(err.contains("refs.files is required"));
    }

    // ===== parse_inline_bundle_flags =====

    #[test]
    fn test_parse_inline_no_flags() {
        let argv: Vec<String> = vec!["--last".into(), "20".into()];
        let (bundle, remaining) = parse_inline_bundle_flags(&argv).unwrap();
        assert!(bundle.is_none());
        assert_eq!(remaining, argv);
    }

    #[test]
    fn test_parse_inline_with_title() {
        let argv: Vec<String> = vec![
            "--title".into(),
            "Test".into(),
            "--description".into(),
            "Desc".into(),
            "--events".into(),
            "1,2".into(),
            "--files".into(),
            "a.py".into(),
            "--transcript".into(),
            "1:normal".into(),
        ];
        let (bundle, remaining) = parse_inline_bundle_flags(&argv).unwrap();
        assert!(bundle.is_some());
        let b = bundle.unwrap();
        assert_eq!(b["title"], "Test");
        assert_eq!(b["description"], "Desc");
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_parse_inline_flags_without_title() {
        let argv: Vec<String> = vec!["--description".into(), "Desc".into()];
        let err = parse_inline_bundle_flags(&argv).unwrap_err();
        assert!(err.contains("require --title"));
    }

    #[test]
    fn test_parse_inline_duplicate_flag() {
        let argv: Vec<String> = vec!["--title".into(), "A".into(), "--title".into(), "B".into()];
        let err = parse_inline_bundle_flags(&argv).unwrap_err();
        assert!(err.contains("Duplicate flag"));
    }
}
