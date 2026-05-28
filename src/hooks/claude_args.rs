//! Claude CLI argument parsing and validation.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub use crate::tools::args_common::SourceType;

/// Flag aliases: multiple spellings → canonical form.
fn flag_aliases() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("--model", "--model");
    m.insert("-n", "--name");
    m.insert("--name", "--name");
    m.insert("--allowedtools", "--allowedTools");
    m.insert("--allowed-tools", "--allowedTools");
    m.insert("--disallowedtools", "--disallowedTools");
    m.insert("--disallowed-tools", "--disallowedTools");
    m
}

/// Canonical prefix forms for --flag=value syntax.
fn canonical_prefixes() -> HashMap<String, &'static str> {
    flag_aliases()
        .into_iter()
        .map(|(k, v)| (format!("{}=", k), v))
        .collect()
}

/// Background switches: -p, --print. Not in BOOLEAN_FLAGS for special handling.
const BACKGROUND_SWITCHES: &[&str] = &["-p", "--print"];

/// Fork switches.
const FORK_SWITCHES: &[&str] = &["--fork-session"];

/// Boolean flags (standalone, no value).
const BOOLEAN_FLAGS: &[&str] = &[
    "--verbose",
    "--continue",
    "-c",
    "--dangerously-skip-permissions",
    // hcom-only convenience alias for --dangerously-skip-permissions
    // (Claude itself does not accept --yolo). Translated to the canonical
    // flag at launch time via `translate_yolo_alias`.
    "--yolo",
    "--include-partial-messages",
    "--allow-dangerously-skip-permissions",
    "--bare",
    "--brief",
    "--replay-user-messages",
    "--mcp-debug",
    "--ide",
    "--strict-mcp-config",
    "--no-session-persistence",
    "--include-hook-events",
    "--disable-slash-commands",
    "--exclude-dynamic-system-prompt-sections",
    "--chrome",
    "--no-chrome",
    "-v",
    "--version",
    "-h",
    "--help",
];

/// Flags with optional values (--resume, --debug, etc.).
const OPTIONAL_VALUE_FLAGS: &[&str] = &[
    "--resume",
    "-r",
    "--debug",
    "-d",
    "--from-pr",
    "-w",
    "--worktree",
    "--tmux",
    "--remote-control",
];

/// Alias groups for optional value flags.
const OPTIONAL_ALIAS_GROUPS: &[&[&str]] = &[
    &["--resume", "-r"],
    &["--debug", "-d"],
    &["--worktree", "-w"],
];

/// Value flags (require following value).
const VALUE_FLAGS: &[&str] = &[
    "--add-dir",
    "--agent",
    "--agents",
    "--allowed-tools",
    "--allowedtools",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--betas",
    "--debug-file",
    "--disallowedtools",
    "--disallowed-tools",
    "--effort",
    "--fallback-model",
    "--file",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--max-turns",
    "--mcp-config",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
    "--tools",
];

/// Case-sensitive value flags. Claude root help currently has none.
const CASE_SENSITIVE_VALUE_FLAGS: &[&str] = &[];

/// Normalized representation of Claude CLI arguments.
#[derive(Debug, Clone)]
pub struct ClaudeArgsSpec {
    pub source: SourceType,
    pub raw_tokens: Vec<String>,
    pub clean_tokens: Vec<String>,
    pub positional_tokens: Vec<String>,
    pub positional_indexes: Vec<usize>,
    pub flag_values: HashMap<String, String>,
    pub errors: Vec<String>,
    pub is_background: bool,
    pub is_fork: bool,
}

