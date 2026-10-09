//! `hcom run` command — execute scripts from embedded bundled scripts and ~/.hcom/scripts/.
//!
//! Bundled scripts are compiled into the binary via `scripts::SCRIPTS`.
//! User scripts in `~/.hcom/scripts/` still discovered from disk and shadow bundled.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::commands::config::{CONFIG_KEYS, config_help};
use crate::commands::help;
use crate::db::HcomDb;
use crate::paths::scripts_dir;
use crate::scripts;
use crate::shared::CommandContext;

#[cfg(windows)]
fn resolve_git_bash() -> Result<String, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    find_git_bash(&path, |bash| {
        // WSL's launcher can successfully run Bash once a distro is installed,
        // but that Bash cannot open the Windows paths passed to our scripts.
        Command::new(bash)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "case \"$OSTYPE\" in msys*|cygwin*) exit 0 ;; *) exit 1 ;; esac",
            ])
            .output()
            .is_ok_and(|output| output.status.success())
    })
    .map(|bash| bash.to_string_lossy().into_owned())
    .ok_or_else(|| format!("{BASH_MISSING_MSG}\nThe Windows WSL launcher is not sufficient."))
}

#[cfg(windows)]
fn find_git_bash(path: &std::ffi::OsStr, is_git_bash: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join("bash.exe"))
        .find(|bash| bash.is_file() && is_git_bash(bash))
}
#[cfg(windows)]
const BASH_MISSING_MSG: &str = "Git Bash required to run shell (.sh) workflow scripts — install it and ensure `bash` is on PATH.";

fn python_command() -> Result<Command, String> {
    if let Ok(program) = std::env::var("PYTHON")
        && !program.trim().is_empty()
    {
        return Ok(Command::new(program));
    }

    #[cfg(windows)]
    {
        for (name, args) in [("python", &[][..]), ("py", &["-3"][..])] {
            let Some(program) = crate::terminal::which_bin(name) else {
                continue;
            };
            let mut probe = Command::new(&program);
            probe.args(args).arg("--version");
            if probe.output().is_ok_and(|output| output.status.success()) {
                let mut command = Command::new(program);
                command.args(args);
                return Ok(command);
            }
        }
        Err(
            "Python 3 is required to run .py workflow scripts. Install Python, set `PYTHON`, \
             or ensure `python`/`py` is on PATH."
                .to_string(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new("python3"))
    }
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "run",
    about = "Run a bundled or user workflow script",
    disable_help_flag = true
)]
pub struct RunArgs {
    /// Script name plus forwarded args
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Bundled script agent descriptions.
fn bundled_agent_desc(name: &str) -> &'static str {
    match name {
        "confess" => "3 agents",
        "debate" => "2+ agents",
        "fatcow" => "1 agent",
        _ => "",
    }
}

/// Script info.
enum ScriptSource {
    /// Embedded bundled script — content is in memory.
    Bundled { content: &'static str },
    /// User script on disk.
    User { path: PathBuf },
}

struct ScriptInfo {
    name: String,
    source: ScriptSource,
    description: String,
}

/// Extract first comment line as description from shell script content.
fn extract_description_from_content(content: &str) -> String {
    for line in content.lines() {
        let stripped = line.trim();
        if stripped.starts_with("#!") {
            continue;
        }
        if stripped.starts_with('#') {
            return stripped
                .strip_prefix('#')
                .unwrap_or(stripped)
                .trim()
                .to_string();
        }
        if !stripped.is_empty() {
            break;
        }
    }
    String::new()
}

/// Extract first line of docstring (Python) or comment (shell) as description from a file.
fn extract_description(path: &Path) -> String {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "py" => {
            let mut in_docstring = false;
            for line in content.lines() {
                let stripped = line.trim();
                if stripped.starts_with("\"\"\"") || stripped.starts_with("'''") {
                    let quote = &stripped[..3];
                    if stripped.matches(quote).count() >= 2 {
                        return stripped.trim_matches(['"', '\'']).trim().to_string();
                    }
                    in_docstring = true;
                    let rest = stripped[3..].trim();
                    if !rest.is_empty() {
                        return rest.trim_end_matches(['"', '\'']).trim().to_string();
                    }
                } else if in_docstring {
                    if stripped.ends_with("\"\"\"") || stripped.ends_with("'''") {
                        return stripped.trim_end_matches(['"', '\'']).trim().to_string();
                    }
                    if !stripped.is_empty() {
                        return stripped.to_string();
                    }
                }
            }
            String::new()
        }
        "sh" => extract_description_from_content(&content),
        _ => String::new(),
    }
}

