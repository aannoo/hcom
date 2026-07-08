//! `hcom completion` command — generate shell completions.
//!
//! Currently supports Zsh. Output a completion script to stdout that the user
//! can source or install into their `$fpath`.

use crate::db::HcomDb;
use crate::shared::CommandContext;

#[derive(clap::Parser, Debug)]
#[command(name = "completion", about = "Generate shell completion scripts")]
pub struct CompletionArgs {
    /// Shell to generate completions for (zsh)
    pub shell: Option<String>,

    /// Install completions (zsh: write to $fpath/_hcom)
    #[arg(long)]
    pub install: bool,
}

/// Commands available at the top level (keep in sync with router.rs).
const CLI_COMMANDS: &[&str] = &[
    "send", "list", "events", "stop", "start", "listen", "status", "config", "hooks",
    "archive", "reset", "transcript", "bundle", "kill", "term", "relay", "run", "update",
];

/// All tool names (released) + their public aliases.
fn all_tool_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for spec in crate::integration_spec::ALL {
        if !spec.released {
            continue;
        }
        names.push(spec.name);
        for alias in spec.aliases {
            names.push(alias);
        }
    }
    names
}

/// Hook names across all released tools.
fn all_hook_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for spec in crate::integration_spec::ALL {
        if !spec.released {
            continue;
        }
        for hook in spec.hooks.names {
            if !names.contains(hook) {
                names.push(hook);
            }
        }
    }
    names
}

/// Generate the tool case arms for the Zsh completion function.
fn generate_tool_arms() -> String {
    let mut arms = String::new();

    for spec in crate::integration_spec::ALL {
        if !spec.released {
            continue;
        }
        let names: Vec<&str> = std::iter::once(spec.name)
            .chain(spec.aliases.iter().copied())
            .collect();
        let case_pattern = names.join("|");

        // Check if this tool has a per-tool args env var.
        let has_args_env = spec.launch.args_env.is_some();
        // Only emit the --*-args flag for the canonical name.
        let canonical = spec.name.replace('_', "-");

        let extra = if has_args_env {
            format!(
                "                    \"--{}-args=[Default args]:args: \" \\\n",
                canonical
            )
        } else {
            String::new()
        };

        let arm = format!(
            r#"            ({case_pattern})
                _arguments -s -S \
                    "--tag=[Group tag]:tag:" \
                    "--terminal=[Terminal preset]:preset:(default kitty wezterm tmux zellij iterm)" \
                    "--dir=[Working directory]:directory:_files -/" \
                    "--headless[Run in background]" \
                    "--hcom-prompt=[Initial prompt]:prompt: " \
                    "--hcom-system-prompt=[System prompt]:prompt: " \
                    "--name=[Instance name]:name: " \
                    "--go[Skip confirmation]" \
                    {extra}\
                    && return 0
                ;;
"#,
            case_pattern = case_pattern,
            extra = extra,
        );
        arms.push_str(&arm);
    }
    arms
}

