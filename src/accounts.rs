//! Logged-in account e-mail for a tool's config directory. Only the e-mail
//! leaves this module; tokens are parsed in place and dropped.
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::tool::Tool;

/// Every `config_dir_env_key`.
pub const CONFIG_DIR_ENV_KEYS: [&str; 2] = ["CLAUDE_CONFIG_DIR", "CODEX_HOME"];

/// Env var that points a tool at a non-default config directory.
pub fn config_dir_env_key(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("CLAUDE_CONFIG_DIR"),
        Tool::Codex => Some("CODEX_HOME"),
        _ => None,
    }
}

pub fn account_file(tool: Tool, config_dir: Option<&Path>) -> Option<PathBuf> {
    let home = crate::runtime_env::user_home;
    match tool {
        Tool::Claude => Some(match config_dir {
            Some(d) => d.join(".claude.json"),
            None => home()?.join(".claude.json"),
        }),
        Tool::Codex => Some(match config_dir {
            Some(d) => d.join("auth.json"),
            None => home()?.join(".codex").join("auth.json"),
        }),
        _ => None,
    }
}

pub type Stamp = Option<(u64, Option<SystemTime>)>;

pub fn file_stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()))
}

pub fn read_email(tool: Tool, config_dir: Option<&Path>) -> Option<String> {
    let path = account_file(tool, config_dir)?;
    let text = std::fs::read_to_string(path).ok()?;
    match tool {
        Tool::Claude => parse_claude_email(&text),
        Tool::Codex => parse_codex_email(&text),
        _ => None,
    }
}

fn parse_claude_email(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// ChatGPT logins carry the e-mail in the id_token; API-key logins have none.
fn parse_codex_email(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v.get("auth_mode").and_then(|m| m.as_str()) != Some("chatgpt") {
        return None;
    }
    jwt_payload_email(v.get("tokens")?.get("id_token")?.as_str()?)
}

fn jwt_payload_email(id_token: &str) -> Option<String> {
    use base64::Engine;
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("email")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Re-reads an account file only when its size or mtime changes;
/// `.claude.json` can be large.
#[derive(Default)]
pub struct EmailCache {
    entries: Vec<(PathBuf, Stamp, Option<String>)>,
}

impl EmailCache {
    pub fn email(&mut self, tool: Tool, config_dir: Option<&Path>) -> Option<String> {
        let path = account_file(tool, config_dir)?;
        let stamp = file_stamp(&path);
        if let Some((_, s, e)) = self.entries.iter().find(|(p, ..)| *p == path)
            && *s == stamp
        {
            return e.clone();
        }
        let email = read_email(tool, config_dir);
        self.entries.retain(|(p, ..)| *p != path);
        if self.entries.len() >= 32 {
            self.entries.remove(0);
        }
        self.entries.push((path, stamp, email.clone()));
        email
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fake_jwt(payload: &str) -> String {
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("eyJhbGciOiJub25lIn0.{p}.sig")
    }

    #[test]
    fn claude_email() {
        assert_eq!(
            parse_claude_email(r#"{"oauthAccount":{"emailAddress":"a@b.com"}}"#).as_deref(),
            Some("a@b.com")
        );
        assert_eq!(parse_claude_email(r#"{"other":1}"#), None);
        assert_eq!(parse_claude_email("{broken"), None);
    }

    #[test]
    fn codex_email() {
        let auth = |mode: &str, tok: &str| {
            format!(r#"{{"auth_mode":"{mode}","tokens":{{"id_token":"{tok}"}}}}"#)
        };
        let tok = fake_jwt(r#"{"email":"c@d.com"}"#);
        assert_eq!(
            parse_codex_email(&auth("chatgpt", &tok)).as_deref(),
            Some("c@d.com")
        );
        assert_eq!(parse_codex_email(&auth("apikey", &tok)), None);
        assert_eq!(parse_codex_email(&auth("chatgpt", "a.b")), None);
        assert_eq!(parse_codex_email(&auth("chatgpt", "a.b.c.d")), None);
        // Trailing garbage after the JSON is rejected.
        let padded = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE.encode(r#"{"email":"p@q.com"}x"#)
        );
        assert_eq!(jwt_payload_email(&padded), None);
        // Padded payloads decode too.
        let padded = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE.encode(r#"{"email":"p@q.com"}"#)
        );
        assert_eq!(jwt_payload_email(&padded).as_deref(), Some("p@q.com"));
    }

    #[test]
    fn files_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            account_file(Tool::Claude, Some(dir.path())),
            Some(dir.path().join(".claude.json"))
        );
        assert_eq!(account_file(Tool::Gemini, Some(dir.path())), None);
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, r#"{"oauthAccount":{"emailAddress":"a@b.com"}}"#).unwrap();
        let mut cache = EmailCache::default();
        assert_eq!(
            cache.email(Tool::Claude, Some(dir.path())).as_deref(),
            Some("a@b.com")
        );
        std::fs::write(&path, r#"{"oauthAccount":{"emailAddress":"longer@b.com"}}"#).unwrap();
        assert_eq!(
            cache.email(Tool::Claude, Some(dir.path())).as_deref(),
            Some("longer@b.com")
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.email(Tool::Claude, Some(dir.path())), None);
    }
}
