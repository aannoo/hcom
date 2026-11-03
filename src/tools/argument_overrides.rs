//! Compose known singleton options without taking over the upstream CLI grammar.
//!
//! Unknown and repeatable options stay in order. The same composition is used
//! for configured defaults and for saved settings, before operation rendering.

use std::collections::HashMap;
use std::ops::Range;

use crate::integration_spec::ArgFlags;
use crate::tool::Tool;

struct ArgPart {
    range: Range<usize>,
    key: Option<String>,
}

impl ArgFlags {
    fn known(&self, name: &str) -> bool {
        [
            self.value,
            self.multi_value,
            self.boolean,
            self.drop,
            self.repeatable,
        ]
        .iter()
        .any(|table| table.contains(&name))
            || self.aliases.iter().any(|(_, long)| *long == name)
    }

    /// Canonical name and whether this token already contains the value.
    fn option(&self, token: &str) -> Option<(String, bool)> {
        if token == "--" || !token.starts_with('-') || token.len() < 2 {
            return None;
        }
        let (name, inline) = token
            .split_once('=')
            .map_or((token, false), |(n, _)| (n, true));
        let name = if self.single_dash_long {
            let bare = name.trim_start_matches('-');
            format!("{}{bare}", if bare.len() == 1 { "-" } else { "--" })
        } else {
            name.to_string()
        };
        let name = self
            .aliases
            .iter()
            .find(|(short, _)| *short == name)
            .map_or(name.clone(), |(_, long)| long.to_string());
        if self.known(&name) {
            return Some((name, inline));
        }
        // Value-taking one-letter options may attach their value: -mfoo,
        // -sread-only, -ckey=value. Never interpret Go single-dash long flags
        // or unknown short-option clusters this way.
        if !self.single_dash_long && !token.starts_with("--") {
            for (short, long) in self.aliases {
                if short.len() == 2
                    && token.len() > 2
                    && token.starts_with(short)
                    && self.value.contains(long)
                {
                    return Some((long.to_string(), true));
                }
            }
        }
        None
    }
}

/// Token spans ensure values (including empty and dash-prefixed values) cannot
/// be mistaken for standalone options. Everything after `--` is opaque.
fn parts(tool: Tool, args: &[String]) -> Vec<ArgPart> {
    let flags = tool.spec().args;
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            result.push(ArgPart {
                range: i..args.len(),
                key: None,
            });
            break;
        }
        let option = flags.option(&args[i]);
        let mut end = i + 1;
        if let Some((name, inline)) = &option {
            if flags.multi_value.contains(&name.as_str()) {
                while end < args.len()
                    && !args[end].starts_with('-')
                    && !flags.subcommands.contains(&args[end].as_str())
                {
                    end += 1;
                }
            } else if !inline
                && flags.value.contains(&name.as_str())
                && end < args.len()
                && args[end] != "--"
                && flags.option(&args[end]).is_none()
            {
                end += 1;
            }
        } else if args[i].starts_with('-')
            && !args[i].contains('=')
            && end < args.len()
            && !args[end].starts_with('-')
        {
            // Unknown upstream/extension options are opaque. Keep their likely
            // value with them, including during positional-prompt cleanup.
            // An unknown boolean can therefore retain a seed prompt; favor
            // preserving upstream values over guessing that the final token
            // is a prompt. An explicit `--` makes the boundary unambiguous.
            end += 1;
        }
        result.push(ArgPart {
            range: i..end,
            key: option.map(|(key, _)| key),
        });
        i = end;
    }
    result
}

/// Later occurrences of known singleton options win, including aliases and
/// duplicates already present within a layer. Unknown/repeatable options are
/// passed through in their original order. Extra options precede an inherited
/// `--` tail, so the separator cannot turn an override into prompt text.
pub(crate) fn override_args(tool: Tool, base: &[String], extra: &[String]) -> Vec<String> {
    let split = |args: &[String]| args.iter().position(|t| t == "--").unwrap_or(args.len());
    let base_end = split(base);
    let extra_end = split(extra);
    let mut tokens = base[..base_end].to_vec();
    tokens.extend_from_slice(&extra[..extra_end]);
    if base_end < base.len() || extra_end < extra.len() {
        tokens.push("--".to_string());
        if base_end < base.len() {
            tokens.extend_from_slice(&base[base_end + 1..]);
        }
        if extra_end < extra.len() {
            tokens.extend_from_slice(&extra[extra_end + 1..]);
        }
    }
    let flags = tool.spec().args;
    let spans = parts(tool, &tokens);
    let mut last = HashMap::new();
    for (i, part) in spans.iter().enumerate() {
        if let Some(key) = &part.key
            && !flags.repeatable.contains(&key.as_str())
        {
            last.insert(key, i);
        }
    }
    let mut result = Vec::new();
    for (i, part) in spans.iter().enumerate() {
        if part
            .key
            .as_ref()
            .is_none_or(|key| last.get(key).is_none_or(|at| *at == i))
        {
            result.extend_from_slice(&tokens[part.range.clone()]);
        }
    }
    result
}

