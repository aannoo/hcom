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
