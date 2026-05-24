//! Gemini CLI argument parsing and validation.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use super::args_common::{
    self, FlagValue, SourceType, deduplicate_boolean_flags, extract_flag_name_from_token,
    extract_flag_names_from_tokens, remove_flag_with_value, set_value_flag, shell_quote,
    shell_split, toggle_flag,
};

fn flag_aliases() -> &'static HashMap<&'static str, &'static str> {
    static ALIASES: OnceLock<HashMap<&str, &str>> = OnceLock::new();
    ALIASES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("-d", "--debug");
        m.insert("-m", "--model");
        m.insert("-p", "--prompt");
        m.insert("-i", "--prompt-interactive");
        m.insert("-s", "--sandbox");
        m.insert("-y", "--yolo");
        m.insert("-e", "--extensions");
        m.insert("-l", "--list-extensions");
        m.insert("-r", "--resume");
        m.insert("-w", "--worktree");
        m.insert("-o", "--output-format");
        m.insert("-v", "--version");
        m.insert("-h", "--help");
        m
    })
}

const BOOLEAN_FLAGS: &[&str] = &[
    "-d",
    "--debug",
    "-s",
    "--sandbox",
    "-y",
    "--yolo",
    "-l",
    "--list-extensions",
    "--list-sessions",
    "--screen-reader",
    "-v",
    "--version",
    "-h",
    "--help",
    "--skip-trust",
    "--acp",
    "--experimental-acp",
    "--raw-output",
    "--accept-raw-output-risk",
];

const VALUE_FLAGS: &[&str] = &[
    "-m",
    "--model",
    "-p",
    "--prompt",
    "-i",
    "--prompt-interactive",
    "--approval-mode",
    "--allowed-mcp-server-names",
    "--allowed-tools",
    "--policy",
    "--admin-policy",
    "-e",
    "--extensions",
    "--session-file",
    "--session-id",
    "--delete-session",
    "--include-directories",
    "-o",
    "--output-format",
];

const OPTIONAL_VALUE_FLAGS: &[&str] = &["--resume", "-r", "-w", "--worktree"];

const REPEATABLE_FLAGS: &[&str] = &[
    "-e",
    "--extensions",
    "--include-directories",
    "--allowed-mcp-server-names",
    "--allowed-tools",
    "--policy",
    "--admin-policy",
];

struct GeminiFlagLookup {
    bool_set: HashSet<String>,
    value_set: HashSet<String>,
    opt_val_set: HashSet<String>,
    repeatable_set: HashSet<String>,
    /// All exact flags for looks_like_flag
    exact_flags: HashSet<String>,
    /// All prefix forms for looks_like_flag
    prefix_flags: Vec<String>,
    /// All known flags for suggestions
    known_flags: Vec<String>,
}

fn flag_lookup() -> &'static GeminiFlagLookup {
    static LOOKUP: OnceLock<GeminiFlagLookup> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        let bool_set: HashSet<String> = BOOLEAN_FLAGS.iter().map(|s| s.to_string()).collect();
        let value_set: HashSet<String> = VALUE_FLAGS.iter().map(|s| s.to_string()).collect();
        let opt_val_set: HashSet<String> =
            OPTIONAL_VALUE_FLAGS.iter().map(|s| s.to_string()).collect();
        let repeatable_set: HashSet<String> =
            REPEATABLE_FLAGS.iter().map(|s| s.to_string()).collect();

        let mut exact_flags = HashSet::new();
        for f in BOOLEAN_FLAGS {
            exact_flags.insert(f.to_string());
        }
        for f in VALUE_FLAGS {
            exact_flags.insert(f.to_string());
        }
        for f in OPTIONAL_VALUE_FLAGS {
            exact_flags.insert(f.to_string());
        }
        for (k, _) in flag_aliases().iter() {
            exact_flags.insert(k.to_string());
        }
        exact_flags.insert("--".to_string());

        let mut prefix_flags = Vec::new();
        for f in VALUE_FLAGS {
            prefix_flags.push(format!("{}=", f));
        }
        for f in OPTIONAL_VALUE_FLAGS {
            prefix_flags.push(format!("{}=", f));
        }

        let mut known_set: HashSet<String> = HashSet::new();
        for f in BOOLEAN_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in VALUE_FLAGS {
            known_set.insert(f.to_string());
        }
        for f in OPTIONAL_VALUE_FLAGS {
            known_set.insert(f.to_string());
        }
        for (k, v) in flag_aliases().iter() {
            known_set.insert(k.to_string());
            known_set.insert(v.to_string());
        }
        let mut known_flags: Vec<String> = known_set.into_iter().collect();
        known_flags.sort();

        GeminiFlagLookup {
            bool_set,
            value_set,
            opt_val_set,
            repeatable_set,
            exact_flags,
            prefix_flags,
            known_flags,
        }
    })
}

