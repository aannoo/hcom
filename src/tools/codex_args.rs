//! Codex CLI argument parsing and validation.
//!
//!
//! Key differences from other tool parsers:
//! - **Case-sensitive flags**: -C (--cd) vs -c (--config) are DIFFERENT flags
//! - Subcommands: exec, resume, fork, review, mcp, sandbox, etc.
//! - Repeatable flags: -c, --config, --enable, --disable, -i, --image, --add-dir
//! - Sandbox flag grouping: if CLI has ANY sandbox flag, strip ALL from env

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use super::args_common::{
    self, FlagValue, SourceType, deduplicate_boolean_flags, extract_flag_name_from_token,
    extract_flag_names_from_tokens, remove_positional, set_positional, shell_quote, shell_split,
    toggle_flag,
};

const SUBCOMMANDS: &[&str] = &[
    "exec",
    "e",
    "resume",
    "fork",
    "review",
    "mcp",
    "mcp-server",
    "app-server",
    "login",
    "logout",
    "completion",
    "sandbox",
    "debug",
    "apply",
    "app",
    "a",
    "cloud",
    "features",
    "help",
];

const EXEC_SUBCOMMANDS: &[&str] = &["exec", "e"];

/// Case-sensitive flags: -C -> --cd, -c -> --config
/// These must be matched with original case, NOT lowercased.
const CASE_SENSITIVE_FLAGS: &[(&str, &str)] = &[("-C", "--cd"), ("-c", "--config")];

/// Case-sensitive boolean flags (match exactly, not lowercased).
const CASE_SENSITIVE_BOOLEAN_FLAGS: &[&str] = &["-V"];

const BOOLEAN_FLAGS: &[&str] = &[
    "--oss",
    "--dangerously-bypass-approvals-and-sandbox",
    "--search",
    "--no-alt-screen",
    "-h",
    "--help",
    "--version",
    "--skip-git-repo-check",
    "--json",
    "--last",
    "--all",
    "--uncommitted",
];

const VALUE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "--cd",
    "--add-dir",
    "--color",
    "-o",
    "--output-last-message",
    "--output-schema",
    "--base",
    "--commit",
    "--title",
];

const CASE_SENSITIVE_VALUE_FLAGS: &[&str] = &["-C", "-c"];

const REPEATABLE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "-i",
    "--image",
    "--add-dir",
];

const SANDBOX_FLAGS: &[&str] = &[
    "--sandbox",
    "-s",
    "-a",
    "--ask-for-approval",
    "--dangerously-bypass-approvals-and-sandbox",
];

fn flag_aliases() -> &'static HashMap<&'static str, &'static str> {
    static ALIASES: OnceLock<HashMap<&str, &str>> = OnceLock::new();
    ALIASES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("-m", "--model");
        m.insert("-c", "--config");
        m.insert("-i", "--image");
        m.insert("-p", "--profile");
        m.insert("-s", "--sandbox");
        m.insert("-a", "--ask-for-approval");
        m.insert("-o", "--output-last-message");
        m
    })
}

struct CodexFlagLookup {
    bool_set: HashSet<String>,
    value_set: HashSet<String>,
    repeatable_set: HashSet<String>,
    sandbox_set: HashSet<String>,
    /// Exact flags for looks_like_flag (lowercase)
    exact_flags: HashSet<String>,
    /// Prefix forms for --flag=value (lowercase)
    prefix_flags: Vec<String>,
    /// Case-sensitive prefixes (-C=, -c=)
    cs_prefixes: Vec<String>,
    /// All known flags for suggestions
    known_flags: Vec<String>,
}

