//! OpenCode launch preprocessing — sets environment variables for hcom integration.
//! Plugin management is handled separately in hooks/opencode.rs.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// OpenCode 2 hosts plugins in a shared background service that never sees
/// hcom's launch env; `--standalone` runs a private server per launch instead.
const STANDALONE_FLAG: &str = "--standalone";
/// OpenCode 2's TUI has no `--fork`; the fork happens server-side instead.
const FORK_FLAG: &str = "--fork";
const SESSION_FLAG: &str = "--session";
/// OpenCode 2's TUI has no `--agent`/`--model`; the plugin applies them as the
/// server's defaults from these env vars.
const AGENT_FLAGS: &[&str] = &["--agent"];
const MODEL_FLAGS: &[&str] = &["--model", "-m"];
const AGENT_ENV: &str = "HCOM_OPENCODE_AGENT";
const MODEL_ENV: &str = "HCOM_OPENCODE_MODEL";
const OPENCODE_2_MAJOR: u64 = 2;

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

/// Preprocess OpenCode launch args for OpenCode 2+: move `--agent`/`--model` into
/// `env`, fork a `--session <id> --fork` through the server API, then add `--standalone`.
pub fn preprocess_opencode_args(
    args: &[String],
    env: &mut HashMap<String, String>,
    cwd: &Path,
) -> Result<Vec<String>> {
    if !is_opencode_2() {
        return Ok(args.to_vec());
    }
    let (args, agent) = take_value_arg(args, AGENT_FLAGS)?;
    let (args, model) = take_value_arg(&args, MODEL_FLAGS)?;
    if let Some(ref model) = model {
        validate_model_arg(model)?;
    }
    reject_foreign_server(&args)?;
    for (key, value) in [(AGENT_ENV, agent), (MODEL_ENV, model)] {
        if let Some(value) = value {
            env.insert(key.to_string(), value);
        }
    }
    let server = [STANDALONE_FLAG.to_string()];
    let args = fork_session_server_side(&args, |id| fork_session(id, &server, cwd))?;
    Ok(add_standalone(&args))
}

/// Removes every `--flag value` / `--flag=value` for `flags`; returns the last value.
fn take_value_arg(args: &[String], flags: &[&str]) -> Result<(Vec<String>, Option<String>)> {
    let mut kept = Vec::with_capacity(args.len());
    let mut value = None;
    let mut tokens = args.iter().peekable();
    while let Some(token) = tokens.next() {
        if flags.contains(&token.as_str()) {
            let Some(next) = tokens.next_if(|next| !next.starts_with('-')) else {
                bail!("{token} needs a value");
            };
            if next.trim().is_empty() {
                bail!("{token} needs a value");
            }
            value = Some(next.clone());
        } else if let Some(inline) = flags
            .iter()
            .find_map(|flag| token.strip_prefix(&format!("{flag}=")))
        {
            if inline.trim().is_empty() {
                bail!("{token} needs a value");
            }
            value = Some(inline.to_string());
        } else {
            kept.push(token.clone());
        }
    }
    Ok((kept, value))
}

fn validate_model_arg(model: &str) -> Result<()> {
    let (provider, rest) = model.split_once('/').with_context(|| {
        format!("invalid OpenCode model '{model}': expected provider/model[#variant]")
    })?;
    let (model_id, variant) = match rest.split_once('#') {
        Some((id, variant)) => (id, Some(variant)),
        None => (rest, None),
    };
    if provider.is_empty()
        || provider.contains('#')
        || model_id.is_empty()
        || model.chars().any(char::is_whitespace)
        || variant.is_some_and(|variant| variant.is_empty() || variant.contains('#'))
    {
        bail!("invalid OpenCode model '{model}': expected provider/model[#variant]");
    }
    Ok(())
}

/// hcom's plugin comes from this launch's env (per-run), which only a server
/// hcom starts itself can see, so `--server <url>` would run without hooks.
fn reject_foreign_server(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| arg == "--server" || arg.starts_with("--server="))
    {
        bail!(
            "--server is not supported with hcom on OpenCode 2: an existing server never \
             loads hcom's plugin. Drop --server; hcom runs a private --standalone server."
        );
    }
    Ok(())
}

fn add_standalone(args: &[String]) -> Vec<String> {
    let mut result = args.to_vec();
    if !args.iter().any(|arg| arg == STANDALONE_FLAG) {
        result.insert(0, STANDALONE_FLAG.to_string());
    }
    result
}

/// Replaces `--session <id> --fork` with `--session <fork of id>`.
fn fork_session_server_side(
    args: &[String],
    fork: impl FnOnce(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let Some(fork_at) = args.iter().position(|arg| arg == FORK_FLAG) else {
        return Ok(args.to_vec());
    };
    let Some(session_at) = args.iter().position(|arg| arg == SESSION_FLAG) else {
        bail!("{FORK_FLAG} needs {SESSION_FLAG} <id>");
    };
    let source = args
        .get(session_at + 1)
        .with_context(|| format!("{SESSION_FLAG} needs a session id"))?;
    let forked = fork(source)?;
    let mut result = args.to_vec();
    result[session_at + 1] = forked;
    result.remove(fork_at);
    Ok(result)
}

/// Forks on `server`, the one the launched TUI connects to.
fn fork_session(session_id: &str, server: &[String], cwd: &Path) -> Result<String> {
    let output = crate::terminal::executable_command("opencode")
        .arg("api")
        .args(server)
        .arg("POST")
        .arg(format!("/api/session/{session_id}/fork"))
        .args(["--data", "{}"])
        .current_dir(cwd)
        .output()
        .context("could not run opencode api to fork the session")?;
    if !output.status.success() {
        bail!(
            "opencode could not fork session {session_id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("opencode fork response is not JSON")?;
    response["data"]["id"]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("opencode fork response has no session id: {response}"))
}

fn parse_opencode_major_version(output: &str) -> Option<u64> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|token| token.matches('.').count() >= 2)
        .and_then(|token| token.split('.').next()?.parse().ok())
}

