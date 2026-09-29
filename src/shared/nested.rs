//! Keep hooks of an agent nested under an hcom-launched agent from acting as it.
//!
//! An hcom-launched agent exports `HCOM_PROCESS_ID` to every shell command it
//! runs. Another agent started from those shells as a plain sub-worker
//! (`codex exec`, `opencode run`, ...) inherits it, and if that agent loads hcom
//! hooks (persistent install, or a runtime injection var it also inherited),
//! its hooks would bind to and act as the parent.
//!
//! A hook knows its tool from the hook command. It belongs to a nested agent if
//! - that tool differs from the launched `HCOM_TOOL`, or
//! - its env carries the tool's shell-only markers, which only an agent started
//!   from that tool's shell inherits (the launcher strips them).
//!
//! Detected hooks have the inherited identity scrubbed from the process env at
//! startup, so they run as a plain unlaunched session.
//!
//! Supported: any cross-tool child, and same-tool Codex and Gemini children.
//! Per-run tools (Claude, Codex, Copilot, Pi, Omp, OpenCode, Kilo; see
//! `hooks::runtime`) load hcom only through launch args or env, so a plain
//! same-tool child doesn't load hcom at all (OpenCode's inherited env var is
//! made inert by the plugin's owner-PID guard). Persistent Cursor, Kimi and
//! Antigravity still lack same-tool detection.

use std::collections::HashMap;

use crate::shared::tool_detection::TOOL_DETECTION_RULES;
use crate::tool::Tool;

/// Vars a tool sets only for its shell commands, never for its own process or
/// hooks.
fn shell_only_markers(tool: Tool) -> &'static [&'static str] {
    match tool {
        // Verified: codex hooks get neither; its shell commands get both.
        Tool::Codex => &["CODEX_SESSION_ID", "CODEX_THREAD_ID"],
        // gemini-cli sets it only in shellExecutionService and MCP server envs.
        Tool::Gemini => &["GEMINI_CLI"],
        _ => &[],
    }
}

/// Whether an agent launched as `parent` runs `hook_tool`'s hooks itself.
fn runs_hooks_of(parent: Tool, hook_tool: Tool, env: &HashMap<String, String>) -> bool {
    match (parent, hook_tool) {
        _ if parent == hook_tool => true,
        // Kilo shares OpenCode's hook names. Kilo sets KILO_PID; only a real
        // OpenCode sets OPENCODE_PID.
        (Tool::Kilo, Tool::OpenCode) => !env.contains_key("OPENCODE_PID"),
        // Antigravity shares Gemini's hook names but never sets GEMINI_CLI
        // (verified); gemini-cli relaunches itself with GEMINI_CLI_NO_RELAUNCH.
        (Tool::Antigravity, Tool::Gemini) => {
            !env.contains_key("GEMINI_CLI") && !env.contains_key("GEMINI_CLI_NO_RELAUNCH")
        }
        _ => false,
    }
}

/// A hook serving an agent nested under an hcom-launched agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedAgent {
    pub parent: Tool,
    pub child: Tool,
}

/// Detect a `hook_tool` hook running for an agent nested under the launched one.
///
/// Only applies when a launch identity was inherited (`HCOM_PROCESS_ID` plus the
/// launcher's `HCOM_TOOL`).
pub fn detect(env: &HashMap<String, String>, hook_tool: Tool) -> Option<NestedAgent> {
    env.get("HCOM_PROCESS_ID").filter(|v| !v.is_empty())?;
    let parent: Tool = env.get("HCOM_TOOL")?.parse().ok()?;
    let nested = !runs_hooks_of(parent, hook_tool, env)
        || (hook_tool == parent
            && shell_only_markers(hook_tool)
                .iter()
                .any(|var| env.contains_key(*var)));
    nested.then_some(NestedAgent {
        parent,
        child: hook_tool,
    })
}

/// Env vars carrying the parent's identity, to drop from a nested agent's view.
fn inherited_identity_vars(nested: NestedAgent) -> Vec<&'static str> {
    let mut vars: Vec<&'static str> = crate::shared::constants::HCOM_IDENTITY_VARS.to_vec();
    vars.extend([
        "HCOM_TOOL",
        "HCOM_INSTANCE_NAME",
        "HCOM_LAUNCHED_PRESET",
        crate::claude_actor::ENV_VAR,
        crate::claude_actor::SESSION_ENV_VAR,
    ]);
    if nested.parent == Tool::Claude && nested.child != Tool::Claude {
        // Inherited from the parent's Bash; a Claude child sets its own.
        vars.push("CLAUDE_CODE_SESSION_ID");
    }
    if nested.parent == nested.child {
        // The parent's shell-only markers carry the parent's session.
        vars.extend(shell_only_markers(nested.parent));
    } else {
        for rule in TOOL_DETECTION_RULES
            .iter()
            .filter(|r| r.tool == nested.parent)
        {
            vars.extend(rule.predicates.iter().map(|p| p.var));
            vars.extend(rule.clear_for_child.iter().copied());
        }
    }
    vars.extend(nested.parent.spec().instance_state_env);
    vars
}

