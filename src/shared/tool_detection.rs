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

#[derive(Debug)]
pub struct ToolDetectionRule {
    pub tool: Tool,
    pub predicates: &'static [EnvPredicate],
    pub clear_for_child: &'static [&'static str],
}

const CLAUDE_NATIVE: &[EnvPredicate] = &[
    EnvPredicate {
        var: "CLAUDECODE",
        condition: EnvMatch::Equals("1"),
    },
    EnvPredicate {
        var: "CLAUDE_ENV_FILE",
        condition: EnvMatch::NonEmpty,
    },
];
const ANTIGRAVITY_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "ANTIGRAVITY_AGENT",
    condition: EnvMatch::Set,
}];
const GEMINI_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "GEMINI_CLI",
    condition: EnvMatch::Equals("1"),
}];
const CODEX_NATIVE: &[EnvPredicate] = &[
    EnvPredicate {
        var: "CODEX_SANDBOX",
        condition: EnvMatch::Set,
    },
    EnvPredicate {
        var: "CODEX_SANDBOX_NETWORK_DISABLED",
        condition: EnvMatch::Set,
    },
    EnvPredicate {
        var: "CODEX_MANAGED_BY_NPM",
        condition: EnvMatch::Set,
    },
    EnvPredicate {
        var: "CODEX_MANAGED_BY_BUN",
        condition: EnvMatch::Set,
    },
    EnvPredicate {
        var: "CODEX_THREAD_ID",
        condition: EnvMatch::Set,
    },
];
const OPENCODE_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "OPENCODE",
    condition: EnvMatch::Equals("1"),
}];
const KILO_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "KILO",
    condition: EnvMatch::Equals("1"),
}];
const CURSOR_NATIVE: &[EnvPredicate] = &[
    EnvPredicate {
        var: "CURSOR_AGENT",
        condition: EnvMatch::Set,
    },
    EnvPredicate {
        var: "CURSOR_PROJECT_DIR",
        condition: EnvMatch::Set,
    },
];
const KIMI_NATIVE: &[EnvPredicate] = &[
    EnvPredicate {
        var: "KIMI_CODE_CLI",
        condition: EnvMatch::Equals("1"),
    },
    EnvPredicate {
        var: "KIMI_SESSION_ID",
        condition: EnvMatch::Set,
    },
];
const PI_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "HCOM_PI",
    condition: EnvMatch::Equals("1"),
}];
const OMP_NATIVE: &[EnvPredicate] = &[EnvPredicate {
    var: "HCOM_OMP",
    condition: EnvMatch::Equals("1"),
}];

macro_rules! hcom_tool_predicate {
    ($name:literal, $ident:ident) => {
        const $ident: &[EnvPredicate] = &[EnvPredicate {
            var: "HCOM_TOOL",
            condition: EnvMatch::Equals($name),
        }];
    };
}

hcom_tool_predicate!("claude", HCOM_TOOL_CLAUDE);
hcom_tool_predicate!("antigravity", HCOM_TOOL_ANTIGRAVITY);
hcom_tool_predicate!("gemini", HCOM_TOOL_GEMINI);
hcom_tool_predicate!("codex", HCOM_TOOL_CODEX);
hcom_tool_predicate!("opencode", HCOM_TOOL_OPENCODE);
hcom_tool_predicate!("kilo", HCOM_TOOL_KILO);
hcom_tool_predicate!("cursor", HCOM_TOOL_CURSOR);
hcom_tool_predicate!("kimi", HCOM_TOOL_KIMI);
hcom_tool_predicate!("copilot", HCOM_TOOL_COPILOT);
hcom_tool_predicate!("pi", HCOM_TOOL_PI);
hcom_tool_predicate!("omp", HCOM_TOOL_OMP);