fn flag_lookup() -> &'static CodexFlagLookup {
    static LOOKUP: OnceLock<CodexFlagLookup> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        let bool_set: HashSet<String> = BOOLEAN_FLAGS.iter().map(|s| s.to_string()).collect();
        let value_set: HashSet<String> = VALUE_FLAGS.iter().map(|s| s.to_string()).collect();
        let repeatable_set: HashSet<String> =
            REPEATABLE_FLAGS.iter().map(|s| s.to_string()).collect();
        let sandbox_set: HashSet<String> = SANDBOX_FLAGS.iter().map(|s| s.to_string()).collect();

        let mut exact_flags = HashSet::new();
        for f in BOOLEAN_FLAGS {
            exact_flags.insert(f.to_string());
        }
        for f in VALUE_FLAGS {
            exact_flags.insert(f.to_string());
        }
        for (k, _) in flag_aliases().iter() {
            exact_flags.insert(k.to_string());
        }
        for sub in SUBCOMMANDS {
            exact_flags.insert(sub.to_string());
        }
        exact_flags.insert("--".to_string());

        let mut prefix_flags = Vec::new();
        for f in VALUE_FLAGS {
            prefix_flags.push(format!("{}=", f));
        }

        let cs_prefixes: Vec<String> = CASE_SENSITIVE_VALUE_FLAGS
            .iter()
            .map(|f| format!("{}=", f))
            .collect();

        let mut known_set: HashSet<String> = HashSet::new();
        for f in BOOLEAN_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in CASE_SENSITIVE_BOOLEAN_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in VALUE_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in CASE_SENSITIVE_VALUE_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in SUBCOMMANDS {
            known_set.insert(f.to_string());
        }
        for (k, v) in flag_aliases().iter() {
            known_set.insert(k.to_string());
            known_set.insert(v.to_string());
        }
        let mut known_flags: Vec<String> = known_set.into_iter().collect();
        known_flags.sort();

        CodexFlagLookup {
            bool_set,
            value_set,
            repeatable_set,
            sandbox_set,
            exact_flags,
            prefix_flags,
            cs_prefixes,
            known_flags,
        }
    })
}

fn looks_like_flag(token_lower: &str) -> bool {
    let lookup = flag_lookup();
    args_common::looks_like_flag(token_lower, &lookup.exact_flags, &lookup.prefix_flags)
}

/// Normalized representation of Codex CLI arguments.
#[derive(Debug, Clone)]
pub struct CodexArgsSpec {
    pub source: SourceType,
    pub raw_tokens: Vec<String>,
    pub clean_tokens: Vec<String>,
    pub positional_tokens: Vec<String>,
    pub positional_indexes: Vec<usize>,
    pub flag_values: HashMap<String, FlagValue>,
    pub errors: Vec<String>,
    pub subcommand: Option<String>,
    pub is_json: bool,
    pub is_exec: bool,
}

