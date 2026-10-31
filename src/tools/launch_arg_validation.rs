//! Delivery-model validation for tools without full argument parsers.

#[derive(Debug, Clone, Copy)]
pub(crate) struct RejectedArg {
    pub token: &'static str,
    pub reason: &'static str,
    pub kind: RejectedArgKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RejectedArgKind {
    Flag,
    RootSubcommand,
}

const GROK_LEADER_REASON: &str =
    "Grok ignores this flag in leader mode; configure it in Grok instead";

const GROK_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "--allow",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--deny",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--allowedTools",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--allowed-tools",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--disallowedTools",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--disable-web-search",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--no-memory",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--experimental-memory",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--storage-mode",
        reason: GROK_LEADER_REASON,
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--leader",
        reason: "hcom manages Grok's leader connection; remove this flag",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--no-leader",
        reason: "hcom manages Grok's leader connection; remove this flag",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--leader-socket",
        reason: "hcom manages Grok's leader connection; remove this flag",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--tools",
        reason: "Grok ignores this tool restriction in the TUI; configure it in Grok instead",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--disallowed-tools",
        reason: "Grok ignores this tool restriction in the TUI; configure it in Grok instead",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--agents",
        reason: "Grok only supports inline agent definitions in one-shot mode",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--max-turns",
        reason: "Grok ignores this turn limit in the TUI",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--memory-flush",
        reason: "Grok only supports this action in one-shot mode",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "-p",
        reason: "exits after one prompt; not a persistent hcom agent",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--single",
        reason: "exits after one prompt; not a persistent hcom agent",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--prompt-file",
        reason: "one-shot prompt from a file; not a persistent hcom agent",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--prompt-json",
        reason: "one-shot JSON prompt; not a persistent hcom agent",
        kind: RejectedArgKind::Flag,
    },
];

/// Validate Grok's persistent leader-backed TUI, excluding prompt text.
pub(crate) fn validate_grok_args(args: &[&str]) -> Vec<String> {
    args.iter()
        .copied()
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| {
            GROK_REJECTED_ARGS
                .iter()
                .find(|rule| long_flag_matches(arg, rule.token))
        })
        .map(|rule| {
            format!(
                "Grok argument `{}` is not supported by `hcom grok`: {}.",
                rule.token, rule.reason
            )
        })
        .collect()
}

pub(crate) const KIMI_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--prompt",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
];

pub(crate) const QODER_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--print",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--acp",
        reason: "starts an ACP server instead of the interactive TUI",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--remote",
        reason: "creates a cloud session and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--remote-control",
        reason: "starts a headless remote-control worker",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--list-sessions",
        reason: "lists sessions and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--delete-session",
        reason: "deletes a session and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--list-models",
        reason: "lists models instead of starting an interactive agent",
        kind: RejectedArgKind::Flag,
    },
];

const DEVIN_UTILITY_REASON: &str = "runs a utility subcommand instead of the interactive TUI";

pub(crate) const DEVIN_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--print",
        reason: "runs one prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--cloud",
        reason: "drives Devin Cloud sessions instead of the local agent",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "acp",
        reason: "starts an ACP server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "cloud",
        reason: "starts a Devin Cloud session instead of the local agent",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "desktop",
        reason: "opens Devin Desktop instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "auth",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "mcp",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "models",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "doctor",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "rules",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "skills",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "plugins",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "list",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "ls",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "rm",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "ssh",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "forward",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "update",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "version",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "migrate",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "sandbox",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "setup",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "uninstall",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "help",
        reason: DEVIN_UTILITY_REASON,
        kind: RejectedArgKind::RootSubcommand,
    },
];

pub(crate) const OPENCODE_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "run",
        reason: "starts the one-shot run subcommand instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "serve",
        reason: "starts a headless server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "acp",
        reason: "starts an ACP server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "web",
        reason: "starts a browser/server surface instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
];