/// Discover all available scripts (user scripts shadow bundled).
fn discover_scripts() -> Vec<ScriptInfo> {
    let user_dir = scripts_dir();
    let mut scripts = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // User scripts first (they shadow bundled)
    if user_dir.exists() {
        for ext in &["py", "sh"] {
            if let Ok(rd) = std::fs::read_dir(&user_dir) {
                let mut entries: Vec<PathBuf> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().and_then(|e| e.to_str()) == Some(ext)
                            && !p
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with('_') || n.starts_with('.'))
                    })
                    .collect();
                entries.sort();

                for path in entries {
                    let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
                    if !name.is_empty() && !seen.contains(name) {
                        seen.insert(name.to_string());
                        let desc = extract_description(&path);
                        scripts.push(ScriptInfo {
                            name: name.to_string(),
                            source: ScriptSource::User { path },
                            description: desc,
                        });
                    }
                }
            }
        }
    }

    // Bundled scripts from embedded const (if not shadowed by user)
    for (name, content) in scripts::SCRIPTS {
        if !seen.contains(*name) {
            seen.insert(name.to_string());
            let desc = extract_description_from_content(content);
            scripts.push(ScriptInfo {
                name: name.to_string(),
                source: ScriptSource::Bundled { content },
                description: desc,
            });
        }
    }

    scripts
}

/// Find a script by name.
fn find_script(name: &str) -> Option<ScriptInfo> {
    discover_scripts().into_iter().find(|s| s.name == name)
}

/// List all available scripts.
fn list_scripts() -> i32 {
    let scripts = discover_scripts();

    let bundled: Vec<&ScriptInfo> = scripts
        .iter()
        .filter(|s| matches!(s.source, ScriptSource::Bundled { .. }))
        .collect();
    let user: Vec<&ScriptInfo> = scripts
        .iter()
        .filter(|s| matches!(s.source, ScriptSource::User { .. }))
        .collect();

    if scripts.is_empty() {
        println!("No scripts available");
        return 0;
    }

    if !bundled.is_empty() {
        println!("Bundled Scripts:");
        println!();
        for s in &bundled {
            let agents = bundled_agent_desc(&s.name);
            let agents_part = if agents.is_empty() {
                String::new()
            } else {
                format!("  ({agents})")
            };
            println!("  {}{agents_part}", s.name);
            println!("      {}", s.description);
        }
        println!();
    }

    if !user.is_empty() {
        println!("User Scripts:");
        println!();
        for s in &user {
            println!("  {}", s.name);
            println!("      {}", s.description);
        }
        println!();
    } else {
        println!("User Scripts:");
        println!();
        println!("  No custom scripts yet.");
        println!();
        println!("  Run 'hcom run docs' to create a custom script:");
        println!("    - Multi-agent workflows");
        println!("    - Background watchers");
        println!("    - Task automation");
        println!("    - etc...");
        println!();
    }

    println!("Commands:");
    println!("  hcom run <script>           Run workflow script");
    println!("  hcom run <script> --source  View script source");
    println!("  hcom run <script> --help    Script help");
    println!("  hcom run docs               CLI reference + config + script guide");
    println!("    --cli                     CLI reference only");
    println!("    --config                  Config settings only");
    println!("    --scripts                 Script creation guide");

    0
}