fn looks_like_flag(token_lower: &str) -> bool {
    let lookup = flag_lookup();
    args_common::looks_like_flag(token_lower, &lookup.exact_flags, &lookup.prefix_flags)
}

/// Check if token looks like a Gemini session ID (numeric, "latest", or UUID).
fn looks_like_session_id(token: &str) -> bool {
    let lower = token.to_lowercase();
    if lower == "latest" {
        return true;
    }
    if token.chars().all(|c| c.is_ascii_digit()) && !token.is_empty() {
        return true;
    }
    // UUID: 8-4-4-4-12 hex chars
    if lower.len() == 36 {
        let parts: Vec<&str> = lower.split('-').collect();
        if parts.len() == 5
            && parts[0].len() == 8
            && parts[1].len() == 4
            && parts[2].len() == 4
            && parts[3].len() == 4
            && parts[4].len() == 12
            && parts
                .iter()
                .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return true;
        }
    }
    false
}

/// Normalized representation of Gemini CLI arguments.
#[derive(Debug, Clone)]
pub struct GeminiArgsSpec {
    pub source: SourceType,
    pub raw_tokens: Vec<String>,
    pub clean_tokens: Vec<String>,
    pub positional_tokens: Vec<String>,
    pub positional_indexes: Vec<usize>,
    pub flag_values: HashMap<String, FlagValue>,
    pub errors: Vec<String>,
    pub subcommand: Option<String>,
    pub is_headless: bool,
    pub is_json: bool,
    pub is_yolo: bool,
    pub output_format: String,
    pub approval_mode: String,
}

impl GeminiArgsSpec {
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

    /// Get value of a flag. For repeatable flags returns the list joined.
    /// For single-value flags returns the LAST occurrence.
    pub fn get_flag_value(&self, flag_name: &str) -> Option<FlagValue> {
        let flag_lower = flag_name.to_lowercase();
        let aliases = flag_aliases();

        // Build set of possible flag names
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

        // Check pre-parsed flag_values
        for pf in &possible_flags {
            if let Some(val) = self.flag_values.get(pf.as_str()) {
                return Some(val.clone());
            }
        }

        // Fallback: scan clean_tokens
        let lookup = flag_lookup();
        let is_optional = possible_flags
            .iter()
            .any(|pf| lookup.opt_val_set.contains(pf));
        let mut last_value: Option<String> = None;

        let mut i = 0;
        while i < self.clean_tokens.len() {
            let token = &self.clean_tokens[i];
            let token_lower = token.to_lowercase();

            // Check --flag=value form
            let mut found_eq = false;
            for pf in &possible_flags {
                let prefix = format!("{}=", pf);
                if token_lower.starts_with(&prefix) {
                    last_value = Some(token[prefix.len()..].to_string());
                    found_eq = true;
                    break;
                }
            }

            if !found_eq
                && possible_flags.contains(&token_lower)
                && !is_optional
                && i + 1 < self.clean_tokens.len()
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
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        json_output: Option<bool>,
        stream_json: Option<bool>,
        prompt: Option<&str>,
        _subcommand: Option<Option<&str>>,
        yolo: Option<bool>,
        approval_mode: Option<&str>,
        include_directories: Option<&[String]>,
    ) -> GeminiArgsSpec {
        let mut tokens = self.clean_tokens.clone();

        if let Some(y) = yolo {
            tokens = toggle_flag(&tokens, "--yolo", y);
        }

        if let Some(am) = approval_mode {
            tokens = set_value_flag(&tokens, "--approval-mode", am);
        }

        if json_output == Some(true) {
            tokens = set_value_flag(&tokens, "--output-format", "json");
        } else if stream_json == Some(true) {
            tokens = set_value_flag(&tokens, "--output-format", "stream-json");
        }

        if let Some(p) = prompt {
            if p.is_empty() {
                tokens = remove_flag_with_value(&tokens, "-i");
                tokens = remove_flag_with_value(&tokens, "--prompt-interactive");
            } else {
                tokens = set_value_flag(&tokens, "-i", p);
            }
        }

        if let Some(dirs) = include_directories {
            tokens = remove_flag_with_value(&tokens, "--include-directories");
            for dir in dirs {
                tokens.push("--include-directories".to_string());
                tokens.push(dir.clone());
            }
        }

        parse_tokens(&tokens, self.source)
    }
}

/// Resolve Gemini args from CLI (highest precedence) or env string.
pub fn resolve_gemini_args(cli_args: Option<&[String]>, env_value: Option<&str>) -> GeminiArgsSpec {
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
                    vec![format!("invalid Gemini args: {}", e)],
                );
            }
        }
    }

    let empty: &[String] = &[];
    parse_tokens(empty, SourceType::None)
}

