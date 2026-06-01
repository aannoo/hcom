//! OpenCode launch preprocessing — sets environment variables for hcom integration.
//! Plugin management is handled separately in hooks/opencode.rs.

use std::collections::HashMap;

fn opencode_permission_json() -> String {
    let prefix = crate::runtime_env::build_hcom_command();
    let bash = crate::hooks::common::SAFE_HCOM_COMMANDS
        .iter()
        .map(|command| (format!("{prefix} {command}*"), serde_json::json!("allow")))
        .collect();
    serde_json::Value::Object(serde_json::Map::from_iter([(
        "bash".to_string(),
        serde_json::Value::Object(bash),
    )]))
    .to_string()
}

/// Preprocess environment variables for an OpenCode-family launch.
///
/// Sets:
/// - App-specific permission override: Auto-approve safe hcom bash commands when enabled
/// - `HCOM_NAME`: Instance name for plugin diagnostics (set before identity binding)
pub fn preprocess_opencode_env(
    env: &mut HashMap<String, String>,
    tool: &str,
    instance_name: &str,
    auto_approve: bool,
) {
    if auto_approve {
        let key = if tool == "kilo" {
            "KILO_PERMISSION"
        } else {
            "OPENCODE_PERMISSION"
        };
        env.insert(key.to_string(), opencode_permission_json());
    }
    env.insert("HCOM_NAME".to_string(), instance_name.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_sets_permission() {
        let mut env = HashMap::new();
        preprocess_opencode_env(&mut env, "opencode", "luna", true);
        let perm = env.get("OPENCODE_PERMISSION").unwrap();
        let prefix = crate::runtime_env::build_hcom_command();
        assert!(perm.contains(&format!("{prefix} send*")));
        assert!(!perm.contains(&format!("\"{prefix} *\"")));
        assert!(!perm.contains("hcom kill"));
    }

    #[test]
    fn test_preprocess_skips_permission_when_disabled() {
        let mut env = HashMap::new();
        preprocess_opencode_env(&mut env, "opencode", "luna", false);
        assert!(!env.contains_key("OPENCODE_PERMISSION"));
    }

    #[test]
    fn test_preprocess_sets_hcom_name() {
        let mut env = HashMap::new();
        preprocess_opencode_env(&mut env, "opencode", "nova", true);
        assert_eq!(env.get("HCOM_NAME").unwrap(), "nova");
    }

    #[test]
    fn test_preprocess_overwrites_existing() {
        let mut env = HashMap::new();
        env.insert("HCOM_NAME".to_string(), "old".to_string());
        preprocess_opencode_env(&mut env, "opencode", "nova", true);
        assert_eq!(env.get("HCOM_NAME").unwrap(), "nova");
    }

    #[test]
    fn test_preprocess_kilo_sets_kilo_permission() {
        let mut env = HashMap::new();
        preprocess_opencode_env(&mut env, "kilo", "luna", true);
        assert!(env.contains_key("KILO_PERMISSION"));
        assert!(!env.contains_key("OPENCODE_PERMISSION"));
    }

    #[test]
    fn test_permission_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(&opencode_permission_json()).expect("valid JSON");
        let prefix = crate::runtime_env::build_hcom_command();
        assert!(parsed["bash"][format!("{prefix} send*")].is_string());
    }
}