pub(crate) const KILO_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "run",
        reason: "starts the one-shot run subcommand instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "serve",
        reason: "starts a headless server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "acp",
        reason: "starts an ACP server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
    RejectedArg {
        token: "remote",
        reason: "starts the remote relay surface instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
];

pub(crate) const PI_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--print",
        reason: "runs non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
];

pub(crate) const OMP_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--print",
        reason: "runs non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--mode",
        reason: "can disable the interactive extension host used by hcom delivery",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--no-extensions",
        reason: "prevents hcom's delivery extension from loading",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        // `omp acp` forces mode=acp and starts an ACP stdio server with no TUI
        // readiness marker — same class as OpenCode/Kilo `acp`. The `--mode`
        // flag rejection above does not cover it because `acp` is a root
        // subcommand, not a flag value.
        token: "acp",
        reason: "starts an ACP stdio server instead of the interactive TUI",
        kind: RejectedArgKind::RootSubcommand,
    },
];

pub(crate) const GEMINI_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs headless and exits before joining hcom; use -i/--prompt-interactive instead",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--prompt",
        reason: "runs headless and exits before joining hcom; use -i/--prompt-interactive instead",
        kind: RejectedArgKind::Flag,
    },
];

pub(crate) const ANTIGRAVITY_REJECTED_ARGS: &[RejectedArg] = &[
    RejectedArg {
        token: "-p",
        reason: "runs a single prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--print",
        reason: "runs a single prompt non-interactively and exits",
        kind: RejectedArgKind::Flag,
    },
    RejectedArg {
        token: "--prompt",
        reason: "aliases non-interactive print mode",
        kind: RejectedArgKind::Flag,
    },
];

/// Match a CLI token against a flag, accepting the `--flag=value` equals form
/// for long flags. Short flags (`-p`) and subcommand words match exactly only.
/// Shared so per-tool validators (cursor/copilot) reject `--print=…` the same
/// way the generic `validate_rejected_args` table does.
pub(crate) fn long_flag_matches(token: &str, rejected: &str) -> bool {
    token == rejected
        || (rejected.starts_with("--")
            && token
                .strip_prefix(rejected)
                .is_some_and(|suffix| suffix.starts_with('=')))
}

