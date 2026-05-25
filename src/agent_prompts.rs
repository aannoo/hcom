use std::path::PathBuf;

const AGENTS_DIR: &str = "agents";

fn get_agents_dir() -> PathBuf {
    let hcom_dir = crate::config::Config::get().hcom_dir;
    hcom_dir.join(AGENTS_DIR)
}

pub fn get_agent_prompt_path(instance_name: &str) -> PathBuf {
    get_agents_dir().join(format!("{}.md", instance_name))
}

pub fn load_agent_prompt(instance_name: &str) -> Option<String> {
    let path = get_agent_prompt_path(instance_name);
    if path.exists() {
        std::fs::read_to_string(&path).ok().filter(|s| !s.trim().is_empty())
    } else {
        None
    }
}

pub fn ensure_agents_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(get_agents_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.md");
        assert!(!path.exists());
    }

    #[test]
    fn test_ensure_agents_dir_creates() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("agents");
        assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());
    }
}