/// Write embedded script content to a temp file and return the path.
fn write_embedded_to_temp(name: &str, content: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("hcom-{name}-"))
        .suffix(".sh")
        .tempfile()?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = tmp.as_file().metadata()?;
        let mut perms = metadata.permissions();
        perms.set_mode(0o755);
        tmp.as_file().set_permissions(perms)?;
    }
    Ok(tmp)
}

pub fn cmd_run(db: &HcomDb, args: &RunArgs, ctx: Option<&CommandContext>) -> i32 {
    // Re-inject --name for scripts that parse it themselves
    let mut argv = args.args.clone();
    if let Some(ctx) = ctx
        && let Some(ref name) = ctx.explicit_name
    {
        let canonical =
            crate::identity::resolve_display_name(db, name).unwrap_or_else(|| name.clone());
        argv = vec!["--name".to_string(), canonical]
            .into_iter()
            .chain(argv)
            .collect();
    }

    if argv.is_empty() {
        return list_scripts();
    }

    // `run` disables clap's built-in help flag so script-specific `--help`
    // can be forwarded. Handle help here when there is no script name.
    let run_help_only = {
        let mut saw_help = false;
        let mut only_help = true;
        let mut i = 0;
        while i < argv.len() {
            if argv[i] == "--name" && i + 1 < argv.len() {
                i += 2;
            } else if argv[i] == "-h" || argv[i] == "--help" {
                saw_help = true;
                i += 1;
            } else {
                only_help = false;
                break;
            }
        }
        saw_help && only_help
    };
    if run_help_only {
        println!("{}", help::get_command_help("run"));
        return 0;
    }

    // Handle --source flag
    let show_source = argv.iter().any(|a| a == "--source");
    let argv: Vec<String> = argv.into_iter().filter(|a| a != "--source").collect();

    // Find script name (skip --name flag and value)
    let mut script_idx = None;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--name" && i + 1 < argv.len() {
            i += 2;
        } else if argv[i].starts_with('-') {
            i += 1;
        } else {
            script_idx = Some(i);
            break;
        }
    }

    let script_idx = match script_idx {
        Some(idx) => idx,
        None => return list_scripts(),
    };

    let name = &argv[script_idx];
    let mut args: Vec<String> = argv[..script_idx].to_vec();
    args.extend(argv[script_idx + 1..].to_vec());

    // Special: docs command
    if name == "docs" {
        let mut docs_args = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--name" && i + 1 < args.len() {
                i += 2;
                continue;
            }
            docs_args.push(args[i].as_str());
            i += 1;
        }

        if docs_args.iter().any(|a| *a == "-h" || *a == "--help") {
            println!("{}", help::get_command_help("run"));
            return 0;
        }

        let unknown: Vec<&str> = docs_args
            .iter()
            .copied()
            .filter(|a| !matches!(*a, "--cli" | "--config" | "--scripts"))
            .collect();
        if !unknown.is_empty() {
            eprintln!("Unknown docs option: {}", unknown.join(", "));
            eprintln!("Run 'hcom run docs --help' for available sections.");
            return 1;
        }

        let show_cli = docs_args.contains(&"--cli");
        let show_config = docs_args.contains(&"--config");
        let show_api = docs_args.contains(&"--scripts");
        return print_docs(show_cli, show_config, show_api);
    }

    // Find script
    let script = match find_script(name) {
        Some(s) => s,
        None => {
            println!("Unknown script: {name}");
            println!("Run 'hcom run' to list available scripts");
            return 1;
        }
    };

    // --source: print source and exit
    if show_source {
        match &script.source {
            ScriptSource::Bundled { content } => {
                print!("{content}");
                return 0;
            }
            ScriptSource::User { path } => match std::fs::read_to_string(path) {
                Ok(content) => {
                    print!("{content}");
                    return 0;
                }
                Err(e) => {
                    eprintln!("Error reading {}: {e}", path.display());
                    return 1;
                }
            },
        }
    }

    // Run the script
    match &script.source {
        ScriptSource::User { path } => {
            let mut cmd = if path.extension().and_then(|e| e.to_str()) == Some("py") {
                let mut c = match python_command() {
                    Ok(command) => command,
                    Err(err) => {
                        eprintln!("{err}");
                        return 1;
                    }
                };
                c.arg(path);
                c.args(&args);
                c
            } else {
                #[cfg(windows)]
                let bash = match resolve_git_bash() {
                    Ok(bash) => bash,
                    Err(err) => {
                        eprintln!("{err}");
                        return 1;
                    }
                };
                #[cfg(not(windows))]
                let bash = "bash";
                let mut c = Command::new(bash);
                c.arg(path);
                c.args(&args);
                c
            };

            match cmd
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
            {
                Ok(status) => status.code().unwrap_or(1),
                Err(e) => {
                    eprintln!("Error running {}: {e}", path.display());
                    1
                }
            }
        }
        ScriptSource::Bundled { content } => {
            // Write to temp file and execute
            let tmp = match write_embedded_to_temp(name, content) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error creating temp file for {name}: {e}");
                    return 1;
                }
            };

            #[cfg(windows)]
            let bash = match resolve_git_bash() {
                Ok(bash) => bash,
                Err(err) => {
                    eprintln!("{err}");
                    return 1;
                }
            };
            #[cfg(not(windows))]
            let bash = "bash";
            let mut cmd = Command::new(bash);
            cmd.arg(tmp.path());
            cmd.args(&args);

            match cmd
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
            {
                Ok(status) => status.code().unwrap_or(1),
                Err(e) => {
                    eprintln!("Error running {name}: {e}");
                    1
                }
            }
        }
    }
}