impl ClaudeArgsSpec {
    pub fn has_flag(&self, names: &[&str], prefixes: &[&str]) -> bool {
        let name_set: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
        let exact_name_set: HashSet<&str> = names.iter().copied().collect();
        let prefix_list: Vec<String> = prefixes.iter().map(|p| p.to_lowercase()).collect();
        let exact_prefix_list: Vec<&str> = prefixes.to_vec();
        let dash_idx = self
            .clean_tokens
            .iter()
            .position(|t| t == "--")
            .unwrap_or(self.clean_tokens.len());

        for token in &self.clean_tokens[..dash_idx] {
            if CASE_SENSITIVE_VALUE_FLAGS.contains(&token.as_str()) {
                if exact_name_set.contains(token.as_str())
                    || exact_prefix_list.iter().any(|p| token.starts_with(*p))
                {
                    return true;
                }
                continue;
            }

            let lower = token.to_lowercase();
            if name_set.contains(&lower) {
                return true;
            }
            if prefix_list.iter().any(|p| lower.starts_with(p.as_str())) {
                return true;
            }
        }

        false
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn to_env_string(&self) -> String {
        self.rebuild_tokens(true)
            .iter()
            .map(|t| shell_quote(t))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Claude has no subcommand, so this only takes include_positionals.
    pub fn rebuild_tokens(&self, include_positionals: bool) -> Vec<String> {
        crate::tools::args_common::rebuild_tokens_from(
            &self.clean_tokens,
            &self.positional_indexes,
            None,
            include_positionals,
            false,
        )
    }

    /// Get value of any flag by searching clean_tokens.
    ///
    /// Handles both --flag value and --flag=value forms.
    /// Handles registered aliases. Returns LAST occurrence ("last wins").
    pub fn get_flag_value(&self, flag_name: &str) -> Option<String> {
        let flag_lower = flag_name.to_lowercase();
        let aliases = flag_aliases();

        // Build all possible flag names
        let mut possible_flags: HashSet<String> = HashSet::new();
        possible_flags.insert(flag_lower.clone());

        // Add canonical form if this is an alias
        if let Some(&canonical) = aliases.get(flag_lower.as_str()) {
            possible_flags.insert(canonical.to_lowercase());
        }

        // Add all aliases that map to the same canonical
        for (&alias, &canonical) in &aliases {
            if canonical.to_lowercase() == flag_lower || alias.to_lowercase() == flag_lower {
                possible_flags.insert(alias.to_lowercase());
                possible_flags.insert(canonical.to_lowercase());
            }
        }

        // Include optional flag aliases (e.g., -r <-> --resume)
        for group in OPTIONAL_ALIAS_GROUPS {
            if group.iter().any(|a| a.to_lowercase() == flag_lower) {
                for alias in *group {
                    possible_flags.insert(alias.to_lowercase());
                }
            }
        }

        let mut last_value: Option<String> = None;
        let mut i = 0;
        while i < self.clean_tokens.len() {
            let token = &self.clean_tokens[i];
            let token_lower = token.to_lowercase();

            // Check --flag=value form
            let mut found_eq = false;
            for possible in &possible_flags {
                let prefix = format!("{}=", possible);
                if token_lower.starts_with(&prefix) {
                    last_value = Some(token[prefix.len()..].to_string());
                    found_eq = true;
                    break;
                }
            }

            if !found_eq && possible_flags.contains(&token_lower) {
                // Check --flag value form
                if i + 1 < self.clean_tokens.len() {
                    let next = &self.clean_tokens[i + 1];
                    if !looks_like_new_flag(&next.to_lowercase()) {
                        last_value = Some(next.clone());
                    }
                }
            }

            i += 1;
        }

        last_value
    }

    /// Return new spec with requested updates applied.
    pub fn update(&self, background: Option<bool>, prompt: Option<&str>) -> ClaudeArgsSpec {
        let mut tokens = self.clean_tokens.clone();
        let mut positional_indexes = self.positional_indexes.clone();

        if let Some(bg) = background {
            tokens = toggle_background(&tokens, &positional_indexes, bg);
            let temp = parse_tokens(&tokens, self.source);
            positional_indexes = temp.positional_indexes;
        }

        if let Some(p) = prompt {
            if p.is_empty() {
                tokens = remove_positional(&tokens, &positional_indexes);
            } else {
                tokens = set_positional(&tokens, p, &positional_indexes);
            }
        }

        parse_tokens(&tokens, self.source)
    }
}

/// Resolve Claude args from CLI (highest precedence) or env string.
pub fn resolve_claude_args(cli_args: Option<&[String]>, env_value: Option<&str>) -> ClaudeArgsSpec {
    if let Some(args) = cli_args
        && !args.is_empty()
    {
        return parse_tokens(args, SourceType::Cli);
    }

    if let Some(env_str) = env_value {
        match shell_split(env_str) {
            Ok(tokens) => return parse_tokens(&tokens, SourceType::Env),
            Err(e) => {
                let empty: &[String] = &[];
                return parse_tokens_with_errors(
                    empty,
                    SourceType::Env,
                    vec![format!("invalid Claude args: {}", e)],
                );
            }
        }
    }

    let empty: &[String] = &[];
    parse_tokens(empty, SourceType::None)
}

/// Merge env and CLI specs with smart precedence rules.
///
/// Rules:
/// 1. CLI positionals REPLACE all env positionals (empty string deletes them)
/// 2. CLI flags override env flags (per-flag precedence)
/// 3. Duplicate boolean flags are deduped
pub fn merge_claude_args(env_spec: &ClaudeArgsSpec, cli_spec: &ClaudeArgsSpec) -> ClaudeArgsSpec {
    // Handle positionals: CLI replaces env (if present), else inherit env
    let final_positionals: Vec<String> = if !cli_spec.positional_tokens.is_empty() {
        if cli_spec.positional_tokens == [""] {
            vec![] // Empty string = delete
        } else {
            cli_spec.positional_tokens.clone()
        }
    } else {
        env_spec.positional_tokens.clone()
    };

    // Extract flag names from CLI to know what to override
    let cli_flag_names = extract_flag_names_from_tokens(&cli_spec.clean_tokens);
    let env_pos_set: HashSet<usize> = env_spec.positional_indexes.iter().copied().collect();
    let cli_pos_set: HashSet<usize> = cli_spec.positional_indexes.iter().copied().collect();

    // Build merged tokens: env flags (not overridden) + CLI flags
    let mut merged: Vec<String> = Vec::new();
    let mut skip_next = false;

    for (i, token) in env_spec.clean_tokens.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if env_pos_set.contains(&i) {
            continue;
        }
        if let Some(flag_name) = extract_flag_name_from_token(token)
            && cli_flag_names.contains(&flag_name)
        {
            // CLI overrides — skip env version
            if !token.contains('=') && i + 1 < env_spec.clean_tokens.len() {
                let next = &env_spec.clean_tokens[i + 1];
                if !looks_like_new_flag(&next.to_lowercase()) {
                    skip_next = true;
                }
            }
            continue;
        }
        merged.push(token.clone());
    }

    // Append CLI tokens (excluding positionals)
    for (i, token) in cli_spec.clean_tokens.iter().enumerate() {
        if !cli_pos_set.contains(&i) {
            merged.push(token.clone());
        }
    }

    // Deduplicate boolean flags
    merged = deduplicate_boolean_flags(&merged);

    // Insert positionals before -- or at end
    let insert_idx = merged
        .iter()
        .position(|t| t == "--")
        .unwrap_or(merged.len());
    for (j, pos) in final_positionals.iter().enumerate() {
        merged.insert(insert_idx + j, pos.clone());
    }

    parse_tokens(&merged, SourceType::Cli)
}

/// Add HCOM-specific background mode defaults if missing.
///
/// When -p/--print detected, adds --output-format stream-json and --verbose.
pub fn add_background_defaults(spec: &ClaudeArgsSpec) -> ClaudeArgsSpec {
    if !spec.is_background {
        return spec.clone();
    }

    let mut tokens = spec.clean_tokens.clone();
    let mut modified = false;

    let insert_idx = tokens
        .iter()
        .position(|t| t == "--")
        .unwrap_or(tokens.len());

    if !spec.has_flag(&["--output-format"], &["--output-format="]) {
        tokens.insert(insert_idx, "stream-json".to_string());
        tokens.insert(insert_idx, "--output-format".to_string());
        modified = true;
    }

    let new_insert_idx = tokens
        .iter()
        .position(|t| t == "--")
        .unwrap_or(tokens.len());
    if !spec.has_flag(&["--verbose"], &[]) {
        tokens.insert(new_insert_idx, "--verbose".to_string());
        modified = true;
    }

    if !modified {
        return spec.clone();
    }

    parse_tokens(&tokens, spec.source)
}

/// Rewrite hcom's `--yolo` alias to Claude's real `--dangerously-skip-permissions`.
///
/// Claude has no native `--yolo` flag; this is an hcom-only ergonomic alias for
/// users who already type `--yolo` for Codex. We accept it in the parser so the
/// allowlist passes, then translate it here before the args reach the `claude`
/// binary. Returns `true` if any token was translated.
///
/// Tokens after `--` are left alone (those are prompt text, not Claude flags).
/// If the canonical `--dangerously-skip-permissions` already appears earlier in
/// the args, the translated `--yolo` will be deduplicated by the normal boolean
/// dedup pass on the next `parse_tokens` round-trip.
pub fn translate_yolo_alias(spec: &ClaudeArgsSpec) -> (ClaudeArgsSpec, bool) {
    let dash_idx = spec
        .clean_tokens
        .iter()
        .position(|t| t == "--")
        .unwrap_or(spec.clean_tokens.len());

    let mut translated = false;
    let mut new_tokens = spec.clean_tokens.clone();
    for token in new_tokens.iter_mut().take(dash_idx) {
        if token.to_lowercase() == "--yolo" {
            *token = "--dangerously-skip-permissions".to_string();
            translated = true;
        }
    }

    if !translated {
        return (spec.clone(), false);
    }

    // Re-parse to refresh derived fields (and let dedup drop any duplicate
    // canonical flag the user might also have passed alongside --yolo).
    let deduped = deduplicate_boolean_flags(&new_tokens);
    (parse_tokens(&deduped, spec.source), true)
}

/// Check for conflicting flag combinations. Returns warning messages.
pub fn validate_conflicts(spec: &ClaudeArgsSpec) -> Vec<String> {
    let mut warnings = Vec::new();

    let mut system_flags: Vec<String> = Vec::new();
    for token in &spec.clean_tokens {
        let lower = token.to_lowercase();
        if lower == "--system-prompt" || lower == "--append-system-prompt" {
            system_flags.push(lower);
        } else if lower.starts_with("--system-prompt=")
            || lower.starts_with("--append-system-prompt=")
        {
            system_flags.push(lower.split('=').next().unwrap_or("").to_string());
        }
    }

    if system_flags.len() > 1 {
        let is_standard = system_flags.len() == 2
            && system_flags.contains(&"--system-prompt".to_string())
            && system_flags.contains(&"--append-system-prompt".to_string());
        if !is_standard {
            warnings.push(format!(
                "Multiple system prompts: {}. All included in order.",
                system_flags.join(", ")
            ));
        }
    }

    warnings
}

fn parse_tokens(tokens: &[impl AsRef<str>], source: SourceType) -> ClaudeArgsSpec {
    parse_tokens_with_errors(tokens, source, vec![])
}

fn parse_tokens_with_errors(
    tokens: &[impl AsRef<str>],
    source: SourceType,
    initial_errors: Vec<String>,
) -> ClaudeArgsSpec {
    let aliases = flag_aliases();
    let canon_prefixes = canonical_prefixes();
    let bg_set: HashSet<&str> = BACKGROUND_SWITCHES.iter().copied().collect();
    let fork_set: HashSet<&str> = FORK_SWITCHES.iter().copied().collect();
    let bool_set: HashSet<&str> = BOOLEAN_FLAGS.iter().copied().collect();
    let opt_val_set: HashSet<&str> = OPTIONAL_VALUE_FLAGS.iter().copied().collect();
    let val_set: HashSet<&str> = VALUE_FLAGS.iter().copied().collect();
    let case_sensitive_val_set: HashSet<&str> =
        CASE_SENSITIVE_VALUE_FLAGS.iter().copied().collect();

    let opt_val_prefixes: Vec<String> = OPTIONAL_VALUE_FLAGS
        .iter()
        .map(|f| format!("{}=", f))
        .collect();
    let val_prefixes: Vec<String> = VALUE_FLAGS.iter().map(|f| format!("{}=", f)).collect();
    let case_sensitive_val_prefixes: Vec<String> = CASE_SENSITIVE_VALUE_FLAGS
        .iter()
        .map(|f| format!("{}=", f))
        .collect();

    let raw_tokens: Vec<String> = tokens.iter().map(|t| t.as_ref().to_string()).collect();

    let mut errors = initial_errors;
    let mut clean: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut positional_indexes: Vec<usize> = Vec::new();
    let mut flag_values: HashMap<String, String> = HashMap::new();

    let mut pending_canonical: Option<&str> = None;
    let mut pending_canonical_token: Option<String> = None;
    let mut pending_generic_flag: Option<String> = None;
    let mut after_double_dash = false;
    let mut is_background = false;
    let mut is_fork = false;

    let mut i = 0;
    while i < raw_tokens.len() {
        let token = &raw_tokens[i];
        let token_lower = token.to_lowercase();
        let mut advance = true;

        if let Some(canonical) = pending_canonical {
            if looks_like_new_flag(&token_lower) {
                let display = pending_canonical_token.as_deref().unwrap_or(canonical);
                errors.push(format!("{} requires a value before '{}'", display, token));
                pending_canonical = None;
                pending_canonical_token = None;
                advance = false;
            } else {
                let idx = clean.len();
                clean.push(token.clone());
                if after_double_dash {
                    positional.push(token.clone());
                    positional_indexes.push(idx);
                }
                flag_values.insert(canonical.to_string(), token.clone());
                pending_canonical = None;
                pending_canonical_token = None;
            }
            if advance {
                i += 1;
            }
            continue;
        }

        if let Some(ref generic_flag) = pending_generic_flag.clone() {
            if looks_like_new_flag(&token_lower) {
                errors.push(format!(
                    "{} requires a value before '{}'",
                    generic_flag, token
                ));
                pending_generic_flag = None;
                advance = false;
            } else {
                let idx = clean.len();
                clean.push(token.clone());
                if after_double_dash {
                    positional.push(token.clone());
                    positional_indexes.push(idx);
                }
                pending_generic_flag = None;
            }
            if advance {
                i += 1;
            }
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

        if token_lower == "--" {
            clean.push(token.clone());
            after_double_dash = true;
            i += 1;
            continue;
        }

        if case_sensitive_val_prefixes
            .iter()
            .any(|p| token.starts_with(p))
        {
            clean.push(token.clone());
            i += 1;
            continue;
        }

        if case_sensitive_val_set.contains(token.as_str()) {
            pending_generic_flag = Some(token.clone());
            clean.push(token.clone());
            i += 1;
            continue;
        }

        if bg_set.contains(token_lower.as_str()) {
            is_background = true;
            clean.push(token.clone());
            i += 1;
            continue;
        }

        if fork_set.contains(token_lower.as_str()) {
            is_fork = true;
            clean.push(token.clone());
            i += 1;
            continue;
        }

        if bool_set.contains(token_lower.as_str()) {
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Check canonical --flag=value form
        if let Some((canonical, value)) =
            extract_canonical_prefixed(token, &token_lower, &canon_prefixes)
        {
            clean.push(token.clone());
            flag_values.insert(canonical.to_string(), value);
            i += 1;
            continue;
        }

        // Check value flag prefixes (--flag=value for non-canonical)
        if val_prefixes.iter().any(|p| token_lower.starts_with(p)) {
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Check canonical alias (space-separated form)
        if let Some(&canonical) = aliases.get(token_lower.as_str()) {
            pending_canonical = Some(canonical);
            pending_canonical_token = Some(token.clone());
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Check optional value flag with = form
        if opt_val_prefixes.iter().any(|p| token_lower.starts_with(p)) {
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Check optional value flags
        if opt_val_set.contains(token_lower.as_str()) {
            // Peek ahead — only consume value if next isn't a flag
            if i + 1 < raw_tokens.len() {
                let next_lower = raw_tokens[i + 1].to_lowercase();
                if !looks_like_new_flag(&next_lower) {
                    pending_generic_flag = Some(token.clone());
                    clean.push(token.clone());
                    i += 1;
                    continue;
                }
            }
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Check value flags
        if val_set.contains(token_lower.as_str()) {
            pending_generic_flag = Some(token.clone());
            clean.push(token.clone());
            i += 1;
            continue;
        }

        // Unknown option detection
        if (token_lower.starts_with("--")
            || (token_lower.starts_with('-')
                && token_lower.len() == 2
                && token_lower
                    .chars()
                    .nth(1)
                    .is_some_and(|c| c.is_alphabetic())))
            && !looks_like_new_flag(&token_lower)
        {
            let base = token.split('=').next().unwrap_or(token);
            let known_flags = build_known_flags();
            let suggestion = find_close_match(base, &known_flags);
            if let Some(suggested) = suggestion {
                errors.push(format!(
                    "unknown option '{}' (did you mean {}?). \
                     If this was prompt text, pass '--' before it.",
                    token, suggested
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

        // Positional argument
        let idx = clean.len();
        clean.push(token.clone());
        if !looks_like_new_flag(&token_lower) {
            positional.push(token.clone());
            positional_indexes.push(idx);
        }
        i += 1;
    }

    // Handle unterminated pending flags
    if let Some(canonical) = pending_canonical {
        let display = pending_canonical_token.as_deref().unwrap_or(canonical);
        errors.push(format!("{} requires a value at end of arguments", display));
    }
    if let Some(ref flag) = pending_generic_flag {
        errors.push(format!("{} requires a value at end of arguments", flag));
    }

    ClaudeArgsSpec {
        source,
        raw_tokens,
        clean_tokens: clean,
        positional_tokens: positional,
        positional_indexes,
        flag_values,
        errors,
        is_background,
        is_fork,
    }
}

/// Pre-computed flag lookup sets for `looks_like_new_flag`.
/// Built once via OnceLock to avoid rebuilding on every call.
struct FlagLookup {
    exact: HashSet<String>,
    prefixes: Vec<String>,
}

static FLAG_LOOKUP: OnceLock<FlagLookup> = OnceLock::new();

fn get_flag_lookup() -> &'static FlagLookup {
    FLAG_LOOKUP.get_or_init(|| {
        let mut exact = HashSet::new();
        for f in BACKGROUND_SWITCHES {
            exact.insert(f.to_string());
        }
        for f in FORK_SWITCHES {
            exact.insert(f.to_string());
        }
        for f in BOOLEAN_FLAGS {
            exact.insert(f.to_string());
        }
        for f in VALUE_FLAGS {
            exact.insert(f.to_string());
        }
        for f in CASE_SENSITIVE_VALUE_FLAGS {
            exact.insert(f.to_string());
        }
        for f in OPTIONAL_VALUE_FLAGS {
            exact.insert(f.to_string());
        }
        for (k, _) in flag_aliases() {
            exact.insert(k.to_string());
        }
        exact.insert("--".to_string());

        let mut prefixes = Vec::new();
        for f in OPTIONAL_VALUE_FLAGS {
            prefixes.push(format!("{}=", f));
        }
        for f in VALUE_FLAGS {
            prefixes.push(format!("{}=", f));
        }
        for f in CASE_SENSITIVE_VALUE_FLAGS {
            prefixes.push(format!("{}=", f));
        }
        for (prefix, _) in canonical_prefixes() {
            prefixes.push(prefix);
        }

        FlagLookup { exact, prefixes }
    })
}

/// Check if token looks like a flag (not a value).
///
/// No catch-all `-` check — explicitly list known flags to avoid
/// rejecting valid values like "- check something" or "-1".
fn looks_like_new_flag(token_lower: &str) -> bool {
    let lookup = get_flag_lookup();

    if lookup.exact.contains(token_lower) {
        return true;
    }

    lookup.prefixes.iter().any(|p| token_lower.starts_with(p))
}

/// Extract canonical flag and value from --flag=value syntax.
fn extract_canonical_prefixed<'a>(
    token: &str,
    token_lower: &str,
    prefixes: &'a HashMap<String, &'static str>,
) -> Option<(&'a str, String)> {
    for (prefix, canonical) in prefixes {
        if token_lower.starts_with(prefix) {
            return Some((canonical, token[prefix.len()..].to_string()));
        }
    }
    None
}

/// Extract flag name from token, handling --flag=value syntax.
/// Remove duplicate boolean flags, keeping first occurrence.
/// Wraps args_common::deduplicate_boolean_flags with Claude-specific flag sets.
fn deduplicate_boolean_flags(tokens: &[String]) -> Vec<String> {
    let all_bool: HashSet<String> = BOOLEAN_FLAGS
        .iter()
        .chain(BACKGROUND_SWITCHES.iter())
        .chain(FORK_SWITCHES.iter())
        .map(|s| s.to_lowercase())
        .collect();
    dedup_bool_flags(tokens, &all_bool)
}

/// Toggle background flag, preserving positional arguments.
fn toggle_background(
    tokens: &[String],
    positional_indexes: &[usize],
    desired: bool,
) -> Vec<String> {
    let pos_set: HashSet<usize> = positional_indexes.iter().copied().collect();
    let bg_set: HashSet<&str> = BACKGROUND_SWITCHES.iter().copied().collect();

    let filtered: Vec<String> = tokens
        .iter()
        .enumerate()
        .filter(|(idx, token)| {
            pos_set.contains(idx) || !bg_set.contains(token.to_lowercase().as_str())
        })
        .map(|(_, t)| t.clone())
        .collect();

    if desired {
        if filtered.len() != tokens.len() {
            // Already had background, keep original
            tokens.to_vec()
        } else {
            let mut result = vec!["-p".to_string()];
            result.extend(filtered);
            result
        }
    } else {
        filtered
    }
}

/// Set or replace the first positional argument.
fn set_positional(tokens: &[String], value: &str, positional_indexes: &[usize]) -> Vec<String> {
    let mut result = tokens.to_vec();
    if !positional_indexes.is_empty() {
        result[positional_indexes[0]] = value.to_string();
    } else {
        result.push(value.to_string());
    }
    result
}

/// Remove the first positional argument.
fn remove_positional(tokens: &[String], positional_indexes: &[usize]) -> Vec<String> {
    if positional_indexes.is_empty() {
        return tokens.to_vec();
    }
    let idx = positional_indexes[0];
    let mut result = tokens[..idx].to_vec();
    result.extend_from_slice(&tokens[idx + 1..]);
    result
}

/// Build sorted list of all known Claude flags (for suggestions).
fn build_known_flags() -> Vec<String> {
    let mut flags: HashSet<String> = HashSet::new();
    for f in BACKGROUND_SWITCHES {
        flags.insert(f.to_string());
    }
    for f in BOOLEAN_FLAGS {
        flags.insert(f.to_string());
    }
    for f in OPTIONAL_VALUE_FLAGS {
        flags.insert(f.to_string());
    }
    for f in VALUE_FLAGS {
        flags.insert(f.to_string());
    }
    for f in CASE_SENSITIVE_VALUE_FLAGS {
        flags.insert(f.to_string());
    }
    for (k, v) in flag_aliases() {
        flags.insert(k.to_string());
        flags.insert(v.to_string());
    }
    let mut sorted: Vec<String> = flags.into_iter().collect();
    sorted.sort();
    sorted
}

use crate::tools::args_common::{
    deduplicate_boolean_flags as dedup_bool_flags, extract_flag_name_from_token,
    extract_flag_names_from_tokens, find_close_match, shell_split,
};

/// Simple shell-safe quoting for env string serialization.
use crate::tools::args_common::shell_quote;

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_tokens --

    #[test]
    fn test_parse_empty() {
        let spec = resolve_claude_args(Some(&[]), None);
        assert!(spec.clean_tokens.is_empty());
        assert!(!spec.is_background);
        assert!(!spec.is_fork);
        assert!(spec.errors.is_empty());
    }

    #[test]
    fn test_parse_boolean_flags() {
        let args: Vec<String> = vec!["--verbose".into(), "--continue".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--verbose"], &[]));
        assert!(spec.has_flag(&["--continue"], &[]));
        assert!(!spec.is_background);
    }

    #[test]
    fn test_parse_yolo_accepted_as_boolean() {
        let args: Vec<String> = vec!["--yolo".into(), "--model".into(), "opus".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(!spec.has_errors(), "{:?}", spec.errors);
        assert!(spec.has_flag(&["--yolo"], &[]));
        assert!(spec.positional_tokens.is_empty());
    }

    #[test]
    fn test_translate_yolo_rewrites_to_dangerously_skip() {
        let spec = parse_tokens(&["--yolo", "--model", "opus"], SourceType::Cli);
        let (translated, fired) = translate_yolo_alias(&spec);
        assert!(fired);
        assert!(translated.has_flag(&["--dangerously-skip-permissions"], &[]));
        assert!(!translated.has_flag(&["--yolo"], &[]));
        assert_eq!(
            translated.get_flag_value("--model"),
            Some("opus".to_string())
        );
    }

    #[test]
    fn test_translate_yolo_noop_without_flag() {
        let spec = parse_tokens(&["--model", "opus"], SourceType::Cli);
        let (translated, fired) = translate_yolo_alias(&spec);
        assert!(!fired);
        assert_eq!(translated.clean_tokens, spec.clean_tokens);
    }

    #[test]
    fn test_translate_yolo_dedups_against_canonical() {
        let spec = parse_tokens(
            &["--dangerously-skip-permissions", "--yolo"],
            SourceType::Cli,
        );
        let (translated, fired) = translate_yolo_alias(&spec);
        assert!(fired);
        let count = translated
            .clean_tokens
            .iter()
            .filter(|t| t.to_lowercase() == "--dangerously-skip-permissions")
            .count();
        assert_eq!(count, 1, "{:?}", translated.clean_tokens);
    }

    #[test]
    fn test_translate_yolo_preserves_prompt_after_double_dash() {
        let spec = parse_tokens(&["--yolo", "--", "literal --yolo prompt"], SourceType::Cli);
        let (translated, fired) = translate_yolo_alias(&spec);
        assert!(fired);
        assert!(translated.has_flag(&["--dangerously-skip-permissions"], &[]));
        assert_eq!(translated.positional_tokens, vec!["literal --yolo prompt"]);
    }

    #[test]
    fn test_parse_background_flag() {
        let args: Vec<String> = vec!["-p".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_background);
        assert!(spec.has_flag(&["-p"], &[]));
    }

    #[test]
    fn test_parse_fork_flag() {
        let args: Vec<String> = vec!["--fork-session".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_fork);
    }

    #[test]
    fn test_parse_new_current_value_flags_not_positionals() {
        let args = [
            "--bare",
            "--brief",
            "--effort",
            "high",
            "--plugin-url=https://example.invalid/plugin.zip",
            "--remote-control",
            "reviewer",
            "--remote-control-session-name-prefix",
            "host",
            "-w",
            "branch-name",
            "--tmux=classic",
            "-n",
            "demo",
        ];
        let spec = parse_tokens(&args, SourceType::Cli);

        assert!(!spec.has_errors(), "{:?}", spec.errors);
        assert!(spec.positional_tokens.is_empty());
        assert!(spec.has_flag(&["--bare"], &[]));
        assert!(spec.has_flag(&["--brief"], &[]));
        assert_eq!(spec.get_flag_value("--effort"), Some("high".to_string()));
        assert_eq!(
            spec.get_flag_value("--plugin-url"),
            Some("https://example.invalid/plugin.zip".to_string())
        );
        assert_eq!(
            spec.get_flag_value("--remote-control"),
            Some("reviewer".to_string())
        );
        assert_eq!(
            spec.get_flag_value("--worktree"),
            Some("branch-name".to_string())
        );
        assert_eq!(spec.get_flag_value("--tmux"), Some("classic".to_string()));
        assert_eq!(spec.get_flag_value("--name"), Some("demo".to_string()));
    }

    #[test]
    fn test_parse_worktree_optional_value_without_value() {
        let spec = parse_tokens(&["--worktree", "--verbose"], SourceType::Cli);

        assert!(!spec.has_errors(), "{:?}", spec.errors);
        assert!(spec.has_flag(&["--worktree"], &[]));
        assert!(spec.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_parse_value_flag_space() {
        let args: Vec<String> = vec!["--model".into(), "opus".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(spec.flag_values.get("--model"), Some(&"opus".to_string()));
        assert_eq!(spec.get_flag_value("--model"), Some("opus".to_string()));
    }

    #[test]
    fn test_parse_value_flag_equals() {
        let args: Vec<String> = vec!["--model=sonnet".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(spec.get_flag_value("--model"), Some("sonnet".to_string()));
    }

    #[test]
    fn test_parse_alias_flags() {
        let args: Vec<String> = vec!["--allowed-tools".into(), "Bash".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.flag_values.get("--allowedTools"),
            Some(&"Bash".to_string())
        );
        // Alias lookup should work
        assert_eq!(
            spec.get_flag_value("--allowedtools"),
            Some("Bash".to_string())
        );
        assert_eq!(
            spec.get_flag_value("--allowed-tools"),
            Some("Bash".to_string())
        );
    }

    #[test]
    fn test_parse_positional() {
        let args: Vec<String> = vec!["--verbose".into(), "hello world".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(spec.positional_tokens, vec!["hello world"]);
    }

    #[test]
    fn test_parse_double_dash() {
        let args: Vec<String> = vec!["--verbose".into(), "--".into(), "--not-a-flag".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--verbose"], &[]));
        assert_eq!(spec.positional_tokens, vec!["--not-a-flag"]);
    }

    #[test]
    fn test_parse_optional_value_with_value() {
        let args: Vec<String> = vec!["--resume".into(), "session-123".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--resume"),
            Some("session-123".to_string())
        );
    }

    #[test]
    fn test_parse_optional_value_without_value() {
        let args: Vec<String> = vec!["--resume".into(), "--verbose".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        // --resume should be present but without consuming --verbose as value
        assert!(spec.has_flag(&["--resume"], &[]));
        assert!(spec.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_parse_optional_value_equals() {
        let args: Vec<String> = vec!["--resume=abc".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&[], &["--resume="]));
    }

    #[test]
    fn test_parse_missing_value_error() {
        let args: Vec<String> = vec!["--model".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_errors());
        assert!(spec.errors[0].contains("requires a value"));
    }

    #[test]
    fn test_parse_missing_value_before_flag() {
        let args: Vec<String> = vec!["--model".into(), "--verbose".into()];
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_errors());
        assert!(spec.errors[0].contains("requires a value before '--verbose'"));
    }

    // -- resolve_claude_args --

    #[test]
    fn test_resolve_from_cli() {
        let args: Vec<String> = vec!["--model".into(), "opus".into()];
        let spec = resolve_claude_args(Some(&args), Some("--model sonnet"));
        // CLI takes precedence
        assert_eq!(spec.get_flag_value("--model"), Some("opus".to_string()));
    }

    #[test]
    fn test_resolve_from_env() {
        let spec = resolve_claude_args(None, Some("--model sonnet --verbose"));
        assert_eq!(spec.get_flag_value("--model"), Some("sonnet".to_string()));
        assert!(spec.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_resolve_invalid_env() {
        let spec = resolve_claude_args(None, Some("--model 'unterminated"));
        assert!(spec.has_errors());
    }

    // -- merge_claude_args --

    #[test]
    fn test_merge_cli_overrides_env() {
        let env_spec = parse_tokens(&["--model", "sonnet", "--verbose"], SourceType::Env);
        let cli_spec = parse_tokens(&["--model", "opus"], SourceType::Cli);
        let merged = merge_claude_args(&env_spec, &cli_spec);
        assert_eq!(merged.get_flag_value("--model"), Some("opus".to_string()));
        assert!(merged.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_merge_cli_positional_replaces_env() {
        let env_spec = parse_tokens(&["env prompt"], SourceType::Env);
        let cli_spec = parse_tokens(&["cli prompt"], SourceType::Cli);
        let merged = merge_claude_args(&env_spec, &cli_spec);
        assert_eq!(merged.positional_tokens, vec!["cli prompt"]);
    }

    #[test]
    fn test_merge_inherit_env_positional() {
        let env_spec = parse_tokens(&["env prompt"], SourceType::Env);
        let cli_spec = parse_tokens(&["--verbose"], SourceType::Cli);
        let merged = merge_claude_args(&env_spec, &cli_spec);
        assert_eq!(merged.positional_tokens, vec!["env prompt"]);
    }

    #[test]
    fn test_merge_dedup_boolean() {
        let env_spec = parse_tokens(&["--verbose"], SourceType::Env);
        let cli_spec = parse_tokens(&["--verbose"], SourceType::Cli);
        let merged = merge_claude_args(&env_spec, &cli_spec);
        let verbose_count = merged
            .clean_tokens
            .iter()
            .filter(|t| t.to_lowercase() == "--verbose")
            .count();
        assert_eq!(verbose_count, 1);
    }

    // -- add_background_defaults --

    #[test]
    fn test_add_background_defaults() {
        let spec = parse_tokens(&["-p"], SourceType::Cli);
        let updated = add_background_defaults(&spec);
        assert!(updated.has_flag(&["--output-format"], &["--output-format="]));
        assert!(updated.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_add_background_defaults_no_op() {
        let spec = parse_tokens(&["--verbose"], SourceType::Cli);
        let updated = add_background_defaults(&spec);
        // Not background, should be unchanged
        assert_eq!(updated.clean_tokens, spec.clean_tokens);
    }

    // -- update --

    #[test]
    fn test_update_enable_background() {
        let spec = parse_tokens(&["--model", "opus"], SourceType::Cli);
        let updated = spec.update(Some(true), None);
        assert!(updated.is_background);
    }

    #[test]
    fn test_update_disable_background() {
        let spec = parse_tokens(&["-p", "--model", "opus"], SourceType::Cli);
        let updated = spec.update(Some(false), None);
        assert!(!updated.is_background);
    }

    #[test]
    fn test_update_set_prompt() {
        let spec = parse_tokens(&["--verbose"], SourceType::Cli);
        let updated = spec.update(None, Some("do something"));
        assert_eq!(updated.positional_tokens, vec!["do something"]);
    }

    #[test]
    fn test_update_delete_prompt() {
        let spec = parse_tokens(&["--verbose", "old prompt"], SourceType::Cli);
        let updated = spec.update(None, Some(""));
        assert!(updated.positional_tokens.is_empty());
    }

    // -- validate_conflicts --

    #[test]
    fn test_validate_no_conflicts() {
        let spec = parse_tokens(&["--model", "opus"], SourceType::Cli);
        assert!(validate_conflicts(&spec).is_empty());
    }

    #[test]
    fn test_validate_standard_system_prompt() {
        let spec = parse_tokens(
            &["--system-prompt", "a", "--append-system-prompt", "b"],
            SourceType::Cli,
        );
        // Standard pattern (one of each) = no warning
        assert!(validate_conflicts(&spec).is_empty());
    }

    #[test]
    fn test_validate_multiple_system_prompt() {
        let spec = parse_tokens(
            &["--system-prompt", "a", "--system-prompt", "b"],
            SourceType::Cli,
        );
        let warnings = validate_conflicts(&spec);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Multiple system prompts"));
    }

    // -- helpers --

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_to_env_string() {
        let spec = parse_tokens(&["--model", "opus", "--verbose"], SourceType::Cli);
        let env_str = spec.to_env_string();
        assert!(env_str.contains("--model"));
        assert!(env_str.contains("opus"));
        assert!(env_str.contains("--verbose"));
    }

    #[test]
    fn test_rebuild_tokens_without_positionals() {
        let spec = parse_tokens(
            &["--verbose", "my prompt", "--model", "opus"],
            SourceType::Cli,
        );
        let tokens = spec.rebuild_tokens(false);
        assert!(!tokens.contains(&"my prompt".to_string()));
        assert!(tokens.contains(&"--verbose".to_string()));
    }

    #[test]
    fn test_get_flag_value_last_wins() {
        let spec = parse_tokens(&["--model", "sonnet", "--model", "opus"], SourceType::Cli);
        assert_eq!(spec.get_flag_value("--model"), Some("opus".to_string()));
    }

    #[test]
    fn test_has_flag_before_double_dash() {
        let spec = parse_tokens(&["--verbose", "--", "--model", "x"], SourceType::Cli);
        assert!(spec.has_flag(&["--verbose"], &[]));
        // --model is after --, should not be found as a flag
        assert!(!spec.has_flag(&["--model"], &[]));
    }
}
