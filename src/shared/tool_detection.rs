//! Canonical environment-based AI tool detection and child-env clearing.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::tool::Tool;

#[derive(Debug, Clone, Copy)]
pub enum EnvMatch {
    Set,
    NonEmpty,
    Equals(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct EnvPredicate {
    pub var: &'static str,
    pub condition: EnvMatch,
}

impl EnvPredicate {
    pub const fn set(var: &'static str) -> Self {
        Self {
            var,
            condition: EnvMatch::Set,
        }
    }

    pub const fn non_empty(var: &'static str) -> Self {
        Self {
            var,
            condition: EnvMatch::NonEmpty,
        }
    }

    pub const fn equals(var: &'static str, value: &'static str) -> Self {
        Self {
            var,
            condition: EnvMatch::Equals(value),
        }
    }
}

/// Order native markers are checked in. A tool started from another tool's
/// shell inherits that tool's markers, so the more specific one goes first:
/// Antigravity sets Gemini's markers too, and Hermes terminals pass theirs to
/// every agent started in them. Each tool's markers live in its spec.
pub static NATIVE_PRECEDENCE: &[Tool] = &[
    Tool::Claude,
    Tool::Antigravity,
    Tool::Gemini,
    Tool::Codex,
    Tool::OpenCode,
    Tool::Kilo,
    Tool::Cursor,
    Tool::Kimi,
    Tool::Qoder,
    Tool::Devin,
    Tool::Grok,
    Tool::Pi,
    Tool::Omp,
    Tool::Hermes,
];

/// hcom's own fallback marker, `HCOM_TOOL=<spec name>`.
pub const HCOM_TOOL: &str = "HCOM_TOOL";

fn predicate_matches(env: &HashMap<String, String>, predicate: &EnvPredicate) -> bool {
    match predicate.condition {
        EnvMatch::Set => env.contains_key(predicate.var),
        EnvMatch::NonEmpty => env
            .get(predicate.var)
            .is_some_and(|value| !value.is_empty()),
        EnvMatch::Equals(expected) => env
            .get(predicate.var)
            .is_some_and(|value| value == expected),
    }
}

pub fn detect_tool(env: &HashMap<String, String>) -> Tool {
    NATIVE_PRECEDENCE
        .iter()
        .copied()
        .find(|tool| {
            tool.spec()
                .detection
                .markers
                .iter()
                .any(|predicate| predicate_matches(env, predicate))
        })
        .or_else(|| {
            let name = env.get(HCOM_TOOL)?;
            crate::integration_spec::ALL
                .iter()
                .find(|spec| spec.tool != Tool::Adhoc && spec.name == name)
                .map(|spec| spec.tool)
        })
        .unwrap_or(Tool::Adhoc)
}

/// A tool's marker and child-clear vars, without `HCOM_TOOL`.
pub fn tool_env_vars(tool: Tool) -> impl Iterator<Item = &'static str> {
    let detection = tool.spec().detection;
    detection
        .markers
        .iter()
        .map(|predicate| predicate.var)
        .chain(detection.clear_for_child.iter().copied())
}

pub fn tool_marker_vars() -> &'static [&'static str] {
    static VARS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        let mut seen = HashSet::new();
        crate::integration_spec::ALL
            .iter()
            .flat_map(|spec| tool_env_vars(spec.tool))
            .chain([HCOM_TOOL])
            .filter(|var| seen.insert(*var))
            .collect()
    });
    VARS.as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn native_markers_beat_hcom_tool_fallback() {
        assert_eq!(
            detect_tool(&env(&[("GEMINI_CLI", "1"), ("HCOM_TOOL", "claude")])),
            Tool::Gemini
        );
    }

    #[test]
    fn antigravity_precedes_overlapping_gemini_marker() {
        assert_eq!(
            detect_tool(&env(&[("ANTIGRAVITY_AGENT", "1"), ("GEMINI_CLI", "1")])),
            Tool::Antigravity
        );
    }

    #[test]
    fn qoder_is_detected_by_its_shell_marker_not_by_claude_look_alikes() {
        assert_eq!(detect_tool(&env(&[("QODER_CLI", "1")])), Tool::Qoder);
        assert_eq!(detect_tool(&env(&[("HCOM_TOOL", "qoder")])), Tool::Qoder);
        // Qoder's hook processes carry CLAUDE_PROJECT_DIR; it is not a Claude marker.
        assert_eq!(
            detect_tool(&env(&[
                ("CLAUDE_PROJECT_DIR", "/w"),
                ("QODER_HOOK_SOURCE", "cli")
            ])),
            Tool::Adhoc
        );
        assert!(tool_marker_vars().contains(&"QODER_CLI"));
        assert!(tool_marker_vars().contains(&"QODER_PID"));
    }

    #[test]
    fn hermes_yields_to_agents_started_from_its_terminal() {
        let hermes = ("HERMES_SESSION_ID", "20261013_183101_ab25f9");
        assert_eq!(detect_tool(&env(&[hermes])), Tool::Hermes);
        assert_eq!(detect_tool(&env(&[("HCOM_TOOL", "hermes")])), Tool::Hermes);
        assert_eq!(detect_tool(&env(&[("HERMES_SESSION_ID", "")])), Tool::Adhoc);
        // Gateway terminals: empty session id, HERMES_AGENT still set.
        assert_eq!(
            detect_tool(&env(&[("HERMES_SESSION_ID", ""), ("HERMES_AGENT", "true")])),
            Tool::Hermes
        );
        // Claude/Codex inside a Hermes terminal inherit HERMES_SESSION_ID.
        assert_eq!(
            detect_tool(&env(&[hermes, ("CLAUDECODE", "1")])),
            Tool::Claude
        );
        assert_eq!(
            detect_tool(&env(&[hermes, ("CODEX_THREAD_ID", "t")])),
            Tool::Codex
        );
        // An hcom-launched agent's fallback marker loses to Hermes's own.
        assert_eq!(
            detect_tool(&env(&[hermes, ("HCOM_TOOL", "claude")])),
            Tool::Hermes
        );
        assert!(tool_marker_vars().contains(&"HERMES_AGENT"));
    }

    #[test]
    fn precedence_lists_every_tool_with_markers_once() {
        for spec in crate::integration_spec::ALL {
            let listed = NATIVE_PRECEDENCE
                .iter()
                .filter(|tool| **tool == spec.tool)
                .count();
            let expected = usize::from(!spec.detection.markers.is_empty());
            assert_eq!(
                listed, expected,
                "{}: NATIVE_PRECEDENCE must list each tool with markers once",
                spec.name
            );
        }
    }

    #[test]
    fn previously_missing_markers_are_in_child_clear_set() {
        assert!(tool_marker_vars().contains(&"CLAUDE_ENV_FILE"));
        assert!(tool_marker_vars().contains(&"HCOM_TOOL"));
    }
}