/// Generate the Zsh completion function.
fn generate_zsh_completion() -> String {
    let tools = all_tool_names();
    let commands = CLI_COMMANDS;
    let hooks = all_hook_names();

    let tool_list = tools.join(" ");
    let command_list = commands.join(" ");
    let hook_list = hooks.join(" ");

    let tool_arms = generate_tool_arms();

    let cmd_entries: Vec<String> = commands
        .iter()
        .map(|c| {
            let desc = match *c {
                "send" => "Send message to agents",
                "list" => "List active agents",
                "events" => "Query event stream",
                "stop" => "Disconnect from hcom",
                "start" => "Connect to hcom",
                "listen" => "Block until message",
                "status" => "System health overview",
                "config" => "Get/set settings",
                "hooks" => "Manage hooks",
                "archive" => "Query past sessions",
                "reset" => "Archive and clear DB",
                "transcript" => "Read agent conversation",
                "bundle" => "Structured context packages",
                "kill" => "Kill agent process",
                "term" => "View/inject PTY screens",
                "relay" => "Cross-device sync",
                "run" => "Execute workflow scripts",
                "update" => "Check and apply updates",
                "completion" => "Generate shell completions",
                _ => c,
            };
            format!("{}:{:?}", c, desc)
        })
        .collect();
    let cmd_entry_str = cmd_entries.join(" ");

    let tool_entries: Vec<String> = tools
        .iter()
        .map(|t| {
            let desc = match *t {
                "claude" => "Launch Claude Code agent",
                "gemini" => "Launch Gemini CLI agent",
                "codex" => "Launch Codex CLI agent",
                "opencode" => "Launch OpenCode agent",
                "kilo" | "kilocode" => "Launch Kilo Code agent",
                "pi" | "pi-agent" => "Launch Pi agent",
                "omp" | "omp-agent" => "Launch Oh My Pi agent",
                "antigravity" | "agy" => "Launch Antigravity agent",
                "cursor" | "cursor-agent" => "Launch Cursor agent",
                "kimi" => "Launch Kimi agent",
                "copilot" => "Launch Copilot agent",
                _ => "Launch agent",
            };
            format!("{}:{:?}", t, desc)
        })
        .collect();
    let tool_entry_str = tool_entries.join(" ");

    let hook_entries: Vec<String> = hooks
        .iter()
        .map(|h| format!("{}:\"Hook command\"", h))
        .collect();
    let hook_entry_str = hook_entries.join(" ");

    let transcript_agents = crate::transcript::transcript_tool_names().join(" ");

    format!(
        r#"#compdef hcom

# hcom Zsh completion
# Source this file: source <(hcom completion zsh)
# Or install: hcom completion zsh --install

typeset -A opt_args

_hcom() {{
    local curcontext="$curcontext" state line ret=1
    local -a tools hook_cmds commands

    tools=({tool_list})
    commands=({command_list})
    hook_cmds=({hook_list})

    # Top-level: try matching a command, tool, hook, or numeric prefix.
    # Global flags before the subcommand.
    local -a global_opts
    global_opts=(
        '--name[Instance name]:name:'
        '--go[Skip confirmation prompts]'
        '--help[Show help]'
        '-h[Show help]'
        '--version[Show version]'
        '-v[Show version]'
        '--new-terminal[Open TUI in new terminal window]'
    )

    # If we have arguments already
    if (( CURRENT > 1 )); then
        local cmd="$words[1]"
        local prev="$words[CURRENT-1]"

        # Numeric prefix + tool: "3 claude ..."
        if [[ "$cmd" = <-> ]] && (( CURRENT > 2 )); then
            cmd="$words[2]"
        fi

        case "$cmd" in
            # -- Commands ---------------------------------------------
            (send)
                _arguments -s -S \
                    '--from=[Sender identity]:name:' \
                    '--intent=[Message intent]:intent:(request inform ack)' \
                    '--reply-to=[Reply to event ID]:id:' \
                    '--thread=[Thread name]:name:' \
                    '--title=[Bundle title]:text:' \
                    '--description=[Bundle description]:text:' \
                    '--events=[Event IDs/ranges]:ids:' \
                    '--files=[File paths]:files:_files' \
                    '--transcript=[Transcript ranges]:ranges:' \
                    '--extends=[Parent bundle ID]:id:' \
                    '--name=[Your identity]:name:' \
                    '--file=[Message from file]:file:_files' \
                    '--base64=[Base64-encoded message]:data:' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (list)
                _arguments -s -S \
                    '--json[JSON output]' \
                    '--names[Just names]' \
                    '--stopped[Show stopped agents]' \
                    '--all[All stopped agents]' \
                    '--format=[Template per agent]:template:' \
                    '--sh[Shell exports]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (events)
                _arguments -s -S \
                    '--last=[Limit count]:count:' \
                    '--all[Include archived]' \
                    '--wait[Block until match]:seconds:' \
                    '--sql=[SQL WHERE expression]:sql:' \
                    '--agent=[Agent name]:name:' \
                    '--type=[Event type]:type:(message status life)' \
                    '--status=[Status value]:status:(listening active blocked)' \
                    '--context=[Context pattern]:pattern:' \
                    '--action=[Lifecycle action]:action:(created started ready stopped batch_launched launch_failed launch_blocked)' \
                    '--cmd=[Command pattern]:pattern:' \
                    '--file=[File path]:file:' \
                    '--collision[Collision detection]' \
                    '--from=[Sender name]:name:' \
                    '--mention=[@mention target]:name:' \
                    '--intent=[Message intent]:intent:(request inform ack)' \
                    '--thread=[Thread name]:name:' \
                    '--after=[After timestamp]:time:' \
                    '--before=[Before timestamp]:time:' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (stop)
                _arguments -s -S \
                    '--help[Show help]' \
                    && return 0
                ;;
            (start)
                _arguments -s -S \
                    '--as=[Reclaim identity]:name:' \
                    '--orphan=[Recover orphaned PTY]:name:' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (listen)
                _arguments -s -S \
                    '--timeout=[Timeout in seconds]:seconds:' \
                    '--json[JSON output]' \
                    '--sql=[SQL filter]:sql:' \
                    '--idle=[Wait for idle]:name:' \
                    '--agent=[Agent name]:name:' \
                    '--type=[Event type]:type:(message status life)' \
                    '--file=[File pattern]:file:' \
                    '--cmd=[Command pattern]:pattern:' \
                    '--from=[Sender]:name:' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (status)
                _arguments -s -S \
                    '--logs[Show recent logs]' \
                    '--json[JSON output]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (config)
                _arguments -s -S \
                    '--json[JSON output]' \
                    '--edit[Edit config]' \
                    '--reset[Reset key]' \
                    '--info[Detailed help for key]' \
                    '-i[Per-agent config]:name:' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (hooks)
                _arguments -s -S \
                    ':action:(status add remove)' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (archive)
                _arguments -s -S \
                    '--here[Current directory only]' \
                    '--sql=[SQL filter]:sql:' \
                    '--last=[Limit]:count:' \
                    '--json[JSON output]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (reset)
                _arguments -s -S \
                    '--help[Show help]' \
                    && return 0
                ;;
            (transcript)
                _arguments -s -S \
                    '--last=[Limit exchanges]:count:' \
                    '--full[Full responses]' \
                    '--detailed[Show tool I/O]' \
                    '--json[JSON output]' \
                    '--live[Only alive agents]' \
                    '--all[All transcripts]' \
                    '--limit=[Max results]:count:' \
                    '--agent=[Agent type]:agent_type:({transcript_agents})' \
                    '--exclude-self[Exclude self]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (bundle)
                _arguments -s -S \
                    '--last=[Limit]:count:' \
                    '--json[JSON output]' \
                    '--title=[Bundle title]:title:' \
                    '--description=[Bundle description]:text:' \
                    '--events=[Event IDs]:ids:' \
                    '--files=[File paths]:files:_files' \
                    '--transcript=[Transcript ranges]:ranges:' \
                    '--extends=[Parent bundle]:id:' \
                    '--bundle=[JSON payload]:json:' \
                    '--bundle-file=[JSON file]:file:_files' \
                    '--for=[Target agent]:name:' \
                    '--last-transcript=[Count]:count:' \
                    '--last-events=[Count]:count:' \
                    '--compact[Hide how-to]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (kill)
                _arguments -s -S \
                    '--help[Show help]' \
                    && return 0
                ;;
            (term)
                _arguments -s -S \
                    '--json[JSON output]' \
                    '--enter[Send Enter]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (relay)
                _arguments -s -S \
                    '--help[Show help]' \
                    && return 0
                ;;
            (run)
                _arguments -s -S \
                    '--help[Show help]' \
                    && return 0
                ;;
            (update)
                _arguments -s -S \
                    '--check[Check only]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (completion)
                _arguments -s -S \
                    '--install[Install completions]' \
                    '--help[Show help]' \
                    ':shell:(zsh)' \
                    && return 0
                ;;
            (r|resume)
                _arguments -s -S \
                    '--tag=[Group tag]:tag:' \
                    '--terminal=[Terminal preset]:preset:(default kitty wezterm tmux zellij iterm)' \
                    '--dir=[Working directory]:directory:_files -/' \
                    '--headless[Run in background]' \
                    '--hcom-prompt=[Initial prompt]:prompt: ' \
                    '--hcom-system-prompt=[System prompt]:prompt: ' \
                    '--go[Skip preview]' \
                    '--help[Show help]' \
                    && return 0
                ;;
            (f|fork)
                _arguments -s -S \
                    '--tag=[Group tag]:tag:' \
                    '--terminal=[Terminal preset]:preset:(default kitty wezterm tmux zellij iterm)' \
                    '--dir=[Working directory]:directory:_files -/' \
                    '--headless[Run in background]' \
                    '--hcom-prompt=[Initial prompt]:prompt: ' \
                    '--hcom-system-prompt=[System prompt]:prompt: ' \
                    '--go[Skip preview]' \
                    '--help[Show help]' \
                    && return 0
                ;;
{tool_arms}
        esac
    fi

    # First argument: suggest commands, tools, and special tokens.
    _alternative \
        'commands:command:(({cmd_entry_str}))' \
        'tools:tool:(({tool_entry_str}))' \
        'hooks:hook:(({hook_entry_str}))' \
        'globals:global flag:((--name:"Instance name" --go:"Skip confirmation" --help:"Show help" -h:"Show help" --version:"Show version" -v:"Show version" --new-terminal:"Open TUI in new window"))' \
        'numbers:count:((1:"Launch 1 agent" 2:"Launch 2 agents" 3:"Launch 3 agents" 4:"Launch 4 agents" 5:"Launch 5 agents" 10:"Launch 10 agents"))'
}}

# Register the completion
_hcom "$@"
"#,
        tool_list = tool_list,
        command_list = command_list,
        hook_list = hook_list,
        tool_arms = tool_arms,
        cmd_entry_str = cmd_entry_str,
        tool_entry_str = tool_entry_str,
        hook_entry_str = hook_entry_str,
        transcript_agents = transcript_agents,
    )
}

