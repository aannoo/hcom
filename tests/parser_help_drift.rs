use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy)]
enum CommandStyle {
    Bare,
    Prefixed(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpValueKind {
    Boolean,
    Required,
    Optional,
    Ambiguous,
}

#[derive(Debug, Clone)]
struct HelpOption {
    token: String,
    kind: HelpValueKind,
    command_path: Vec<String>,
    line: String,
}

#[derive(Debug)]
struct HelpPage {
    command_path: Vec<String>,
    text: String,
}

#[derive(Debug, Default)]
struct ParserTables {
    boolean: BTreeSet<String>,
    value: BTreeSet<String>,
    optional: BTreeSet<String>,
    subcommands: BTreeSet<String>,
    aliases: BTreeMap<String, String>,
}

impl ParserTables {
    fn recognized_kind(&self, token: &str) -> Option<HelpValueKind> {
        let key = normalize_token(token);
        if self.boolean.contains(&key) {
            return Some(HelpValueKind::Boolean);
        }
        if self.value.contains(&key) {
            return Some(HelpValueKind::Required);
        }
        if self.optional.contains(&key) {
            return Some(HelpValueKind::Optional);
        }
        if let Some(canonical) = self.aliases.get(&key) {
            return self.recognized_kind(canonical);
        }
        None
    }
}

fn parser_source(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn help(tool: &str, args: &[String]) -> String {
    let output = Command::new(tool)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap_or_else(|err| panic!("run {tool} {args:?}: {err}"));
    assert!(
        output.status.success(),
        "{tool} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

fn collect_help_pages(tool: &str, style: CommandStyle, max_depth: usize) -> Vec<HelpPage> {
    let mut pages = Vec::new();
    let mut queue = VecDeque::from([Vec::<String>::new()]);
    let mut seen = BTreeSet::new();

    while let Some(command_path) = queue.pop_front() {
        if !seen.insert(command_path.clone()) {
            continue;
        }

        let mut args = command_path.clone();
        args.push("--help".to_string());
        let text = help(tool, &args);

        if command_path.len() < max_depth {
            for command in command_tokens(&text, style) {
                if command == "help" {
                    continue;
                }
                let mut child_path = command_path.clone();
                child_path.push(command);
                queue.push_back(child_path);
            }
        }

        pages.push(HelpPage { command_path, text });
    }

    pages
}

fn collect_help_pages_for_paths(tool: &str, command_paths: &[&[&str]]) -> Vec<HelpPage> {
    command_paths
        .iter()
        .map(|path| {
            let command_path = path.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let mut args = command_path.clone();
            args.push("--help".to_string());
            let text = help(tool, &args);
            HelpPage { command_path, text }
        })
        .collect()
}

fn option_tokens(page: &HelpPage) -> Vec<HelpOption> {
    let mut options = Vec::new();

    for line in page.text.lines() {
        if !is_help_table_row(line) {
            continue;
        }
        let trimmed = line.trim_start();

        let tokens = flag_tokens(split_option_part(trimmed));
        if tokens.is_empty() {
            continue;
        }

        let kind = classify_option_line(trimmed);
        for token in tokens {
            options.push(HelpOption {
                token,
                kind,
                command_path: page.command_path.clone(),
                line: trimmed.to_string(),
            });
        }
    }

    options
}

fn is_help_table_row(line: &str) -> bool {
    let indent = line.chars().take_while(|c| c.is_whitespace()).count();
    indent <= 6 && line.trim_start().starts_with('-')
}

fn flag_tokens(line: &str) -> Vec<String> {
    line.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|raw| {
            let token = raw
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_end_matches(',')
                .split('=')
                .next()
                .unwrap_or("");
            let is_long = token.starts_with("--")
                && token[2..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-');
            let is_short = token.starts_with('-')
                && !token.starts_with("--")
                && token.len() == 2
                && token
                    .chars()
                    .nth(1)
                    .is_some_and(|c| c.is_ascii_alphabetic());
            if is_long || is_short {
                Some(token.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn classify_option_line(line: &str) -> HelpValueKind {
    let option_part = split_option_part(line);

    if option_part.contains('[') {
        return HelpValueKind::Optional;
    }
    if option_part.contains('<') {
        return HelpValueKind::Required;
    }
    if line.contains("[boolean]") {
        return HelpValueKind::Boolean;
    }
    if line.contains("[string]") || line.contains("[array]") || line.contains("[number]") {
        return HelpValueKind::Required;
    }

    HelpValueKind::Ambiguous
}

fn split_option_part(line: &str) -> &str {
    let bytes = line.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b' ' && bytes[i + 1] == b' ' {
            return &line[..i];
        }
    }
    line
}

fn command_tokens(help: &str, style: CommandStyle) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    let mut in_commands = false;

    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if trimmed.is_empty() || trimmed.ends_with(':') {
            break;
        }
        if !line.starts_with("  ") || line[2..].starts_with(' ') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let command = match style {
            CommandStyle::Bare => parts.next(),
            CommandStyle::Prefixed(prefix) => {
                if parts.next() != Some(prefix) {
                    continue;
                }
                parts.next()
            }
        };

        let Some(command) = command else {
            continue;
        };
        if command.starts_with('[') || command.starts_with('<') {
            continue;
        }

        let command = command.split('|').next().unwrap_or(command);
        commands.insert(command.to_string());
    }

    commands
}

fn parser_tables(path: &str, kind: &str) -> ParserTables {
    let source = parser_source(path);
    let mut tables = ParserTables::default();

    tables
        .boolean
        .extend(strings_in_const(&source, "BOOLEAN_FLAGS"));
    tables
        .boolean
        .extend(strings_in_const(&source, "BACKGROUND_SWITCHES"));
    tables
        .boolean
        .extend(strings_in_const(&source, "FORK_SWITCHES"));
    tables
        .boolean
        .extend(strings_in_const(&source, "CASE_SENSITIVE_BOOLEAN_FLAGS"));

    tables
        .value
        .extend(strings_in_const(&source, "VALUE_FLAGS"));
    tables
        .value
        .extend(strings_in_const(&source, "CASE_SENSITIVE_VALUE_FLAGS"));
    tables
        .optional
        .extend(strings_in_const(&source, "OPTIONAL_VALUE_FLAGS"));
    tables
        .subcommands
        .extend(strings_in_const(&source, "SUBCOMMANDS"));
    tables.aliases.extend(alias_inserts(&source));

    if kind != "codex" {
        tables = tables.normalized();
    }

    tables
}

impl ParserTables {
    fn normalized(self) -> Self {
        ParserTables {
            boolean: self
                .boolean
                .into_iter()
                .map(|s| normalize_token(&s))
                .collect(),
            value: self
                .value
                .into_iter()
                .map(|s| normalize_token(&s))
                .collect(),
            optional: self
                .optional
                .into_iter()
                .map(|s| normalize_token(&s))
                .collect(),
            subcommands: self.subcommands,
            aliases: self
                .aliases
                .into_iter()
                .map(|(k, v)| (normalize_token(&k), normalize_token(&v)))
                .collect(),
        }
    }
}

fn strings_in_const(source: &str, const_name: &str) -> BTreeSet<String> {
    let Some(start) = source.find(&format!("const {const_name}:")) else {
        return BTreeSet::new();
    };
    let Some(end) = source[start..].find("];") else {
        return BTreeSet::new();
    };
    quoted_strings(&source[start..start + end])
        .into_iter()
        .map(|s| normalize_token(&s))
        .collect()
}

fn alias_inserts(source: &str) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    for line in source.lines() {
        let trimmed = line.trim();
        let Some(args) = trimmed.strip_prefix("m.insert(") else {
            continue;
        };
        let strings = quoted_strings(args);
        if strings.len() == 2 {
            aliases.insert(normalize_token(&strings[0]), normalize_token(&strings[1]));
        }
    }
    aliases
}

fn quoted_strings(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut value = String::new();
        let mut escaped = false;
        for c in chars.by_ref() {
            if escaped {
                value.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                break;
            } else {
                value.push(c);
            }
        }
        out.push(value);
    }
    out
}

fn normalize_token(token: &str) -> String {
    if token == "-C" || token == "-V" || token == "-H" {
        token.to_string()
    } else {
        token.to_lowercase()
    }
}

fn assert_help_options_match_parser(path: &str, kind: &str, pages: &[HelpPage]) {
    let tables = parser_tables(path, kind);
    let mut missing = Vec::new();
    let mut misclassified = Vec::new();

    for option in pages.iter().flat_map(option_tokens) {
        let actual = tables.recognized_kind(&option.token);
        match actual {
            None => missing.push(format!(
                "{} from `{}`: {}",
                option.token,
                command_display(kind, &option.command_path),
                option.line
            )),
            Some(actual) if !kind_allows(option.kind, actual) => misclassified.push(format!(
                "{} from `{}` looks {:?}, parser has {:?}: {}",
                option.token,
                command_display(kind, &option.command_path),
                option.kind,
                actual,
                option.line
            )),
            Some(_) => {}
        }
    }

    assert!(
        missing.is_empty() && misclassified.is_empty(),
        "{path} parser table drift against installed {kind}\nmissing:\n{}\nmisclassified:\n{}",
        missing.join("\n"),
        misclassified.join("\n")
    );
}

fn kind_allows(help_kind: HelpValueKind, parser_kind: HelpValueKind) -> bool {
    match help_kind {
        HelpValueKind::Boolean | HelpValueKind::Ambiguous => {
            matches!(
                parser_kind,
                HelpValueKind::Boolean | HelpValueKind::Optional
            )
        }
        HelpValueKind::Required => {
            matches!(
                parser_kind,
                HelpValueKind::Required | HelpValueKind::Optional
            )
        }
        HelpValueKind::Optional => matches!(parser_kind, HelpValueKind::Optional),
    }
}

fn command_display(tool: &str, command_path: &[String]) -> String {
    if command_path.is_empty() {
        tool.to_string()
    } else {
        format!("{tool} {}", command_path.join(" "))
    }
}

#[test]
#[ignore = "manual release drift guard: requires installed upstream CLIs on PATH"]
fn installed_gemini_help_is_represented_in_parser_tables() {
    let pages = collect_help_pages("gemini", CommandStyle::Prefixed("gemini"), 0);
    assert_help_options_match_parser("src/tools/gemini_args.rs", "gemini", &pages);
}

#[test]
#[ignore = "manual release drift guard: requires installed upstream CLIs on PATH"]
fn installed_codex_help_is_represented_in_parser_tables() {
    let mut pages = collect_help_pages("codex", CommandStyle::Bare, 0);
    pages.extend(collect_help_pages_for_paths(
        "codex",
        &[&["resume"], &["fork"]],
    ));
    assert_help_options_match_parser("src/tools/codex_args.rs", "codex", &pages);
}

#[test]
#[ignore = "manual release drift guard: requires installed upstream CLIs on PATH"]
fn installed_claude_help_is_represented_in_parser_tables() {
    let pages = collect_help_pages("claude", CommandStyle::Bare, 0);
    assert_help_options_match_parser("src/hooks/claude_args.rs", "claude", &pages);
}

#[test]
#[ignore = "manual release drift guard: requires installed upstream CLIs on PATH"]
fn installed_opencode_help_crawls_but_hcom_keeps_opencode_args_pass_through() {
    let pages = collect_help_pages("opencode", CommandStyle::Prefixed("opencode"), 0);
    let root_options = option_tokens(&pages[0]);
    let option_count = root_options.len();
    assert!(option_count > 0, "opencode help crawl found no options");
    let root_option_tokens = root_options
        .iter()
        .map(|option| option.token.as_str())
        .collect::<BTreeSet<_>>();
    for token in ["--agent", "--model", "-m"] {
        assert!(
            root_option_tokens.contains(token),
            "hcom observes OpenCode launch arg {token}, but installed opencode root help does not list it"
        );
    }
    let model_option = root_options
        .iter()
        .find(|option| option.token == "--model" || option.token == "-m")
        .expect("model option should be present in OpenCode help");
    assert!(
        model_option.line.contains("provider/model"),
        "hcom expects OpenCode --model/-m values to be provider/model, but help line changed: {}",
        model_option.line
    );

    let launcher = parser_source("src/launcher.rs");
    let launch_command = parser_source("src/commands/launch.rs");
    assert!(
        launcher.contains(
            "LaunchTool::OpenCode | LaunchTool::Kilo | LaunchTool::Antigravity => Vec::new(),"
        ),
        "OpenCode has no hcom parser table; if validation is added, add opencode to this drift guard"
    );
    assert!(
        launch_command.contains("_ => cli_args.to_vec(), // OpenCode-family tools: pass through"),
        "OpenCode launch args should remain pass-through unless an opencode parser drift guard is added"
    );
}

#[test]
#[ignore = "manual release drift guard: requires installed upstream CLIs on PATH"]
fn installed_kilo_help_crawls_but_hcom_keeps_kilo_args_pass_through() {
    // Kilo is an OpenCode-family fork: same launch surface (`--agent`,
    // `--model provider/model`) and the same pass-through handling in hcom.
    let pages = collect_help_pages("kilo", CommandStyle::Prefixed("kilo"), 0);
    let root_options = option_tokens(&pages[0]);
    let option_count = root_options.len();
    assert!(option_count > 0, "kilo help crawl found no options");
    let root_option_tokens = root_options
        .iter()
        .map(|option| option.token.as_str())
        .collect::<BTreeSet<_>>();
    for token in ["--agent", "--model", "-m"] {
        assert!(
            root_option_tokens.contains(token),
            "hcom observes Kilo launch arg {token}, but installed kilo root help does not list it"
        );
    }
    let model_option = root_options
        .iter()
        .find(|option| option.token == "--model" || option.token == "-m")
        .expect("model option should be present in Kilo help");
    assert!(
        model_option.line.contains("provider/model"),
        "hcom expects Kilo --model/-m values to be provider/model, but help line changed: {}",
        model_option.line
    );

    // Kilo shares OpenCode's pass-through launch handling; the invariants live in
    // the same source lines the opencode guard asserts.
    let launcher = parser_source("src/launcher.rs");
    let launch_command = parser_source("src/commands/launch.rs");
    assert!(
        launcher.contains(
            "LaunchTool::OpenCode | LaunchTool::Kilo | LaunchTool::Antigravity => Vec::new(),"
        ),
        "Kilo has no hcom parser table; if validation is added, add kilo to this drift guard"
    );
    assert!(
        launch_command.contains("_ => cli_args.to_vec(), // OpenCode-family tools: pass through"),
        "Kilo launch args should remain pass-through unless a kilo parser drift guard is added"
    );
}