pub(crate) fn validate_rejected_args(
    tool: &str,
    invocation: &str,
    tokens: &[String],
    rejected: &[RejectedArg],
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if let Some(rule) = rejected.iter().find(|rule| {
            long_flag_matches(token, rule.token)
                && match rule.kind {
                    RejectedArgKind::Flag => true,
                    RejectedArgKind::RootSubcommand => index == 0,
                }
        }) {
            errors.push(format!(
                "{tool} argument `{}` is not supported by `{invocation}`: {}. Launch the hcom-managed interactive PTY instead.",
                rule.token, rule.reason
            ));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qoder_rejects_modes_without_an_interactive_agent() {
        for flag in [
            "--remote",
            "--remote=task",
            "--remote-control",
            "--list-sessions",
            "--delete-session",
            "--list-models",
        ] {
            assert_eq!(
                validate_rejected_args(
                    "Qoder",
                    "hcom qoder",
                    &[flag.to_string()],
                    QODER_REJECTED_ARGS
                )
                .len(),
                1,
                "{flag}"
            );
        }
    }

    #[test]
    fn devin_rejects_modes_without_an_interactive_agent() {
        for args in [
            vec!["-p"],
            vec!["--print=task"],
            vec!["--cloud"],
            vec!["acp"],
            vec!["cloud"],
            vec!["models"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert_eq!(
                validate_rejected_args("Devin", "hcom devin", &args, DEVIN_REJECTED_ARGS).len(),
                1,
                "{args:?}"
            );
        }
        // A subcommand name is only one in the subcommand position.
        let args: Vec<String> = ["--model", "models"].map(String::from).to_vec();
        assert!(
            validate_rejected_args("Devin", "hcom devin", &args, DEVIN_REJECTED_ARGS).is_empty()
        );
    }

    #[test]
    fn long_flags_match_equals_form() {
        let errors = validate_rejected_args(
            "Kimi",
            "hcom kimi",
            &["--prompt=task".to_string()],
            KIMI_REJECTED_ARGS,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--prompt"));
    }

    #[test]
    fn benign_flags_pass() {
        for (tool, invocation, rejected) in [
            ("Kimi", "hcom kimi", KIMI_REJECTED_ARGS),
            ("Qoder", "hcom qoder", QODER_REJECTED_ARGS),
            ("Devin", "hcom devin", DEVIN_REJECTED_ARGS),
            ("OpenCode", "hcom opencode", OPENCODE_REJECTED_ARGS),
            ("Kilo", "hcom kilo", KILO_REJECTED_ARGS),
            ("Pi", "hcom pi", PI_REJECTED_ARGS),
            ("Oh My Pi", "hcom omp", OMP_REJECTED_ARGS),
            ("Gemini", "hcom gemini", GEMINI_REJECTED_ARGS),
            ("Antigravity", "hcom antigravity", ANTIGRAVITY_REJECTED_ARGS),
        ] {
            assert!(
                validate_rejected_args(
                    tool,
                    invocation,
                    &["--model".to_string(), "safe-model".to_string()],
                    rejected,
                )
                .is_empty(),
                "{tool} should accept benign model arguments"
            );
        }
    }

    #[test]
    fn gemini_headless_flags_rejected_interactive_prompt_allowed() {
        for headless in [
            vec!["-p".to_string(), "task".to_string()],
            vec!["--prompt".to_string(), "task".to_string()],
            vec!["--prompt=task".to_string()],
        ] {
            let errors =
                validate_rejected_args("Gemini", "hcom gemini", &headless, GEMINI_REJECTED_ARGS);
            assert_eq!(errors.len(), 1, "expected rejection for {headless:?}");
            assert!(errors[0].contains("prompt-interactive"));
        }
        // -i/--prompt-interactive stays interactive and must pass.
        assert!(
            validate_rejected_args(
                "Gemini",
                "hcom gemini",
                &["-i".to_string(), "task".to_string()],
                GEMINI_REJECTED_ARGS,
            )
            .is_empty()
        );
    }

    #[test]
    fn omp_rejects_non_interactive_or_extensionless_modes() {
        for args in [
            vec!["--mode".to_string(), "json".to_string()],
            vec!["--mode=rpc".to_string()],
            vec!["--no-extensions".to_string()],
        ] {
            let errors = validate_rejected_args("Oh My Pi", "hcom omp", &args, OMP_REJECTED_ARGS);
            assert_eq!(errors.len(), 1, "expected rejection for {args:?}");
        }

        assert!(
            validate_rejected_args(
                "Oh My Pi",
                "hcom omp",
                &["--model".to_string(), "opus".to_string()],
                OMP_REJECTED_ARGS,
            )
            .is_empty()
        );
    }

    #[test]
    fn omp_rejects_acp_root_subcommand() {
        // `omp acp` starts an ACP stdio server (no TUI); reject it as a root
        // subcommand only.
        let errors = validate_rejected_args(
            "Oh My Pi",
            "hcom omp",
            &["acp".to_string()],
            OMP_REJECTED_ARGS,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("acp"));

        // `acp` as a flag value (not position 0) must still pass.
        assert!(
            validate_rejected_args(
                "Oh My Pi",
                "hcom omp",
                &["--model".to_string(), "acp".to_string()],
                OMP_REJECTED_ARGS,
            )
            .is_empty()
        );
    }

    #[test]
    fn subcommand_words_are_allowed_as_option_values() {
        assert!(
            validate_rejected_args(
                "OpenCode",
                "hcom opencode",
                &["--model".to_string(), "run".to_string()],
                OPENCODE_REJECTED_ARGS,
            )
            .is_empty()
        );
    }
}