pub fn cmd_completion(_db: &HcomDb, args: &CompletionArgs, _ctx: Option<&CommandContext>) -> i32 {
    let shell = args.shell.as_deref().unwrap_or("zsh");

    match shell {
        "zsh" => {
            let script = generate_zsh_completion();
            if args.install {
                let zsh_dir = zsh_completion_dir();
                if let Some(dir) = &zsh_dir {
                    let path = dir.join("_hcom");
                    match std::fs::create_dir_all(dir) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Error: Could not create {}: {e}", dir.display());
                            return 1;
                        }
                    }
                    match std::fs::write(&path, &script) {
                        Ok(_) => {
                            println!("Installed Zsh completion to {}", path.display());
                            println!("Make sure {} is in your $fpath.", dir.display());
                            println!("Then run: compinit");
                            0
                        }
                        Err(e) => {
                            eprintln!("Error: Could not write {}: {e}", path.display());
                            1
                        }
                    }
                } else {
                    eprintln!("Error: Could not determine Zsh completion directory.");
                    eprintln!("Install manually: hcom completion zsh > /usr/local/share/zsh/site-functions/_hcom");
                    1
                }
            } else {
                print!("{script}");
                0
            }
        }
        other => {
            eprintln!("Error: Unsupported shell '{other}'. Supported: zsh");
            1
        }
    }
}