impl CodexArgsSpec {
    pub fn has_flag(&self, names: &[&str], prefixes: &[&str]) -> bool {
        args_common::has_flag_in_tokens(&self.clean_tokens, names, prefixes)
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn to_env_string(&self) -> String {
        self.rebuild_tokens(true, true)
            .iter()
            .map(|t| shell_quote(t))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn rebuild_tokens(
        &self,
        include_positionals: bool,
        include_subcommand: bool,
    ) -> Vec<String> {
        args_common::rebuild_tokens_from(
            &self.clean_tokens,
            &self.positional_indexes,
            self.subcommand.as_deref(),
            include_positionals,
            include_subcommand,
        )
    }

    /// Get value of a flag. For repeatable flags returns FlagValue::List.
    /// Handles case-sensitive flags (-C vs -c) and aliases.
    pub fn get_flag_value(&self, flag_name: &str) -> Option<FlagValue> {
        let flag_lower = flag_name.to_lowercase();
        let aliases = flag_aliases();

        let mut possible_flags: HashSet<String> = HashSet::new();
        possible_flags.insert(flag_lower.clone());

        if let Some(&long) = aliases.get(flag_lower.as_str()) {
            possible_flags.insert(long.to_string());
        }
        for (&short, &long) in aliases.iter() {
            if long == flag_lower {
                possible_flags.insert(short.to_string());
            }
        }

        // Handle case-sensitive flags
        for &(cs_flag, canonical) in CASE_SENSITIVE_FLAGS {
            if flag_name == cs_flag {
                possible_flags.insert(canonical.to_string());
                possible_flags.insert(cs_flag.to_string());
            }
        }

        for pf in &possible_flags {
            if let Some(val) = self.flag_values.get(pf.as_str()) {
                return Some(val.clone());
            }
        }

        // Fallback: scan clean_tokens
        let mut last_value: Option<String> = None;
        let mut i = 0;
        while i < self.clean_tokens.len() {
            let token = &self.clean_tokens[i];
            let token_lower = token.to_lowercase();

            let mut found_eq = false;
            for pf in &possible_flags {
                let prefix = format!("{}=", pf);
                if token_lower.starts_with(&prefix) {
                    last_value = Some(token[prefix.len()..].to_string());
                    found_eq = true;
                    break;
                }
            }

            if !found_eq && possible_flags.contains(&token_lower) && i + 1 < self.clean_tokens.len()
            {
                let next = &self.clean_tokens[i + 1];
                if !looks_like_flag(&next.to_lowercase()) {
                    last_value = Some(next.clone());
                }
            }

            i += 1;
        }

        last_value.map(FlagValue::Single)
    }

    /// Return new spec with requested updates applied.
    pub fn update(
        &self,
        json_output: Option<bool>,
        prompt: Option<&str>,
        subcommand: Option<Option<&str>>,
        developer_instructions: Option<&str>,
    ) -> CodexArgsSpec {
        let mut tokens = self.clean_tokens.clone();
        let mut new_subcommand = self.subcommand.clone();

        if let Some(sub_opt) = subcommand {
            new_subcommand = sub_opt.map(|s| s.to_string());
        }

        if let Some(json) = json_output {
            tokens = toggle_flag(&tokens, "--json", json);
        }

        if let Some(p) = prompt {
            if p.is_empty() {
                tokens = remove_positional(&tokens, &self.positional_indexes);
            } else {
                tokens = set_positional(&tokens, p, &self.positional_indexes);
            }
        }

        // Developer instructions via -c flag - PREPEND for precedence
        if let Some(instructions) = developer_instructions {
            let mut new_tokens = vec![
                "-c".to_string(),
                format!("developer_instructions={}", instructions),
            ];
            new_tokens.extend(tokens);
            tokens = new_tokens;
        }

        let mut combined = Vec::new();
        if let Some(ref sub) = new_subcommand {
            combined.push(sub.clone());
        }
        combined.extend(tokens);

        parse_tokens(&combined, self.source)
    }
}

/// Resolve Codex args from CLI (highest precedence) or env string.
pub fn resolve_codex_args(cli_args: Option<&[String]>, env_value: Option<&str>) -> CodexArgsSpec {
    if let Some(args) = cli_args {
        if !args.is_empty() {
            return parse_tokens(args, SourceType::Cli);
        }
    }

    if let Some(env_str) = env_value {
        match shell_split(env_str) {
            Ok(tokens) => return parse_tokens(&tokens, SourceType::Env),
            Err(e) => {
                let empty: &[String] = &[];
                return parse_tokens_with_errors(
                    empty,
                    SourceType::Env,
                    vec![format!("invalid Codex args: {}", e)],
                );
            }
        }
    }

    let empty: &[String] = &[];
    parse_tokens(empty, SourceType::None)
}

/// Merge env and CLI specs with smart precedence rules.
///
/// Special: sandbox flags are a GROUP — if CLI has ANY sandbox flag,
/// ALL sandbox flags are stripped from env.
pub fn merge_codex_args(env_spec: &CodexArgsSpec, cli_spec: &CodexArgsSpec) -> CodexArgsSpec {
    let lookup = flag_lookup();
    let final_subcommand = cli_spec
        .subcommand
        .clone()
        .or_else(|| env_spec.subcommand.clone());

    let final_positionals: Vec<String> = if !cli_spec.positional_tokens.is_empty() {
        if cli_spec.positional_tokens == [""] {
            vec![]
        } else {
            cli_spec.positional_tokens.clone()
        }
    } else {
        env_spec.positional_tokens.clone()
    };

    let mut cli_flag_names = extract_flag_names_from_tokens(&cli_spec.clean_tokens);

    // Sandbox group: if CLI has ANY sandbox flag, strip ALL from env
    let cli_has_sandbox = cli_flag_names
        .iter()
        .any(|f| lookup.sandbox_set.contains(f));
    if cli_has_sandbox {
        for sf in &lookup.sandbox_set {
            cli_flag_names.insert(sf.clone());
        }
    }

    let mut merged: Vec<String> = Vec::new();
    let mut skip_next = false;
    let env_pos_set: HashSet<usize> = env_spec.positional_indexes.iter().copied().collect();

    for (i, token) in env_spec.clean_tokens.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if env_pos_set.contains(&i) {
            continue;
        }
        if let Some(flag_name) = extract_flag_name_from_token(token) {
            if cli_flag_names.contains(&flag_name) && !lookup.repeatable_set.contains(&flag_name) {
                if !token.contains('=') && i + 1 < env_spec.clean_tokens.len() {
                    let next = &env_spec.clean_tokens[i + 1];
                    if !looks_like_flag(&next.to_lowercase()) {
                        skip_next = true;
                    }
                }
                continue;
            }
        }
        merged.push(token.clone());
    }

    let cli_pos_set: HashSet<usize> = cli_spec.positional_indexes.iter().copied().collect();
    for (i, token) in cli_spec.clean_tokens.iter().enumerate() {
        if !cli_pos_set.contains(&i) {
            merged.push(token.clone());
        }
    }

    let all_bool: HashSet<String> = BOOLEAN_FLAGS.iter().map(|s| s.to_string()).collect();
    merged = deduplicate_boolean_flags(&merged, &all_bool);

    // For session subcommands, positionals must come immediately after the
    // subcommand so Codex parses the target thread/session correctly.
    let mut combined = Vec::new();
    if let Some(ref sub) = final_subcommand {
        combined.push(sub.clone());
        if matches!(sub.as_str(), "resume" | "fork") {
            combined.extend(final_positionals.iter().cloned());
            combined.extend(merged);
        } else {
            combined.extend(merged);
            combined.extend(final_positionals.iter().cloned());
        }
    } else {
        combined.extend(merged);
        combined.extend(final_positionals.iter().cloned());
    }

    parse_tokens(&combined, SourceType::Cli)
}

/// Check for conflicting flag combinations.
pub fn validate_conflicts(spec: &CodexArgsSpec) -> Vec<String> {
    let mut warnings = Vec::new();

    if spec.is_exec {
        warnings.push(
            "ERROR: Codex exec mode not supported in hcom.\n\
             Use interactive mode (no 'exec' subcommand) for PTY sessions.\n\
             For headless: use 'hcom N claude -p \"task\"'"
                .to_string(),
        );
    }

    if spec.is_json && !spec.is_exec {
        warnings.push("--json flag is only valid with 'exec' subcommand".to_string());
    }

    warnings
}

fn parse_tokens(tokens: &[impl AsRef<str>], source: SourceType) -> CodexArgsSpec {
    parse_tokens_with_errors(tokens, source, vec![])
}

fn parse_tokens_with_errors(
    tokens: &[impl AsRef<str>],
    source: SourceType,
    initial_errors: Vec<String>,
) -> CodexArgsSpec {
    let lookup = flag_lookup();
    let raw_tokens: Vec<String> = tokens.iter().map(|t| t.as_ref().to_string()).collect();

    let mut errors = initial_errors;
    let mut clean: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut positional_indexes: Vec<usize> = Vec::new();
    let mut flag_values: HashMap<String, FlagValue> = HashMap::new();

    let mut subcommand: Option<String> = None;
    let mut is_json = false;
    let mut pending_flag: Option<String> = None;
    let mut after_double_dash = false;

    let mut i: usize = 0;

    // Check for subcommand as first token
    if !raw_tokens.is_empty() {
        let first_lower = raw_tokens[0].to_lowercase();
        if SUBCOMMANDS.contains(&first_lower.as_str()) {
            let normalized = if first_lower == "e" {
                "exec"
            } else if first_lower == "a" {
                "apply"
            } else {
                &first_lower
            };
            subcommand = Some(normalized.to_string());
            i = 1;
        }
    }

    while i < raw_tokens.len() {
        let token = &raw_tokens[i];
        let token_lower = token.to_lowercase();

        // Handle pending value flag
        if let Some(ref pf) = pending_flag.clone() {
            if looks_like_flag(&token_lower) && !after_double_dash {
                errors.push(format!("{} requires a value before '{}'", pf, token));
                pending_flag = None;
                continue;
            }

            // Determine flag key: case-sensitive or lowercase
            let is_cs = CASE_SENSITIVE_VALUE_FLAGS.contains(&pf.as_str());
            let flag_key = if is_cs { pf.clone() } else { pf.to_lowercase() };
            let is_repeatable = if is_cs {
                // -c is repeatable (lowercase), -C is not
                *pf == pf.to_lowercase() && lookup.repeatable_set.contains(&pf.to_lowercase())
            } else {
                lookup.repeatable_set.contains(&flag_key)
            };

            if is_repeatable {
                match flag_values.entry(flag_key) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if let FlagValue::List(list) = e.get_mut() {
                            list.push(token.clone());
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(FlagValue::List(vec![token.clone()]));
                    }
                }
            } else {
                flag_values.insert(flag_key, FlagValue::Single(token.clone()));
            }

            clean.push(token.clone());
            pending_flag = None;
            i += 1;
            continue;
        }

        if after_double_dash {
            let idx = clean.len();
            clean.push(token.clone());
            positional.push(token.clone());
            positional_indexes.push(idx);
            i += 1;
            continue;
        }

        if token == "--" {
            clean.push(token.clone());
            after_double_dash = true;
            i += 1;
            continue;
        }

        // Case-sensitive boolean flags (-V)
        if CASE_SENSITIVE_BOOLEAN_FLAGS.contains(&token.as_str()) {
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Boolean flags (lowercase matching)
        if lookup.bool_set.contains(&token_lower) {
            clean.push(token.clone());
            if token_lower == "--json" {
                is_json = true;
            }
            i += 1;
            continue;
        }

        // Case-sensitive --flag=value (-C=path vs -c=key=val)
        let mut matched_prefix: Option<String> = None;
        for prefix in &lookup.cs_prefixes {
            if token.starts_with(prefix.as_str()) {
                matched_prefix = Some(prefix.clone());
                break;
            }
        }
        if matched_prefix.is_none() {
            for prefix in &lookup.prefix_flags {
                if token_lower.starts_with(prefix.as_str()) {
                    matched_prefix = Some(prefix.clone());
                    break;
                }
            }
        }

        if let Some(prefix) = matched_prefix {
            clean.push(token.clone());
            let flag_key = prefix.trim_end_matches('=').to_string();
            let value = token[prefix.len()..].to_string();
            if lookup.repeatable_set.contains(&flag_key) {
                match flag_values.entry(flag_key) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if let FlagValue::List(list) = e.get_mut() {
                            list.push(value);
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(FlagValue::List(vec![value]));
                    }
                }
            } else {
                flag_values.insert(flag_key, FlagValue::Single(value));
            }
            i += 1;
            continue;
        }

        // Case-sensitive value flags (-C vs -c)
        if CASE_SENSITIVE_VALUE_FLAGS.contains(&token.as_str()) {
            clean.push(token.clone());
            pending_flag = Some(token.clone());
            i += 1;
            continue;
        }

        // Value flags (case-insensitive)
        if lookup.value_set.contains(&token_lower) {
            clean.push(token.clone());
            pending_flag = Some(token.clone());
            i += 1;
            continue;
        }

        // Unknown flag detection
        if token_lower.starts_with('-') && !looks_like_flag(&token_lower) {
            let base = token.split('=').next().unwrap_or(token);
            if let Some(suggestion) = args_common::find_close_match(base, &lookup.known_flags) {
                errors.push(format!(
                    "unknown option '{}' (did you mean {}?). If this was prompt text, pass '--' before it.",
                    token, suggestion
                ));
            } else {
                errors.push(format!(
                    "unknown option '{}'. If this was prompt text, pass '--' before it.",
                    token
                ));
            }
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Positional
        let idx = clean.len();
        clean.push(token.clone());
        if !token_lower.starts_with('-') {
            positional.push(token.clone());
            positional_indexes.push(idx);
        }
        i += 1;
    }

    if let Some(ref pf) = pending_flag {
        errors.push(format!("{} requires a value at end of arguments", pf));
    }

    let is_exec = subcommand
        .as_deref()
        .is_some_and(|s| EXEC_SUBCOMMANDS.contains(&s));

    CodexArgsSpec {
        source,
        raw_tokens,
        clean_tokens: clean,
        positional_tokens: positional,
        positional_indexes,
        flag_values,
        errors,
        subcommand,
        is_json,
        is_exec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_empty() {
        let spec = resolve_codex_args(Some(&[]), None);
        assert!(spec.clean_tokens.is_empty());
        assert!(!spec.is_json);
        assert!(!spec.is_exec);
        assert!(spec.subcommand.is_none());
    }

    #[test]
    fn test_parse_model() {
        let args = sv(&["--model", "gpt-4"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gpt-4".to_string()))
        );
    }

    #[test]
    fn test_parse_model_alias() {
        let args = sv(&["-m", "gpt-4"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gpt-4".to_string()))
        );
    }

    #[test]
    fn test_parse_exec_subcommand() {
        let args = sv(&["exec", "do something"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_exec);
        assert_eq!(spec.subcommand, Some("exec".to_string()));
    }

    #[test]
    fn test_parse_exec_alias() {
        let args = sv(&["e", "do something"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_exec);
        assert_eq!(spec.subcommand, Some("exec".to_string()));
    }

    #[test]
    fn test_parse_resume_subcommand() {
        let args = sv(&["resume", "thread-123"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(spec.subcommand, Some("resume".to_string()));
        assert_eq!(spec.positional_tokens, vec!["thread-123"]);
    }

    #[test]
    fn test_parse_json_flag() {
        let args = sv(&["--json"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_json);
    }

    #[test]
    #[allow(non_snake_case)]
    fn test_case_sensitive_c_vs_C() {
        // -c is --config (lowercase), -C is --cd (uppercase)
        let args = sv(&["-c", "key=value", "-C", "/path"]);
        let spec = parse_tokens(&args, SourceType::Cli);

        // -c (config) should be in flag_values
        assert!(spec.flag_values.contains_key("-c"));
        // -C (cd) should be separate
        assert!(spec.flag_values.contains_key("-C"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn test_case_sensitive_V() {
        let args = sv(&["-V"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["-v"], &[])); // -V lowercases to -v... but wait
        // Actually -V is case-sensitive boolean, stored as "-V"
        // has_flag lowercases, so this won't match. Check clean_tokens directly.
        assert!(spec.clean_tokens.contains(&"-V".to_string()));
    }

    #[test]
    fn test_parse_repeatable_config() {
        let args = sv(&["-c", "a=1", "-c", "b=2"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        match spec.flag_values.get("-c") {
            Some(FlagValue::List(list)) => {
                assert_eq!(list, &vec!["a=1".to_string(), "b=2".to_string()]);
            }
            other => panic!("expected list, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_boolean_flags() {
        let args = sv(&["--last", "--oss"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--last"], &[]));
        assert!(spec.has_flag(&["--oss"], &[]));
    }

    #[test]
    fn test_parse_missing_value() {
        let args = sv(&["--model"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_errors());
        assert!(spec.errors[0].contains("requires a value"));
    }

    #[test]
    fn test_parse_double_dash() {
        let args = sv(&["--oss", "--", "--not-a-flag"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--oss"], &[]));
        assert_eq!(spec.positional_tokens, vec!["--not-a-flag"]);
    }

    #[test]
    fn test_merge_sandbox_group() {
        let env_spec = parse_tokens(
            &sv(&["--sandbox", "workspace-write", "-a", "untrusted"]),
            SourceType::Env,
        );
        let cli_spec = parse_tokens(&sv(&["--dangerously-bypass-approvals-and-sandbox"]), SourceType::Cli);
        let merged = merge_codex_args(&env_spec, &cli_spec);
        // CLI has --dangerously-bypass-approvals-and-sandbox (sandbox flag), so ALL env sandbox flags stripped
        assert!(merged.has_flag(&["--dangerously-bypass-approvals-and-sandbox"], &[]));
        assert!(!merged.has_flag(&["-a"], &[]));
    }

    #[test]
    fn test_merge_repeatable_flags() {
        let env_spec = parse_tokens(&sv(&["-c", "a=1"]), SourceType::Env);
        let cli_spec = parse_tokens(&sv(&["-c", "b=2"]), SourceType::Cli);
        let merged = merge_codex_args(&env_spec, &cli_spec);
        // Both should be present
        match merged.flag_values.get("-c") {
            Some(FlagValue::List(list)) => {
                assert!(list.contains(&"a=1".to_string()));
                assert!(list.contains(&"b=2".to_string()));
            }
            other => panic!("expected list with both, got {:?}", other),
        }
    }

    #[test]
    fn test_merge_resume_positional_order() {
        let env_spec = parse_tokens(&sv(&["--model", "gpt-4"]), SourceType::Env);
        let cli_spec = parse_tokens(&sv(&["resume", "thread-1"]), SourceType::Cli);
        let merged = merge_codex_args(&env_spec, &cli_spec);
        assert_eq!(merged.subcommand, Some("resume".to_string()));
        // Positional should come right after "resume" in rebuild
        let tokens = merged.rebuild_tokens(true, true);
        assert_eq!(tokens[0], "resume");
        assert_eq!(tokens[1], "thread-1");
    }

    #[test]
    fn test_merge_fork_positional_order() {
        let env_spec = parse_tokens(&sv(&["--model", "gpt-4"]), SourceType::Env);
        let cli_spec = parse_tokens(&sv(&["fork", "thread-1"]), SourceType::Cli);
        let merged = merge_codex_args(&env_spec, &cli_spec);
        assert_eq!(merged.subcommand, Some("fork".to_string()));
        let tokens = merged.rebuild_tokens(true, true);
        assert_eq!(tokens[0], "fork");
        assert_eq!(tokens[1], "thread-1");
    }

    #[test]
    fn test_validate_exec_error() {
        let spec = parse_tokens(&sv(&["exec", "do something"]), SourceType::Cli);
        let warnings = validate_conflicts(&spec);
        assert!(warnings.iter().any(|w| w.contains("exec mode")));
    }

    #[test]
    fn test_validate_no_conflicts() {
        let spec = parse_tokens(&sv(&["--model", "gpt-4"]), SourceType::Cli);
        assert!(validate_conflicts(&spec).is_empty());
    }

    #[test]
    fn test_update_developer_instructions() {
        let spec = parse_tokens(&sv(&["--model", "gpt-4"]), SourceType::Cli);
        let updated = spec.update(None, None, None, Some("use hcom"));
        // Should have -c developer_instructions=use hcom prepended
        assert!(updated.clean_tokens.contains(&"-c".to_string()));
        assert!(
            updated
                .clean_tokens
                .iter()
                .any(|t| t.starts_with("developer_instructions="))
        );
    }

    #[test]
    fn test_resolve_from_env() {
        let spec = resolve_codex_args(None, Some("--model gpt-4 --last"));
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gpt-4".to_string()))
        );
        assert!(spec.has_flag(&["--last"], &[]));
    }

    #[test]
    fn test_full_auto_removed() {
        let args = sv(&["--full-auto"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_errors());
        assert!(spec.errors[0].contains("unknown option '--full-auto'"));
    }

}