/// Detection precedence: native markers first, then hcom's explicit fallback.
pub static TOOL_DETECTION_RULES: &[ToolDetectionRule] = &[
    ToolDetectionRule {
        tool: Tool::Claude,
        predicates: CLAUDE_NATIVE,
        clear_for_child: &["CLAUDECODE", "CLAUDE_ENV_FILE"],
    },
    ToolDetectionRule {
        tool: Tool::Antigravity,
        predicates: ANTIGRAVITY_NATIVE,
        clear_for_child: &["ANTIGRAVITY_AGENT"],
    },
    ToolDetectionRule {
        tool: Tool::Gemini,
        predicates: GEMINI_NATIVE,
        clear_for_child: &["GEMINI_CLI", "GEMINI_SYSTEM_MD"],
    },
    ToolDetectionRule {
        tool: Tool::Codex,
        predicates: CODEX_NATIVE,
        clear_for_child: &[
            "CODEX_SANDBOX",
            "CODEX_SANDBOX_NETWORK_DISABLED",
            "CODEX_MANAGED_BY_NPM",
            "CODEX_MANAGED_BY_BUN",
            "CODEX_THREAD_ID",
        ],
    },
    ToolDetectionRule {
        tool: Tool::OpenCode,
        predicates: OPENCODE_NATIVE,
        clear_for_child: &["OPENCODE"],
    },
    ToolDetectionRule {
        tool: Tool::Kilo,
        predicates: KILO_NATIVE,
        clear_for_child: &["KILO"],
    },
    ToolDetectionRule {
        tool: Tool::Cursor,
        predicates: CURSOR_NATIVE,
        clear_for_child: &["CURSOR_AGENT", "CURSOR_PROJECT_DIR"],
    },
    ToolDetectionRule {
        tool: Tool::Kimi,
        predicates: KIMI_NATIVE,
        clear_for_child: &["KIMI_CODE_CLI", "KIMI_SESSION_ID"],
    },
    ToolDetectionRule {
        tool: Tool::Pi,
        predicates: PI_NATIVE,
        clear_for_child: &["HCOM_PI", "PI_CODING_AGENT", "PI_CODING_AGENT_SESSION_DIR"],
    },
    ToolDetectionRule {
        tool: Tool::Omp,
        predicates: OMP_NATIVE,
        clear_for_child: &["HCOM_OMP", "PI_CODING_AGENT", "PI_CODING_AGENT_SESSION_DIR"],
    },
    ToolDetectionRule {
        tool: Tool::Claude,
        predicates: HCOM_TOOL_CLAUDE,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Antigravity,
        predicates: HCOM_TOOL_ANTIGRAVITY,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Gemini,
        predicates: HCOM_TOOL_GEMINI,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Codex,
        predicates: HCOM_TOOL_CODEX,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::OpenCode,
        predicates: HCOM_TOOL_OPENCODE,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Kilo,
        predicates: HCOM_TOOL_KILO,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Cursor,
        predicates: HCOM_TOOL_CURSOR,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Kimi,
        predicates: HCOM_TOOL_KIMI,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Copilot,
        predicates: HCOM_TOOL_COPILOT,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Pi,
        predicates: HCOM_TOOL_PI,
        clear_for_child: &["HCOM_TOOL"],
    },
    ToolDetectionRule {
        tool: Tool::Omp,
        predicates: HCOM_TOOL_OMP,
        clear_for_child: &["HCOM_TOOL"],
    },
];

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
    TOOL_DETECTION_RULES
        .iter()
        .find(|rule| {
            rule.predicates
                .iter()
                .any(|predicate| predicate_matches(env, predicate))
        })
        .map(|rule| rule.tool)
        .unwrap_or(Tool::Adhoc)
}

pub fn tool_marker_vars() -> &'static [&'static str] {
    static VARS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        let mut seen = HashSet::new();
        let mut vars = Vec::new();
        for rule in TOOL_DETECTION_RULES {
            for var in rule
                .predicates
                .iter()
                .map(|predicate| predicate.var)
                .chain(rule.clear_for_child.iter().copied())
            {
                if seen.insert(var) {
                    vars.push(var);
                }
            }
        }
        vars
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
    fn every_detection_var_is_cleared_for_children() {
        let clear: HashSet<&str> = tool_marker_vars().iter().copied().collect();
        for rule in TOOL_DETECTION_RULES {
            for predicate in rule.predicates {
                assert!(
                    clear.contains(predicate.var),
                    "{} detection marker must be cleared for child processes",
                    predicate.var
                );
            }
        }
    }

    #[test]
    fn previously_missing_markers_are_in_child_clear_set() {
        assert!(tool_marker_vars().contains(&"CLAUDE_ENV_FILE"));
        assert!(tool_marker_vars().contains(&"HCOM_TOOL"));
    }
}