/// Find a writable directory for Zsh completions.
fn zsh_completion_dir() -> Option<std::path::PathBuf> {
    // Check $fpath first
    if let Ok(fpath_str) = std::env::var("fpath") {
        for dir in fpath_str.split_whitespace() {
            let path = std::path::PathBuf::from(dir);
            if path.is_dir() {
                let test_file = path.join(".hcom_completion_test");
                if std::fs::write(&test_file, "").is_ok() {
                    let _ = std::fs::remove_file(&test_file);
                    return Some(path);
                }
            }
        }
    }

    // Fallback: standard locations
    let candidates = [
        dirs::home_dir()
            .as_ref()
            .map(|h| h.join(".zsh").join("completion")),
        Some(std::path::PathBuf::from(
            "/usr/local/share/zsh/site-functions",
        )),
        dirs::home_dir()
            .as_ref()
            .map(|h| h.join(".oh-my-zsh").join("completions")),
    ];

    for candidate in candidates.iter().flatten() {
        if candidate.is_dir() {
            return Some(candidate.clone());
        }
        if let Some(parent) = candidate.parent() {
            if parent.is_dir() {
                return Some(candidate.clone());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_generation_succeeds() {
        let script = generate_zsh_completion();
        assert!(script.starts_with("#compdef hcom"));
        assert!(script.contains("claude"));
        assert!(script.contains("send"));
        assert!(script.contains("list"));
        assert!(script.contains("gemini"));
    }

    #[test]
    fn zsh_script_contains_all_commands() {
        let script = generate_zsh_completion();
        for cmd in CLI_COMMANDS {
            assert!(
                script.contains(cmd),
                "Zsh completion should contain command '{cmd}'"
            );
        }
    }

    #[test]
    fn zsh_script_contains_all_tools() {
        let script = generate_zsh_completion();
        for tool in all_tool_names() {
            assert!(
                script.contains(tool),
                "Zsh completion should contain tool '{tool}'"
            );
        }
    }

    #[test]
    fn zsh_completion_dir_is_safe() {
        // Should not panic
        let _ = zsh_completion_dir();
    }

    #[test]
    fn zsh_script_valid_syntax() {
        let script = generate_zsh_completion();
        // Check that zsh reserved words in the right context exist
        assert!(script.contains("_arguments"));
        assert!(script.contains("_alternative"));
        assert!(script.contains("_files"));
        // No stray format placeholders
        assert!(!script.contains("{{"));
        assert!(!script.contains("}}"));
    }
}