// ── Docs command ────────────────────────────────────────────────────────

fn print_terminal_help() {
    use crate::commands::config::terminal_help_text;
    println!("{}", terminal_help_text(false));
}

const SCRIPT_GUIDE: &str = r#"# Creating Custom Scripts

## Location

  User scripts:    ~/.hcom/scripts/
  File types:      *.sh (bash), *.py (Python 3)

*.sh scripts (including the bundled confess/debate/fatcow workflows) run via
`bash`. On Windows, install Git Bash (already a common dependency for
Windows dev setups, including npm-installed AI CLIs) so `bash` is on PATH.
Python scripts use `PYTHON` when set; otherwise Windows tries `python`, then
the Python launcher (`py -3`).

User scripts shadow bundled scripts with the same name.
Scripts are discovered automatically — drop a file and run `hcom run <name>`.
Add a description comment on line 2 (after shebang) — it shows in `hcom run` listings.

## Shell Script Template

  #!/usr/bin/env bash
  # Brief description shown in hcom run list.
  set -euo pipefail

  name_flag=""
  target=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -h|--help) echo "Usage: hcom run myscript [OPTIONS]"; exit 0 ;;
      --name|--target)
        [[ $# -ge 2 && -n "$2" ]] || { echo "$1 requires a value" >&2; exit 1; }
        if [[ "$1" == --name ]]; then name_flag="$2"; else target="$2"; fi
        shift 2 ;;
      *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
  done

  name_arg=()
  [[ -n "$name_flag" ]] && name_arg=(--name "$name_flag")

  [[ -n "$target" ]] || { echo "--target is required" >&2; exit 1; }

  # Your logic here
  hcom send "@${target}" ${name_arg[@]+"${name_arg[@]}"} --intent request -- "Do the task"

## Identity Handling

hcom passes --name to scripts automatically. Always parse and forward it:

  name_arg=()
  [[ -n "$name_flag" ]] && name_arg=(--name "$name_flag")
  hcom send @target ${name_arg[@]+"${name_arg[@]}"} -- "message"
  hcom list self --json ${name_arg[@]+"${name_arg[@]}"}

The conditional array expansion also works with macOS Bash 3.2 under `set -u`.

## Launching & Cleaning Up Agents

Launch output includes "Names: <name>" — parse to track spawned agents:

  LAUNCHED_NAMES=()
  track_launch() {
    local output="$1"
    local names
    names=$(echo "$output" | grep '^Names: ' | sed 's/^Names: //')
    for n in $names; do LAUNCHED_NAMES+=("$n"); done
  }
  cleanup() {
    for name in ${LAUNCHED_NAMES[@]+"${LAUNCHED_NAMES[@]}"}; do
      hcom kill "$name" --go ${name_arg[@]+"${name_arg[@]}"} || true
    done
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  launch_out=$(hcom 1 claude --tag worker --go --headless -p \
    --hcom-prompt "Wait for a task via hcom; end your turn to receive messages." \
    ${name_arg[@]+"${name_arg[@]}"} 2>&1) || { echo "$launch_out" >&2; exit 1; }
  track_launch "$launch_out"

An `EXIT` trap also runs after a successful script. If you intentionally want
spawned agents to remain running, clear the traps with `trap - EXIT INT TERM`
only after successful setup. Otherwise, wait for the workflow to finish before
exiting, so cleanup does not kill workers before they can do their task.

## Waiting for Workflow Events

For event-driven workflows, `hcom events --wait` can block until the next
milestone instead of polling:

  worker="${LAUNCHED_NAMES[0]}"
  thread="myscript-$(date +%s)-$$"
  hcom send "@${worker}" --thread "$thread" --intent request \
    ${name_arg[@]+"${name_arg[@]}"} -- "Do the task, then send me DONE"
  hcom events --wait 120 \
    --sql "type='message' AND msg_from='${worker}' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'" \
    ${name_arg[@]+"${name_arg[@]}"}

Filter by the worker sender as well as the thread and completion marker; the
request itself contains `DONE` and must not count as a worker response.

## Workflow Building Blocks

- Give concurrent workflow runs distinct `--thread` values when their messages
  should stay separate.
- `--intent request|inform|ack` tells recipients whether a reply is expected.
- Scripted launches commonly use `--go` so they do not pause at the preview.
- hcom injects `--name` when it knows the caller; parse and forward it when your
  script issues hcom commands on that caller's behalf.
- Launched agent names are generated dynamically. Capture them from the
  `Names:` launch output when you need to address or clean them up.
- `hcom events --wait` and `hcom events sub` are useful alternatives to sleeps
  when the next step depends on an hcom event.

## Generic Workflow Pattern Ideas

- Worker + reviewer loop: a worker sends `ROUND N DONE`; a reviewer replies with
  `FIX: ...` or `APPROVED`. The worker can revise until the reviewer accepts it.
- Cascade pipeline: one stage finishes, the next reads its result or transcript
  (`hcom transcript @name --full`) and continues from there.
- Ensemble + judge: launch several agents on the same question in parallel, then
  have a judge read their thread messages and synthesize a verdict.
- Cross-tool pair: mix tools for different roles, for example one agent implements
  while another tool reviews its transcript or checks the result.
- Reactive workflow: subscribe to an event such as an agent becoming idle or a
  file changing, then trigger the next action with `hcom events sub`.

## Reference Examples

View source of any bundled or user script:

  hcom run <name> --source

See `hcom run docs --cli` for full CLI command reference.
"#;

fn print_docs(show_cli: bool, show_config: bool, show_api: bool) -> i32 {
    let show_all = !show_cli && !show_config && !show_api;

    if show_all {
        println!("# hcom Documentation\n");
        println!("Sections:");
        println!("  1. CLI Reference");
        println!("  2. Config Settings");
        println!("  3. Script Creation Guide\n");
        println!("---\n");
    }

    if show_all || show_cli {
        println!("{}\n", help::get_help_text());
        for name in help::COMMAND_NAMES {
            println!("\n## {name}\n");
            println!("{}", help::get_command_help(name));
        }
        if show_all {
            println!("\n---\n");
        }
    }

    if show_all || show_config {
        println!("# Config Settings\n");
        println!(
            "File: {}",
            crate::paths::hcom_dir().join("config.toml").display()
        );
        println!("Precedence: defaults < config.toml < env vars\n");
        println!("Commands:");
        println!("  hcom config                 Show all values");
        println!("  hcom config <key> <val>     Set value");
        println!("  hcom config <key> --info    Detailed help for a setting");
        println!("  hcom config --edit          Open in $EDITOR\n");
        for (key, desc, typ) in CONFIG_KEYS {
            if *key == "HCOM_TERMINAL" {
                print_terminal_help();
                println!();
            } else if let Some(help_text) = config_help(key) {
                println!("{help_text}\n");
            } else {
                println!("{key} - {desc} ({typ})\n");
            }
        }
        println!("Per-instance config: hcom config -i <name> <key> [value]");
        println!("  Keys: tag, timeout, hints, subagent_timeout");
        if show_all {
            println!("\n---\n");
        }
    }

    if show_all || show_api {
        print!("{SCRIPT_GUIDE}");

        // List bundled scripts only (exclude user scripts from docs)
        let scripts = discover_scripts();
        let bundled_scripts: Vec<&ScriptInfo> = scripts
            .iter()
            .filter(|s| matches!(s.source, ScriptSource::Bundled { .. }))
            .collect();
        if !bundled_scripts.is_empty() {
            println!("Available scripts:");
            for s in &bundled_scripts {
                println!("  hcom run {} --source", s.name);
            }
            println!();
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[cfg(windows)]
    #[test]
    fn git_bash_search_skips_wsl_and_checks_later_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let wsl = dir.path().join("system32");
        let git = dir.path().join("Git with spaces").join("bin");
        for candidate in [&wsl, &git] {
            std::fs::create_dir_all(candidate).unwrap();
            std::fs::write(candidate.join("bash.exe"), "").unwrap();
        }
        let path = std::env::join_paths([&wsl, &git]).unwrap();
        assert_eq!(
            find_git_bash(&path, |bash| bash == git.join("bash.exe")),
            Some(git.join("bash.exe"))
        );
        assert!(find_git_bash(&path, |_| false).is_none());
    }

    #[test]
    fn run_args_capture_script_and_flags() {
        let args = RunArgs::try_parse_from(["run", "debate", "--topic", "hooks"]).unwrap();
        assert_eq!(args.args, vec!["debate", "--topic", "hooks"]);
    }

    #[test]
    fn test_extract_description_python() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.py");
        std::fs::write(&path, "\"\"\"My cool script.\"\"\"\nimport sys\n").unwrap();
        assert_eq!(extract_description(&path), "My cool script.");
    }

    #[test]
    fn test_extract_description_shell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sh");
        std::fs::write(&path, "#!/bin/bash\n# A shell script\necho hi\n").unwrap();
        assert_eq!(extract_description(&path), "A shell script");
    }

    #[test]
    fn test_extract_description_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.py");
        std::fs::write(&path, "import sys\n").unwrap();
        assert_eq!(extract_description(&path), "");
    }

    #[test]
    fn test_bundled_agent_desc() {
        assert_eq!(bundled_agent_desc("confess"), "3 agents");
        assert_eq!(bundled_agent_desc("unknown"), "");
    }

    #[test]
    fn test_embedded_scripts_available() {
        assert_eq!(scripts::SCRIPTS.len(), 4);
        let names: Vec<&str> = scripts::SCRIPTS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"confess"));
        assert!(names.contains(&"debate"));
        assert!(names.contains(&"fatcow"));
        assert!(names.contains(&"onidle"));
    }

    #[test]
    fn test_extract_description_from_embedded() {
        // Verify we can extract descriptions from embedded content
        for (name, content) in scripts::SCRIPTS {
            let desc = extract_description_from_content(content);
            assert!(
                !desc.is_empty(),
                "Bundled script '{name}' should have a description comment"
            );
        }
    }
}