/// Drop the parent's inherited identity from `env` if this hook serves a nested
/// agent. Returns what was detected.
pub fn scrub_env(env: &mut HashMap<String, String>, hook_tool: Tool) -> Option<NestedAgent> {
    let nested = detect(env, hook_tool)?;
    for var in inherited_identity_vars(nested) {
        env.remove(var);
    }
    Some(nested)
}

/// Apply [`scrub_env`] to this process's environment.
///
/// Must run at startup before any thread is spawned or env is read, so every
/// later reader (context, config, hook handlers) sees the child's own view.
pub fn scrub_inherited_identity(hook_tool: Tool) -> Option<NestedAgent> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let before: Vec<String> = env.keys().cloned().collect();
    let nested = scrub_env(&mut env, hook_tool)?;
    for var in before.iter().filter(|var| !env.contains_key(*var)) {
        // SAFETY: called from main() before any other thread exists.
        unsafe { std::env::remove_var(var) };
    }
    Some(nested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn launched(tool: &str, extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut e = env(&[
            ("HCOM_PROCESS_ID", "pid-1"),
            ("HCOM_LAUNCHED", "1"),
            ("HCOM_TOOL", tool),
        ]);
        e.extend(env(extra));
        e
    }

    #[test]
    fn parent_own_hooks_are_not_nested() {
        assert_eq!(
            detect(&launched("claude", &[("CLAUDECODE", "1")]), Tool::Claude),
            None
        );
        assert_eq!(detect(&launched("codex", &[]), Tool::Codex), None);
        assert_eq!(detect(&launched("gemini", &[]), Tool::Gemini), None);
    }

    #[test]
    fn requires_inherited_launch_identity() {
        let vanilla = env(&[("CLAUDECODE", "1"), ("CODEX_SESSION_ID", "s")]);
        assert_eq!(detect(&vanilla, Tool::Codex), None);
        let no_baseline = env(&[("HCOM_PROCESS_ID", "p")]);
        assert_eq!(detect(&no_baseline, Tool::Codex), None);
    }

    #[test]
    fn hook_of_another_tool_is_nested_and_scrubbed() {
        let mut e = launched(
            "claude",
            &[
                ("CLAUDECODE", "1"),
                ("CLAUDE_ENV_FILE", "/x"),
                ("CLAUDE_CODE_SESSION_ID", "parent"),
                ("HCOM_CLAUDE_ACTOR", "tok"),
                ("HCOM_CLAUDE_ACTOR_SESSION", "parent"),
                ("HCOM_DIR", "/h"),
            ],
        );
        assert_eq!(
            scrub_env(&mut e, Tool::Codex),
            Some(NestedAgent {
                parent: Tool::Claude,
                child: Tool::Codex
            })
        );
        for gone in [
            "HCOM_PROCESS_ID",
            "HCOM_LAUNCHED",
            "HCOM_TOOL",
            "CLAUDECODE",
            "CLAUDE_ENV_FILE",
            "CLAUDE_CODE_SESSION_ID",
            "HCOM_CLAUDE_ACTOR",
            "HCOM_CLAUDE_ACTOR_SESSION",
        ] {
            assert!(!e.contains_key(gone), "{gone} should be scrubbed");
        }
        assert_eq!(e.get("HCOM_DIR").map(String::as_str), Some("/h"));
        let ctx = crate::shared::HcomContext::from_env(&e, "/tmp".into());
        assert_eq!(ctx.process_id, None);
        assert!(!ctx.is_launched);
    }

    #[test]
    fn same_tool_hook_with_parent_shell_markers_is_nested() {
        let mut e = launched(
            "codex",
            &[("CODEX_SESSION_ID", "parent"), ("CODEX_THREAD_ID", "t")],
        );
        assert!(scrub_env(&mut e, Tool::Codex).is_some());
        for gone in ["HCOM_PROCESS_ID", "CODEX_SESSION_ID", "CODEX_THREAD_ID"] {
            assert!(!e.contains_key(gone), "{gone} should be scrubbed");
        }
        assert!(detect(&launched("gemini", &[("GEMINI_CLI", "1")]), Tool::Gemini).is_some());
    }

    #[test]
    fn fork_parents_run_upstream_hooks_but_real_upstream_children_are_nested() {
        let kilo = [("KILO", "1"), ("OPENCODE", "1"), ("KILO_PID", "1")];
        assert_eq!(detect(&launched("kilo", &kilo), Tool::OpenCode), None);
        let mut opencode_child = launched("kilo", &kilo);
        opencode_child.insert("OPENCODE_PID".into(), "2".into());
        assert!(detect(&opencode_child, Tool::OpenCode).is_some());

        let agy = [("ANTIGRAVITY_AGENT", "1")];
        assert_eq!(detect(&launched("antigravity", &agy), Tool::Gemini), None);
        let mut gemini_child = launched("antigravity", &agy);
        gemini_child.insert("GEMINI_CLI_NO_RELAUNCH".into(), "true".into());
        assert!(detect(&gemini_child, Tool::Gemini).is_some());
    }
}
