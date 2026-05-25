use std::collections::HashMap;

const KILO_PERMISSION_JSON: &str = r#"{"bash":{"hcom *":"allow"}}"#;

pub fn preprocess_kilo_env(env: &mut HashMap<String, String>, instance_name: &str) {
    env.insert(
        "KILO_PERMISSION".to_string(),
        KILO_PERMISSION_JSON.to_string(),
    );
    env.insert("HCOM_NAME".to_string(), instance_name.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_sets_permission() {
        let mut env = HashMap::new();
        preprocess_kilo_env(&mut env, "luna");
        let perm = env.get("KILO_PERMISSION").unwrap();
        assert!(perm.contains("hcom *"));
        assert!(perm.contains("allow"));
    }

    #[test]
    fn test_preprocess_sets_hcom_name() {
        let mut env = HashMap::new();
        preprocess_kilo_env(&mut env, "nova");
        assert_eq!(env.get("HCOM_NAME").unwrap(), "nova");
    }

    #[test]
    fn test_preprocess_overwrites_existing() {
        let mut env = HashMap::new();
        env.insert("HCOM_NAME".to_string(), "old".to_string());
        preprocess_kilo_env(&mut env, "nova");
        assert_eq!(env.get("HCOM_NAME").unwrap(), "nova");
    }

    #[test]
    fn test_permission_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(KILO_PERMISSION_JSON).expect("valid JSON");
        assert!(parsed["bash"]["hcom *"].is_string());
    }
}