fn is_opencode_2() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let output = match crate::terminal::executable_command("opencode")
            .arg("--version")
            .output()
        {
            Ok(output) => output,
            Err(e) => {
                crate::log::log_warn(
                    "opencode",
                    "opencode.version_failed",
                    &format!("could not run opencode --version; assuming OpenCode 1: {e}"),
                );
                return false;
            }
        };
        parse_opencode_major_version(&String::from_utf8_lossy(&output.stdout))
            .is_some_and(|major| major >= OPENCODE_2_MAJOR)
    })
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
    fn test_parse_opencode_major_version() {
        assert_eq!(parse_opencode_major_version("opencode v2.0.15\n"), Some(2));
        assert_eq!(parse_opencode_major_version("1.14.48"), Some(1));
        assert_eq!(parse_opencode_major_version("opencode"), None);
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn test_add_standalone() {
        assert_eq!(
            add_standalone(&strings(&["--model", "a/b"])),
            ["--standalone", "--model", "a/b"]
        );
    }

    #[test]
    fn test_reject_foreign_server() {
        assert!(reject_foreign_server(&strings(&["--server", "http://x"])).is_err());
        assert!(reject_foreign_server(&strings(&["--server=http://x"])).is_err());
        assert!(reject_foreign_server(&strings(&["--standalone"])).is_ok());
        assert!(reject_foreign_server(&strings(&["--model", "a/b"])).is_ok());
    }

    #[test]
    fn test_add_standalone_keeps_existing_flag() {
        let args = strings(&["--standalone", "--model", "a/b"]);
        assert_eq!(add_standalone(&args), args);
    }

    #[test]
    fn test_take_value_arg_removes_both_forms() {
        let args = strings(&["--agent", "qa", "--session", "ses_src", "-m=google/x"]);
        let (rest, agent) = take_value_arg(&args, AGENT_FLAGS).unwrap();
        assert_eq!(agent.as_deref(), Some("qa"));
        let (rest, model) = take_value_arg(&rest, MODEL_FLAGS).unwrap();
        assert_eq!(model.as_deref(), Some("google/x"));
        assert_eq!(rest, ["--session", "ses_src"]);
    }

    #[test]
    fn test_take_value_arg_absent() {
        let args = strings(&["--session", "ses_src"]);
        assert_eq!(
            take_value_arg(&args, AGENT_FLAGS).unwrap(),
            (args.clone(), None)
        );
    }

    #[test]
    fn test_take_value_arg_rejects_missing_value() {
        for args in [
            strings(&["--model", "--session", "ses_src"]),
            strings(&["--agent"]),
            strings(&["--model="]),
            strings(&["--agent="]),
            strings(&["--model", ""]),
            strings(&["--agent", " "]),
        ] {
            assert!(take_value_arg(&args, &["--model", "--agent"]).is_err());
        }
    }

    #[test]
    fn test_validate_model_arg() {
        for model in ["opencode/big-pickle", "anthropic/claude-sonnet-4-6#fast"] {
            assert!(validate_model_arg(model).is_ok());
        }
        for model in [
            "invalid",
            "/model",
            "provider/",
            "provider/model#",
            "provider/model#a#b",
            "provider#bad/model",
            "provider/model name",
        ] {
            assert!(validate_model_arg(model).is_err());
        }
    }

    #[test]
    fn test_fork_session_server_side_continues_the_fork() {
        let args = strings(&["--model", "a/b", "--session", "ses_src", "--fork"]);
        let forked = fork_session_server_side(&args, |id| {
            assert_eq!(id, "ses_src");
            Ok("ses_copy".to_string())
        })
        .unwrap();
        assert_eq!(forked, ["--model", "a/b", "--session", "ses_copy"]);
    }

    #[test]
    fn test_fork_session_server_side_leaves_plain_resume() {
        let args = strings(&["--session", "ses_src"]);
        let resumed = fork_session_server_side(&args, |_| panic!("no fork requested")).unwrap();
        assert_eq!(resumed, args);
    }

    #[test]
    fn test_fork_session_server_side_needs_a_session() {
        assert!(fork_session_server_side(&strings(&["--fork"]), |_| unreachable!()).is_err());
        let failed =
            fork_session_server_side(&strings(&["--session", "ses_src", "--fork"]), |_| {
                bail!("server down")
            });
        assert!(failed.is_err());
    }

    #[test]
    fn test_permission_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(&opencode_permission_json()).expect("valid JSON");
        let prefix = crate::runtime_env::build_hcom_command();
        assert!(parsed["bash"][format!("{prefix} send*")].is_string());
    }
}