/// Merge env and CLI specs with smart precedence rules.
pub fn merge_gemini_args(env_spec: &GeminiArgsSpec, cli_spec: &GeminiArgsSpec) -> GeminiArgsSpec {
    let final_positionals: Vec<String> = if !cli_spec.positional_tokens.is_empty() {
        if cli_spec.positional_tokens == [""] {
            vec![]
        } else {
            cli_spec.positional_tokens.clone()
        }
    } else {
        env_spec.positional_tokens.clone()
    };

    let cli_flag_names = extract_flag_names_from_tokens(&cli_spec.clean_tokens);
    let lookup = flag_lookup();

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
        if let Some(flag_name) = extract_flag_name_from_token(token)
            && cli_flag_names.contains(&flag_name)
        {
            // Repeatable flags: keep env version (CLI will be appended)
            if !lookup.repeatable_set.contains(&flag_name) {
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

    for pos in &final_positionals {
        merged.push(pos.clone());
    }

    parse_tokens(&merged, SourceType::Cli)
}

/// Check for conflicting flag combinations.
pub fn validate_conflicts(spec: &GeminiArgsSpec) -> Vec<String> {
    let mut warnings = Vec::new();

    let has_prompt_flag = spec.has_flag(&["-p", "--prompt"], &["-p=", "--prompt="]);
    if !spec.positional_tokens.is_empty() {
        warnings.push(
            "ERROR: Gemini headless mode (positional query) not supported in hcom.\n\
             Use -i/--prompt-interactive for interactive sessions with initial prompt.\n\
             For headless: use 'hcom N claude -p \"task\"'"
                .to_string(),
        );
    } else if has_prompt_flag {
        warnings.push(
            "ERROR: Gemini headless mode (-p/--prompt flag) not supported in hcom.\n\
             Use -i/--prompt-interactive for interactive sessions with initial prompt.\n\
             For headless: use 'hcom N claude -p \"task\"'"
                .to_string(),
        );
    }

    if spec.is_yolo && spec.has_flag(&["--approval-mode"], &["--approval-mode="]) {
        warnings.push("ERROR: --yolo and --approval-mode cannot be used together".to_string());
    }

    if has_prompt_flag && !spec.positional_tokens.is_empty() {
        warnings
            .push("ERROR: --prompt cannot be used with a positional query argument".to_string());
    }

    let has_interactive = spec.has_flag(
        &["-i", "--prompt-interactive"],
        &["-i=", "--prompt-interactive="],
    );
    if has_prompt_flag && has_interactive {
        warnings
            .push("ERROR: --prompt and --prompt-interactive cannot be used together".to_string());
    }

    if let Some(FlagValue::Single(ref val)) = spec.get_flag_value("--approval-mode")
        && !["default", "auto_edit", "yolo", "plan"].contains(&val.to_lowercase().as_str())
    {
        warnings.push(format!(
            "ERROR: invalid --approval-mode value '{}' (must be: default, auto_edit, yolo, plan)",
            val
        ));
    }

    if let Some(FlagValue::Single(ref val)) = spec.get_flag_value("--output-format")
        && !["text", "json", "stream-json"].contains(&val.to_lowercase().as_str())
    {
        warnings.push(format!(
            "ERROR: invalid --output-format value '{}' (must be: text, json, stream-json)",
            val
        ));
    }

    warnings
}

fn parse_tokens(tokens: &[impl AsRef<str>], source: SourceType) -> GeminiArgsSpec {
    parse_tokens_with_errors(tokens, source, vec![])
}

fn parse_tokens_with_errors(
    tokens: &[impl AsRef<str>],
    source: SourceType,
    initial_errors: Vec<String>,
) -> GeminiArgsSpec {
    let lookup = flag_lookup();
    let raw_tokens: Vec<String> = tokens.iter().map(|t| t.as_ref().to_string()).collect();

    let mut errors = initial_errors;
    let mut clean: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut positional_indexes: Vec<usize> = Vec::new();
    let mut flag_values: HashMap<String, FlagValue> = HashMap::new();

    let subcommand: Option<String> = None;
    let mut is_headless = false;
    let mut is_json = false;
    let mut is_yolo = false;
    let mut has_prompt_interactive = false;
    let mut output_format = "text".to_string();
    let mut approval_mode = "default".to_string();
    let mut pending_flag: Option<String> = None;
    let mut after_double_dash = false;

    let mut i: usize = 0;

    while i < raw_tokens.len() {
        let token = &raw_tokens[i];
        let token_lower = token.to_lowercase();

        // Handle pending value flag
        if let Some(ref pf) = pending_flag.clone() {
            if looks_like_flag(&token_lower) && !after_double_dash {
                errors.push(format!("{} requires a value before '{}'", pf, token));
                pending_flag = None;
                continue; // Don't advance
            }

            let flag_key = pf.to_lowercase();
            let is_repeatable = lookup.repeatable_set.contains(&flag_key);

            if is_repeatable {
                match flag_values.entry(flag_key.clone()) {
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
                flag_values.insert(flag_key.clone(), FlagValue::Single(token.clone()));
            }

            if flag_key == "-p" || flag_key == "--prompt" {
                is_headless = true;
            }
            if flag_key == "-i" || flag_key == "--prompt-interactive" {
                has_prompt_interactive = true;
            }
            if flag_key == "-o" || flag_key == "--output-format" {
                let val_lower = token.to_lowercase();
                if val_lower == "json" || val_lower == "stream-json" {
                    is_json = true;
                    output_format = val_lower;
                } else if val_lower == "text" {
                    output_format = "text".to_string();
                }
            }
            if flag_key == "--approval-mode" {
                let val_lower = token.to_lowercase();
                if val_lower == "yolo" {
                    is_yolo = true;
                    approval_mode = "yolo".to_string();
                } else if ["default", "auto_edit", "plan"].contains(&val_lower.as_str()) {
                    approval_mode = val_lower;
                }
            }

            clean.push(token.clone());
            pending_flag = None;
            i += 1;
            continue;
        }

        // After -- separator
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

        // Boolean flags
        if lookup.bool_set.contains(&token_lower) {
            clean.push(token.clone());
            if token_lower == "--yolo" || token_lower == "-y" {
                is_yolo = true;
                approval_mode = "yolo".to_string();
            }
            i += 1;
            continue;
        }

        // Optional value flags (--resume, -r, --worktree, -w)
        if lookup.opt_val_set.contains(&token_lower) {
            clean.push(token.clone());
            if i + 1 < raw_tokens.len() {
                let next = &raw_tokens[i + 1];
                let next_lower = next.to_lowercase();
                let value_allowed = token_lower == "-w"
                    || token_lower == "--worktree"
                    || looks_like_session_id(next);
                if !looks_like_flag(&next_lower) && value_allowed {
                    flag_values.insert(token_lower.clone(), FlagValue::Single(next.clone()));
                    clean.push(next.clone());
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        // --flag=value syntax
        let mut matched_prefix: Option<String> = None;
        for prefix in &lookup.prefix_flags {
            if token_lower.starts_with(prefix.as_str()) {
                matched_prefix = Some(prefix.clone());
                break;
            }
        }

        if let Some(prefix) = matched_prefix {
            clean.push(token.clone());
            let flag_key = prefix.trim_end_matches('=').to_string();
            let value = token[prefix.len()..].to_string();

            if lookup.repeatable_set.contains(&flag_key) {
                match flag_values.entry(flag_key.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if let FlagValue::List(list) = e.get_mut() {
                            list.push(value.clone());
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(FlagValue::List(vec![value.clone()]));
                    }
                }
            } else {
                flag_values.insert(flag_key.clone(), FlagValue::Single(value.clone()));
            }

            if flag_key == "-p" || flag_key == "--prompt" {
                is_headless = true;
            }
            if flag_key == "-i" || flag_key == "--prompt-interactive" {
                has_prompt_interactive = true;
            }
            if flag_key == "-o" || flag_key == "--output-format" {
                let val_lower = value.to_lowercase();
                if val_lower == "json" || val_lower == "stream-json" {
                    is_json = true;
                    output_format = val_lower;
                } else if val_lower == "text" {
                    output_format = "text".to_string();
                }
            }
            if flag_key == "--approval-mode" {
                let val_lower = value.to_lowercase();
                if val_lower == "yolo" {
                    is_yolo = true;
                    approval_mode = "yolo".to_string();
                } else if ["default", "auto_edit", "plan"].contains(&val_lower.as_str()) {
                    approval_mode = val_lower;
                }
            }

            i += 1;
            continue;
        }

        // Value flags (space-separated)
        if lookup.value_set.contains(&token_lower) {
            clean.push(token.clone());
            pending_flag = Some(token.clone());
            i += 1;
            continue;
        }

        // Unknown flag detection or positional
        let idx = clean.len();
        clean.push(token.clone());

        let is_flag_shaped = token_lower.starts_with("--")
            || (token_lower.starts_with('-')
                && token_lower.len() == 2
                && token_lower
                    .chars()
                    .nth(1)
                    .is_some_and(|c| c.is_alphabetic()));

        if is_flag_shaped && !looks_like_flag(&token_lower) {
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
            i += 1;
            continue;
        }

        if !token_lower.starts_with('-') {
            positional.push(token.clone());
            positional_indexes.push(idx);
            if !has_prompt_interactive {
                is_headless = true;
            }
        }
        i += 1;
    }

    if let Some(ref pf) = pending_flag {
        errors.push(format!("{} requires a value at end of arguments", pf));
    }

    GeminiArgsSpec {
        source,
        raw_tokens,
        clean_tokens: clean,
        positional_tokens: positional,
        positional_indexes,
        flag_values,
        errors,
        subcommand,
        is_headless,
        is_json,
        is_yolo,
        output_format,
        approval_mode,
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
        let spec = resolve_gemini_args(Some(&[]), None);
        assert!(spec.clean_tokens.is_empty());
        assert!(!spec.is_headless);
        assert!(!spec.is_json);
        assert!(!spec.is_yolo);
        assert!(spec.subcommand.is_none());
    }

    #[test]
    fn test_parse_boolean_flags() {
        let args = sv(&["--debug", "--sandbox"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--debug"], &[]));
        assert!(spec.has_flag(&["--sandbox"], &[]));
    }

    #[test]
    fn test_parse_yolo() {
        let args = sv(&["--yolo"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_yolo);
        assert_eq!(spec.approval_mode, "yolo");
    }

    #[test]
    fn test_parse_yolo_short() {
        let args = sv(&["-y"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_yolo);
    }

    #[test]
    fn test_parse_model_value() {
        let args = sv(&["--model", "gemini-2.0"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gemini-2.0".to_string()))
        );
    }

    #[test]
    fn test_parse_model_alias() {
        let args = sv(&["-m", "gemini-2.0"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gemini-2.0".to_string()))
        );
    }

    #[test]
    fn test_parse_model_equals() {
        let args = sv(&["--model=gemini-2.0"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gemini-2.0".to_string()))
        );
    }

    #[test]
    fn test_parse_new_current_value_flags_not_positionals() {
        let args = sv(&[
            "--skip-trust",
            "--acp",
            "--policy",
            "policy.json",
            "--admin-policy=admin.json",
            "--session-file",
            "session.json",
            "--session-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--worktree",
            "branch-name",
        ]);
        let spec = parse_tokens(&args, SourceType::Cli);

        assert!(spec.subcommand.is_none());
        assert!(!spec.has_errors(), "{:?}", spec.errors);
        assert!(spec.positional_tokens.is_empty());
        assert!(spec.has_flag(&["--skip-trust"], &[]));
        assert!(spec.has_flag(&["--acp"], &[]));
        assert_eq!(
            spec.get_flag_value("--session-file"),
            Some(FlagValue::Single("session.json".to_string()))
        );
        assert_eq!(
            spec.get_flag_value("--session-id"),
            Some(FlagValue::Single(
                "550e8400-e29b-41d4-a716-446655440000".to_string()
            ))
        );
        assert_eq!(
            spec.get_flag_value("--worktree"),
            Some(FlagValue::Single("branch-name".to_string()))
        );
    }

    #[test]
    fn test_parse_headless_positional() {
        let args = sv(&["explain this code"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_headless);
        assert_eq!(spec.positional_tokens, vec!["explain this code"]);
    }

    #[test]
    fn test_parse_headless_prompt_flag() {
        let args = sv(&["-p", "explain this"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_headless);
    }

    #[test]
    fn test_parse_interactive_prompt_not_headless() {
        let args = sv(&["-i", "explain this"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(!spec.is_headless);
    }

    #[test]
    fn test_parse_json_output() {
        let args = sv(&["--output-format", "json"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_json);
        assert_eq!(spec.output_format, "json");
    }

    #[test]
    fn test_parse_stream_json() {
        let args = sv(&["-o", "stream-json"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_json);
        assert_eq!(spec.output_format, "stream-json");
    }

    #[test]
    fn test_parse_approval_mode_yolo() {
        let args = sv(&["--approval-mode", "yolo"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.is_yolo);
        assert_eq!(spec.approval_mode, "yolo");
    }

    #[test]
    fn test_parse_repeatable_extensions() {
        let args = sv(&["-e", "ext1", "-e", "ext2"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        match spec.get_flag_value("-e") {
            Some(FlagValue::List(list)) => assert_eq!(list, vec!["ext1", "ext2"]),
            other => panic!("expected list, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_resume_with_session_id() {
        let args = sv(&["--resume", "123"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--resume"),
            Some(FlagValue::Single("123".to_string()))
        );
    }

    #[test]
    fn test_parse_resume_with_latest() {
        let args = sv(&["--resume", "latest"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert_eq!(
            spec.get_flag_value("--resume"),
            Some(FlagValue::Single("latest".to_string()))
        );
    }

    #[test]
    fn test_parse_resume_without_value() {
        let args = sv(&["--resume", "--debug"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        // --resume should be present but not consume --debug
        assert!(spec.has_flag(&["--resume"], &[]));
        assert!(spec.has_flag(&["--debug"], &[]));
    }

    #[test]
    fn test_parse_missing_value_error() {
        let args = sv(&["--model"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_errors());
        assert!(spec.errors[0].contains("requires a value"));
    }

    #[test]
    fn test_parse_double_dash() {
        let args = sv(&["--debug", "--", "--not-a-flag"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        assert!(spec.has_flag(&["--debug"], &[]));
        assert_eq!(spec.positional_tokens, vec!["--not-a-flag"]);
    }

    #[test]
    fn test_resolve_from_env() {
        let spec = resolve_gemini_args(None, Some("--model gemini-2.0 --debug"));
        assert_eq!(
            spec.get_flag_value("--model"),
            Some(FlagValue::Single("gemini-2.0".to_string()))
        );
        assert!(spec.has_flag(&["--debug"], &[]));
    }

    #[test]
    fn test_resolve_invalid_env() {
        let spec = resolve_gemini_args(None, Some("'unterminated"));
        assert!(spec.has_errors());
    }

    #[test]
    fn test_merge_cli_overrides_env() {
        let env_spec = parse_tokens(&sv(&["--model", "old", "--debug"]), SourceType::Env);
        let cli_spec = parse_tokens(&sv(&["--model", "new"]), SourceType::Cli);
        let merged = merge_gemini_args(&env_spec, &cli_spec);
        assert_eq!(
            merged.get_flag_value("--model"),
            Some(FlagValue::Single("new".to_string()))
        );
        assert!(merged.has_flag(&["--debug"], &[]));
    }

    #[test]
    fn test_merge_repeatable_flags() {
        let env_spec = parse_tokens(&sv(&["-e", "ext1"]), SourceType::Env);
        let cli_spec = parse_tokens(&sv(&["-e", "ext2"]), SourceType::Cli);
        let merged = merge_gemini_args(&env_spec, &cli_spec);
        // Both should be present
        match merged.get_flag_value("-e") {
            Some(FlagValue::List(list)) => {
                assert!(list.contains(&"ext1".to_string()));
                assert!(list.contains(&"ext2".to_string()));
            }
            other => panic!("expected list with both extensions, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_headless_positional() {
        let spec = parse_tokens(&sv(&["explain this"]), SourceType::Cli);
        let warnings = validate_conflicts(&spec);
        assert!(warnings.iter().any(|w| w.contains("headless mode")));
    }

    #[test]
    fn test_validate_headless_prompt_flag() {
        let spec = parse_tokens(&sv(&["-p", "explain"]), SourceType::Cli);
        let warnings = validate_conflicts(&spec);
        assert!(warnings.iter().any(|w| w.contains("headless mode")));
    }

    #[test]
    fn test_validate_no_conflicts() {
        let spec = parse_tokens(
            &sv(&["--model", "gemini-2.0", "-i", "hello"]),
            SourceType::Cli,
        );
        assert!(validate_conflicts(&spec).is_empty());
    }

    #[test]
    fn test_to_env_string() {
        let spec = parse_tokens(&sv(&["--model", "gemini-2.0"]), SourceType::Cli);
        let env_str = spec.to_env_string();
        assert!(env_str.contains("--model"));
        assert!(env_str.contains("gemini-2.0"));
    }

    #[test]
    fn test_rebuild_tokens_ignores_subcommand_parameter() {
        let args = sv(&["--model", "gemini-2.0"]);
        let spec = parse_tokens(&args, SourceType::Cli);
        let tokens = spec.rebuild_tokens(true, false);
        assert_eq!(tokens[0], "--model");
    }

    #[test]
    fn test_session_id_detection() {
        assert!(looks_like_session_id("123"));
        assert!(looks_like_session_id("latest"));
        assert!(looks_like_session_id(
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        ));
        assert!(!looks_like_session_id("explain this"));
        assert!(!looks_like_session_id("--debug"));
    }

    #[test]
    fn test_update_set_yolo() {
        let spec = parse_tokens(&sv(&["--model", "gemini-2.0"]), SourceType::Cli);
        let updated = spec.update(None, None, None, None, Some(true), None, None);
        assert!(updated.is_yolo);
    }

    #[test]
    fn test_update_set_prompt() {
        let spec = parse_tokens(&sv(&["--debug"]), SourceType::Cli);
        let updated = spec.update(None, None, Some("hello"), None, None, None, None);
        assert!(updated.has_flag(&["-i"], &[]));
    }
}
