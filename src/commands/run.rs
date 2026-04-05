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

#[derive(clap::Parser, Debug)]
#[command(name = "run", about = "Run a bundled or user workflow script")]
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
        println!("Examples:");
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
    if let Some(ctx) = ctx {
        if let Some(ref name) = ctx.explicit_name {
            let canonical =
                crate::instances::resolve_display_name(db, name).unwrap_or_else(|| name.clone());
            argv = vec!["--name".to_string(), canonical]
                .into_iter()
                .chain(argv)
                .collect();
        }
    }

    if argv.is_empty() {
        return list_scripts();
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
        let show_cli = args.iter().any(|a| a == "--cli");
        let show_config = args.iter().any(|a| a == "--config");
        let show_api = args.iter().any(|a| a == "--scripts");
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
                let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
                let mut c = Command::new(&python);
                c.arg(path);
                c.args(&args);
                c
            } else {
                let mut c = Command::new("bash");
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

            let mut cmd = Command::new("bash");
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
  File types:      *.sh (bash), *.py (python3)

User scripts shadow bundled scripts with the same name.
Scripts are discovered automatically — drop a file and run `hcom run <name>`.
Add a description comment on line 2 (after shebang) — it shows in `hcom run` listings.

## Shell Script Template

  #!/usr/bin/env bash
  # Brief description shown in hcom run list.
  set -euo pipefail

  name_flag=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -h|--help) echo "Usage: hcom run myscript [OPTIONS]"; exit 0 ;;
      --name) name_flag="$2"; shift 2 ;;
      --target) target="$2"; shift 2 ;;
      *) shift ;;
    esac
  done

  name_arg=""
  [[ -n "$name_flag" ]] && name_arg="--name $name_flag"

  # Your logic here
  hcom send "@${target}" $name_arg --intent request -- "Do the task"

## Identity Handling

hcom passes --name to scripts automatically. Always parse and forward it:

  name_arg=""
  [[ -n "$name_flag" ]] && name_arg="--name $name_flag"
  hcom send @target $name_arg -- "message"
  hcom list self --json $name_arg

## Launching & Cleaning Up Agents

Launch output includes "Names: <name>" — parse to track spawned agents:

  LAUNCHED_NAMES=()
  track_launch() {
    local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
    for n in $names; do LAUNCHED_NAMES+=("$n"); done
  }
  cleanup() {
    for name in "${LAUNCHED_NAMES[@]}"; do
      hcom kill "$name" --go 2>/dev/null || true
    done
  }
  trap cleanup ERR

  launch_out=$(hcom 1 claude --tag worker --go --headless 2>&1)
  track_launch "$launch_out"

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
        assert_eq!(scripts::SCRIPTS.len(), 3);
        let names: Vec<&str> = scripts::SCRIPTS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"confess"));
        assert!(names.contains(&"debate"));
        assert!(names.contains(&"fatcow"));
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