/// Inherit reusable settings, excluding old session selectors, launch prompts,
/// and cwd/worktree controls. Explicit arguments are composed AFTER this step.
pub(crate) fn saved_args(tool: Tool, args: &[String]) -> Vec<String> {
    let flags = tool.spec().args;
    let mut result = Vec::new();
    for part in parts(tool, args) {
        if let Some(key) = &part.key {
            if flags.drop.contains(&key.as_str()) {
                continue;
            }
        } else if flags.positional_prompt && !args[part.range.start].starts_with('-') {
            continue;
        } else if flags.positional_prompt && args[part.range.start] == "--" {
            break;
        }
        result.extend_from_slice(&args[part.range]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn known_model_overrides_apply_to_every_launchable_tool() {
        for spec in crate::integration_spec::ALL
            .iter()
            .filter(|spec| spec.tool != Tool::Adhoc)
        {
            assert_eq!(
                override_args(spec.tool, &s(&["--model", "default"]), &s(&["--model=new"])),
                s(&["--model=new"]),
                "{}",
                spec.name,
            );
            assert_eq!(
                override_args(spec.tool, &s(&["--model=old", "--model", "new"]), &[]),
                s(&["--model", "new"]),
                "{} must repair saved duplicates",
                spec.name,
            );
        }
    }

    #[test]
    fn aliases_and_attached_values_replace_whole_options() {
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["-mold", "-s", "read-only", "--model=stale"]),
                &s(&["--sandbox=workspace-write", "-m=new"])
            ),
            s(&["--sandbox=workspace-write", "-m=new"]),
        );
        assert_eq!(
            override_args(
                Tool::Antigravity,
                &s(&["-model=old"]),
                &s(&["--model", "new"])
            ),
            s(&["--model", "new"]),
        );
    }

    #[test]
    fn unknown_and_repeatable_options_keep_their_order_and_values() {
        let base = s(&["--future", "one", "-ckey=1", "--add-dir", "/a"]);
        let extra = s(&["--future", "two", "--config", "key=2", "--add-dir=/b"]);
        assert_eq!(
            override_args(Tool::Codex, &base, &extra),
            [base, extra].concat()
        );
        let base = s(&["--extension", "preset.ts", "--preset", "old"]);
        let extra = s(&["--preset", "new"]);
        assert_eq!(
            override_args(Tool::Pi, &base, &extra),
            [base, extra].concat()
        );
    }

    #[test]
    fn override_options_precede_inherited_separator_and_never_rewrite_prompt_text() {
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["--model", "old", "--", "--model", "literal prompt"]),
                &s(&["--model", "new"])
            ),
            s(&["--model", "new", "--", "--model", "literal prompt"]),
        );
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["--model", "old"]),
                &s(&["--", "--model", "literal"])
            ),
            s(&["--model", "old", "--", "--model", "literal"]),
        );
    }

    #[test]
    fn positional_tails_keep_layer_order_with_one_separator() {
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["--model", "old", "--", "base"]),
                &s(&["--model", "new", "--", "--model", "literal"])
            ),
            s(&["--model", "new", "--", "base", "--model", "literal"])
        );
    }

    #[test]
    fn optional_values_stop_at_subcommands_but_required_values_can_name_them() {
        assert_eq!(
            saved_args(Tool::Hermes, &s(&["--continue", "chat", "--model", "chat"])),
            s(&["chat", "--model", "chat"]),
        );
        assert_eq!(
            saved_args(
                Tool::Hermes,
                &s(&["--continue", "old", "chat", "-m", "new"])
            ),
            s(&["chat", "-m", "new"]),
        );
    }

    #[test]
    fn codex_approval_aliases_collapse_and_saved_picker_flags_are_dropped() {
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["--approve-for-me"]),
                &s(&["--not-so-yolo"])
            ),
            s(&["--not-so-yolo"]),
        );
        assert_eq!(
            saved_args(
                Tool::Codex,
                &s(&["--include-non-interactive", "--model", "m", "--", "prompt"])
            ),
            s(&["--model", "m"]),
        );
    }

    #[test]
    fn option_values_are_opaque_including_empty_and_dash_prefixed_text() {
        assert_eq!(
            override_args(
                Tool::Qoder,
                &s(&[
                    "--append-system-prompt",
                    "- Always run tests",
                    "--model",
                    "old"
                ]),
                &s(&["--model", "new"])
            ),
            s(&[
                "--append-system-prompt",
                "- Always run tests",
                "--model",
                "new"
            ]),
        );
        assert_eq!(
            override_args(Tool::Qoder, &s(&["--tools", ""]), &[]),
            s(&["--tools", ""])
        );
        // Inline values can contain option-looking strings verbatim.
        assert_eq!(
            override_args(
                Tool::Codex,
                &s(&["--model", "old", "--future=--model=new"]),
                &[]
            ),
            s(&["--model", "old", "--future=--model=new"])
        );
    }

    #[test]
    fn grok_approval_flag_is_singleton_and_does_not_keep_old_prompt() {
        let saved = saved_args(
            Tool::Grok,
            &s(&["--always-approve", "--always-approve", "old prompt"]),
        );
        assert_eq!(saved, s(&["--always-approve", "--always-approve"]));
        assert_eq!(
            override_args(Tool::Grok, &saved, &[]),
            s(&["--always-approve"]),
        );
        assert_eq!(
            override_args(
                Tool::Grok,
                &saved,
                &s(&["--always-approve", "--model", "grok-4.7", "new prompt"]),
            ),
            s(&["--always-approve", "--model", "grok-4.7", "new prompt"]),
        );
    }

    #[test]
    fn inherited_session_and_prompt_controls_are_removed_before_new_overrides() {
        for (tool, args) in [
            (
                Tool::Claude,
                s(&["-r", "old", "--fork-session", "--model", "m", "--", "seed"]),
            ),
            (
                Tool::Gemini,
                s(&[
                    "--resume",
                    "old",
                    "--model",
                    "m",
                    "--prompt-interactive",
                    "seed",
                ]),
            ),
            (
                Tool::Codex,
                s(&["--model", "m", "resume", "old", "--", "seed"]),
            ),
        ] {
            assert_eq!(saved_args(tool, &args), s(&["--model", "m"]), "{tool:?}");
        }
    }
}
