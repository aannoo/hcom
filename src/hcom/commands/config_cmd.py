"""Config commands for HCOM"""

import sys
import os
import json
import shlex
import shutil
from .utils import format_error
from ..shared import CommandContext


# ==================== Terminal Config Helpers ====================

# Managed parent presets (recommended in bare get — auto split/tab/window + close on kill)
_MANAGED_PARENTS = ("kitty", "wezterm", "tmux")

_MANAGED_VARIANTS = {
    "kitty": ("kitty-window", "kitty-tab", "kitty-split"),
    "wezterm": ("wezterm-window", "wezterm-tab", "wezterm-split"),
    "tmux": ("tmux-split",),
}

_MANAGED_DESCS = {
    "kitty": "auto split/tab/window",
    "wezterm": "auto tab/split/window",
    "tmux": "detached sessions",
}


def _resolve_default_terminal_name() -> str:
    """Human name for what 'default' resolves to on this platform."""
    import platform as _platform

    system = _platform.system()
    if system == "Darwin":
        return "Terminal.app"
    elif system == "Windows":
        return "Windows Terminal" if shutil.which("wt") else "cmd"
    else:
        for name in ("gnome-terminal", "konsole", "xterm"):
            if shutil.which(name):
                return name
        return "system terminal"


def _is_managed_preset(preset_name: str) -> bool:
    """Check if a preset has close-on-kill support."""
    from ..core.settings import get_merged_preset

    preset = get_merged_preset(preset_name) or {}
    return bool(preset.get("close"))


def _get_available_managed() -> list[tuple[str, str]]:
    """Available managed parent presets on this system. Returns [(name, desc)]."""
    from ..terminal import get_available_presets

    available_names = {name for name, avail in get_available_presets() if avail}
    return [(name, _MANAGED_DESCS[name]) for name in _MANAGED_PARENTS if name in available_names]


def _find_kitty_binary() -> str | None:
    """Find kitty binary — PATH first, then macOS app bundle."""
    from ..terminal import _find_macos_app
    import platform
    path_kitty = shutil.which("kitty")
    if path_kitty:
        return path_kitty
    if platform.system() == "Darwin":
        app = _find_macos_app("kitty")
        if app:
            full = app / "Contents" / "MacOS" / "kitty"
            if full.exists():
                return str(full)
    return None


def _find_kitty_conf() -> str | None:
    """Get kitty's config path using kitty itself."""
    import subprocess
    kitty = _find_kitty_binary()
    if not kitty:
        return None
    try:
        result = subprocess.run(
            [kitty, "+runpy", "from kitty.constants import config_dir; print(config_dir)"],
            capture_output=True, text=True, timeout=3,
        )
        if result.returncode != 0:
            return None
    except Exception:
        return None
    conf_dir = result.stdout.strip()
    if not conf_dir:
        return None
    conf = os.path.join(conf_dir, "kitty.conf")
    return conf if os.path.isfile(conf) else None


def _kitty_conf_has(conf_path: str, key: str) -> str | None:
    """Return the value of an uncommented key in kitty.conf, or None."""
    with open(conf_path) as f:
        for line in f:
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            parts = stripped.split(None, 1)
            if len(parts) == 2 and parts[0] == key:
                return parts[1]
    return None


def _show_kitty_status(value: str) -> None:
    """Check kitty remote control socket and show status / hint at --setup.

    Runs in daemon context — no access to KITTY_WINDOW_ID/KITTY_LISTEN_ON env vars.
    Only filesystem-based detection (_find_kitty_socket) works here.
    """
    from ..terminal import _find_kitty_socket

    socket = _find_kitty_socket()

    if socket:
        if value == "kitty":
            print(f"  Socket found ({socket}) → splits/tabs available")
        return

    # No socket — diagnose why
    conf = _find_kitty_conf()
    if not conf:
        print("  No kitty.conf found")
        print("  Run: hcom config terminal --setup")
        return

    has_rc = _kitty_conf_has(conf, "allow_remote_control")
    has_listen = _kitty_conf_has(conf, "listen_on")

    if has_rc in ("yes", "socket") and has_listen:
        print("  Config OK but no socket — restart kitty")
    elif has_rc and has_rc not in ("yes", "socket"):
        print(f"  allow_remote_control is '{has_rc}' — needs 'yes' or 'socket'")
        print(f"  Edit {conf} manually, then restart kitty")
    else:
        print("  Remote control not configured")
        print("  Run: hcom config terminal --setup")


def _kitty_setup() -> int:
    """Configure kitty for remote control (splits/tabs). Returns exit code."""
    from ..terminal import _find_kitty_socket

    socket = _find_kitty_socket()
    if socket:
        print(f"Kitty remote control already working ({socket})")
        return 0

    conf = _find_kitty_conf()
    if not conf:
        print(format_error("Could not find kitty.conf"), file=sys.stderr)
        return 1

    has_rc = _kitty_conf_has(conf, "allow_remote_control")
    has_listen = _kitty_conf_has(conf, "listen_on")

    if has_rc in ("yes", "socket") and has_listen:
        print(f"Config OK ({conf}) but no socket — restart kitty")
        return 0

    if has_rc and has_rc not in ("yes", "socket"):
        print(format_error(f"allow_remote_control is '{has_rc}' in {conf}"), file=sys.stderr)
        print("  Change to 'yes' or 'socket', then restart kitty", file=sys.stderr)
        return 1

    lines: list[str] = []
    if not has_rc:
        lines.append("allow_remote_control yes")
    if not has_listen:
        lines.append("listen_on unix:/tmp/kitty")

    try:
        with open(conf, "a") as f:
            f.write("\n# Added by hcom for remote control (splits/tabs)\n")
            for line in lines:
                f.write(f"{line}\n")
    except OSError as e:
        print(format_error(f"Failed to write {conf}: {e}"), file=sys.stderr)
        return 1

    for line in lines:
        print(f"Added to {conf}: {line}")
    print("Restart kitty for changes to take effect")
    return 0


def _show_terminal_status(current_value: str) -> int:
    """Rich terminal config status for bare 'hcom config terminal'."""
    if not current_value:
        current_value = "default"

    managed = _get_available_managed()

    if current_value == "default":
        resolved = _resolve_default_terminal_name()
        print(f"Terminal: default ({resolved})")
        if managed:
            print()
            print("'hcom kill' stops processes but can't close terminal windows.")
            print("For full lifecycle control (open + close on kill), set a managed terminal:")
            print()
            for name, desc in managed:
                print(f"  hcom config terminal {name:<14} {desc}")
    elif _is_managed_preset(current_value):
        print(f"Terminal: {current_value} (managed — opens and closes panes on kill)")
        print()
        print("  hcom config terminal default     reset to platform default")
    else:
        print(f"Terminal: {current_value} (open only — kill can't close windows)")
        if managed:
            print()
            print("For full lifecycle control, set a managed terminal:")
            print()
            for name, desc in managed:
                print(f"  hcom config terminal {name:<14} {desc}")

    print()
    print("  hcom config terminal --info      all options")
    return 0


# ==================== Detailed Config Help ====================


_CONFIG_HELP: dict[str, str] = {
    "HCOM_TAG": """HCOM_TAG - Group tag for launched instances

Current value: Use 'hcom config tag' to see current value

Purpose:
  Creates named groups of agents that can be addressed together.
  When set, launched instances get names like: <tag>-<name>

Usage:
  hcom config tag myteam        # Set tag
  hcom config tag ""            # Clear tag

  # Or via environment:
  HCOM_TAG=myteam hcom 3 claude

Effect:
  Without tag: launches create → luna, nova, kira
  With tag "dev": launches create → dev-luna, dev-nova, dev-kira

Addressing:
  @dev         → sends to all agents with tag "dev"
  @dev-luna    → sends to specific agent

Allowed characters: letters, numbers, hyphens (a-z, A-Z, 0-9, -)
""",
    "HCOM_HINTS": """HCOM_HINTS - Text injected with all messages

Current value: Use 'hcom config hints' to see current value

Purpose:
  Appends text to every message received by launched agents.
  Useful for persistent instructions or context.

Usage:
  hcom config hints "Always respond in JSON format"
  hcom config hints ""   # Clear hints

Example:
  hcom config hints "You are part of team-alpha. Coordinate with @team-alpha members."

Notes:
  - Hints are appended to message content, not system prompt
  - Each agent can have different hints (set via hcom config -i <name> hints)
  - Global hints apply to all new launches
""",
    "HCOM_NOTES": """HCOM_NOTES - One-time notes appended to bootstrap

  Custom text added to agent system context at startup.
  Unlike HCOM_HINTS (per-message), this is injected once and does not repeat.

Usage:
  hcom config notes "Always check hcom list before spawning new agents"
  hcom config notes ""                            # Clear
  HCOM_NOTES="tips" hcom 1 claude                 # Per-launch override

  Changing after launch has no effect (bootstrap already delivered).
""",
    "HCOM_TIMEOUT": """HCOM_TIMEOUT - Advanced: idle timeout for headless/vanilla Claude (seconds)

Default: 86400 (24 hours)

This setting only applies to:
  - Headless Claude: hcom N claude -p
  - Vanilla Claude: claude + hcom start

Does NOT apply to:
  - Interactive PTY mode: hcom N claude (main path)
  - Gemini or Codex

How it works:
  - Claude's Stop hook runs when Claude goes idle
  - Hook waits up to TIMEOUT seconds for a message
  - If no message within timeout, instance is unregistered

Usage (if needed):
  hcom config HCOM_TIMEOUT 3600   # 1 hour
  export HCOM_TIMEOUT=3600        # via environment
""",
    "HCOM_SUBAGENT_TIMEOUT": """HCOM_SUBAGENT_TIMEOUT - Timeout for Claude subagents (seconds)

Current value: Use 'hcom config subagent_timeout' to see current value
Default: 30

Purpose:
  How long Claude waits for a subagent (Task tool) to complete.
  Shorter than main timeout since subagents should be quick.

Usage:
  hcom config subagent_timeout 60    # 1 minute
  hcom config subagent_timeout 30    # 30 seconds (default)

Notes:
  - Only applies to Claude Code's Task tool spawned agents
  - Parent agent blocks until subagent completes or times out
  - Increase for complex subagent tasks
""",
    "HCOM_CLAUDE_ARGS": """HCOM_CLAUDE_ARGS - Default args passed to claude on launch

Example: hcom config claude_args "--model opus"
Clear:   hcom config claude_args ""

Merged with launch-time cli args (launch args win on conflict).
""",
    "HCOM_GEMINI_ARGS": """HCOM_GEMINI_ARGS - Default args passed to gemini on launch

Example: hcom config gemini_args "--model gemini-2.5-flash"
Clear:   hcom config gemini_args ""

Merged with launch-time cli args (launch args win on conflict).
""",
    "HCOM_CODEX_ARGS": """HCOM_CODEX_ARGS - Default args passed to codex on launch

Example: hcom config codex_args "--search"
Clear:   hcom config codex_args ""

Merged with launch-time cli args (launch args win on conflict).
""",
    "HCOM_RELAY": """HCOM_RELAY - Relay server URL

Set automatically by 'hcom relay hf'.

Custom server: implement POST /push/{device_id}, GET /poll, GET /version
See: https://huggingface.co/spaces/aannoo/hcom-relay/blob/main/app.py
""",
    "HCOM_RELAY_TOKEN": """HCOM_RELAY_TOKEN - HuggingFace token for private Space auth
Set automatically by 'hcom relay hf'

Or optional authentication token for custom server.
""",
    "HCOM_AUTO_APPROVE": """HCOM_AUTO_APPROVE - Auto-approve safe hcom commands

Current value: Use 'hcom config auto_approve' to see current value

Purpose:
  When enabled, Claude/Gemini/Codex auto-approve "safe" hcom commands
  without requiring user confirmation.

Usage:
  hcom config auto_approve 1    # Enable auto-approve
  hcom config auto_approve 0    # Disable (require approval)

Safe commands (auto-approved when enabled):
  send, start, list, events, listen, relay, config,
  transcript, archive, status, help, --help, --version

Always require approval:
  - hcom reset          (archives and clears database)
  - hcom stop           (stops instances)
  - hcom <N> claude     (launches new instances)

Values: 1, true, yes, on (enabled) | 0, false, no, off, "" (disabled)
""",
    "HCOM_AUTO_SUBSCRIBE": """HCOM_AUTO_SUBSCRIBE - Auto-subscribe event presets for new instances

Current value: Use 'hcom config auto_subscribe' to see current value
Default: collision

Purpose:
  Comma-separated list of event subscriptions automatically added
  when an instance registers with 'hcom start'.

Usage:
  hcom config auto_subscribe "collision,created"
  hcom config auto_subscribe ""   # No auto-subscribe

Available presets:
  collision    - Alert when agents edit same file (within 20s window)
  created      - Notify when new instances join
  stopped      - Notify when instances leave
  blocked      - Notify when any instance is blocked (needs approval)

Notes:
  - Instances can add/remove subscriptions at runtime
  - See 'hcom events --help' for subscription management
""",
    "HCOM_NAME_EXPORT": """HCOM_NAME_EXPORT - Export instance name to custom env var

Current value: Use 'hcom config name_export' to see current value

Purpose:
  When set, launched instances will have their name exported to
  the specified environment variable. Useful for scripts that need
  to reference the current instance name.

Usage:
  hcom config name_export "MY_AGENT_NAME"   # Export to MY_AGENT_NAME
  hcom config name_export ""                 # Disable export

Example:
  # Set export variable
  hcom config name_export "HCOM_NAME"

  # Now launched instances have:
  # HCOM_NAME=luna (or whatever name was generated)

  # Scripts can use it:
  # hcom send "@$HCOM_NAME completed task"

Notes:
  - Only affects hcom-launched instances (hcom N claude/gemini/codex)
  - Variable name must be a valid shell identifier
  - Works alongside HCOM_PROCESS_ID (always set) for identity
""",
}


def _build_terminal_help(current_value: str = "") -> str:
    """Dynamic help for HCOM_TERMINAL (depends on installed tools).

    Args:
        current_value: Current terminal setting. Caller should pass this
            from the freshly-loaded config snapshot, not get_config() cache.
    """
    from ..terminal import get_available_presets

    current = current_value or "default"

    # Current value display
    if current == "default":
        current_display = f"default ({_resolve_default_terminal_name()})"
    elif _is_managed_preset(current):
        current_display = f"{current} (managed)"
    else:
        current_display = f"{current} (open only)"

    # Build managed section
    presets_avail = {name: avail for name, avail in get_available_presets()}

    managed_lines = []
    for parent in _MANAGED_PARENTS:
        mark = "[+]" if presets_avail.get(parent, False) else "[-]"
        desc = _MANAGED_DESCS[parent]
        managed_lines.append(f"  {mark} {parent:<14} {desc}")

    variant_lines = []
    for parent, variants in _MANAGED_VARIANTS.items():
        variant_lines.append(f"    {parent}: {', '.join(variants)}")

    # Build other section (platform-filtered by get_available_presets)
    all_managed = set(_MANAGED_PARENTS)
    for variants in _MANAGED_VARIANTS.values():
        all_managed.update(variants)

    install_hints = {
        "ttab": "npm install -g ttab",
        "wttab": "npm install -g wttab",
    }

    other_lines = []
    for name, avail in get_available_presets():
        if name in ("default", "custom") or name in all_managed:
            continue
        mark = "[+]" if avail else "[-]"
        hint = install_hints.get(name, "")
        if not avail and hint:
            other_lines.append(f"  {mark} {name:<18} {hint}")
        else:
            other_lines.append(f"  {mark} {name}")

    return f"""HCOM_TERMINAL — where hcom opens new agent windows

Current: {current_display}

Managed (open + close on kill):
{chr(10).join(managed_lines)}

  Variants:
{chr(10).join(variant_lines)}

Other (opens window only):
{chr(10).join(other_lines)}

Custom command (open only):
  hcom config terminal "my-terminal -e bash {{script}}"

Custom preset with close (~/.hcom/settings.toml):
  [terminal.myterm]
  open = "myterm spawn -- bash {{script}}"
  close = "myterm kill --id {{id}}"
  binary = "myterm"

  {{id}} = stdout from the open command.
  {{pid}} and {{process_id}} also available.

Set:    hcom config terminal kitty
Reset:  hcom config terminal default
"""


def _get_config_help(key: str, current_value: str = "") -> str | None:
    """Get detailed help for a config key. Returns None if no detailed help available.

    Args:
        current_value: For HCOM_TERMINAL, the freshly-loaded current value.
    """
    key = key.upper()
    if not key.startswith("HCOM_"):
        key = f"HCOM_{key}"
    if key == "HCOM_TERMINAL":
        return _build_terminal_help(current_value)
    return _CONFIG_HELP.get(key)


def _config_instance(target: str, argv: list[str], json_output: bool) -> int:
    """Handle instance-level config via -i <name>

    Supported keys: tag, timeout, hints, subagent_timeout
    """
    from .utils import resolve_identity
    from ..core.instances import (
        load_instance_position,
        update_instance_position,
        get_full_name,
    )

    # Valid instance-level config keys and their DB column names
    INSTANCE_CONFIG_KEYS = {
        "tag": "tag",
        "timeout": "wait_timeout",
        "hints": "hints",
        "subagent_timeout": "subagent_timeout",
    }

    # Resolve instance name
    if target.lower() == "self":
        # Resolve current instance identity from environment
        try:
            identity = resolve_identity()
        except Exception as e:
            print(format_error(f"Cannot resolve identity: {e}"), file=sys.stderr)
            print("-i self requires running inside Claude/Gemini/Codex", file=sys.stderr)
            return 1
        instance_name = identity.name
    else:
        instance_name = target
    instance_data = load_instance_position(instance_name)
    if not instance_data:
        print(format_error(f"'{instance_name}' not found"), file=sys.stderr)
        return 1

    # No key specified: show all instance config
    if not argv:
        full_name = get_full_name(instance_data)
        config = {
            "name": instance_name,
            "full_name": full_name,
            "tag": instance_data.get("tag") or None,
            "timeout": instance_data.get("wait_timeout"),
            "hints": instance_data.get("hints") or None,
            "subagent_timeout": instance_data.get("subagent_timeout"),
        }
        if json_output:
            print(json.dumps(config))
        else:
            print(f"Agent: {full_name}")
            print(f"  tag: {config['tag'] or '(none)'}")
            print(f"  timeout: {config['timeout']}s")
            print(f"  hints: {config['hints'] or '(none)'}")
            print(f"  subagent_timeout: {config['subagent_timeout'] or '(default)'}s")
        return 0

    key = argv[0].lower()
    if key not in INSTANCE_CONFIG_KEYS:
        print(format_error(f"Unknown agent config key: {key}"), file=sys.stderr)
        print(
            f"Valid keys: {', '.join(sorted(INSTANCE_CONFIG_KEYS.keys()))}",
            file=sys.stderr,
        )
        return 1

    db_column = INSTANCE_CONFIG_KEYS[key]

    # Get value (no second arg)
    if len(argv) == 1:
        current_value = instance_data.get(db_column)
        if json_output:
            print(json.dumps({key: current_value}))
        else:
            if current_value is None:
                print("(none)")
            elif key == "timeout":
                print(f"{current_value}s")
            elif key == "subagent_timeout":
                print(f"{current_value}s" if current_value else "(default)")
            else:
                print(current_value if current_value else "(none)")
        return 0

    # Set value
    new_value = " ".join(argv[1:])  # Allow spaces in hints

    # Validate and convert based on key type
    db_value: str | int | None
    if key == "tag":
        if new_value and not all(c.isalnum() or c in "-_" for c in new_value):
            print(
                format_error("Tag must be alphanumeric (hyphens and underscores allowed)"),
                file=sys.stderr,
            )
            return 1
        db_value = new_value if new_value else None

    elif key in ("timeout", "subagent_timeout"):
        if new_value == "" or new_value.lower() == "default":
            db_value = None if key == "subagent_timeout" else 86400  # timeout has a default
        else:
            try:
                db_value = int(new_value)
                if db_value is not None and db_value < 0:
                    raise ValueError("negative")
            except ValueError:
                print(
                    format_error(f"{key} must be a positive integer (seconds)"),
                    file=sys.stderr,
                )
                return 1

    elif key == "hints":
        db_value = new_value if new_value else None

    else:
        db_value = new_value

    # Update in DB
    update_instance_position(instance_name, {db_column: db_value})  # type: ignore[misc]

    # Wake PTY delivery thread so terminal title updates instantly
    if key == "tag":
        from ..core.runtime import notify_instance
        notify_instance(instance_name)

    # Show result
    new_data = load_instance_position(instance_name)
    if json_output:
        print(json.dumps({key: new_data.get(db_column)}))
    else:
        if key == "tag":
            new_full_name = get_full_name(new_data)
            if db_value:
                print(f"Tag set to '{db_value}' - display name is now '{new_full_name}'")
            else:
                print(f"Tag cleared - display name is now '{new_full_name}'")
        elif key == "timeout":
            print(f"Timeout set to {db_value}s")
        elif key == "subagent_timeout":
            if db_value:
                print(f"Subagent timeout set to {db_value}s")
            else:
                print("Subagent timeout cleared (using default)")
        elif key == "hints":
            if db_value:
                print(f"Hints set to: {db_value}")
            else:
                print("Hints cleared")

    return 0


def cmd_config(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Config management: hcom config [key] [value] [--json] [--edit] [--reset] [-i <name>]

    Usage:
        hcom config              Show all config (pretty)
        hcom config --json       Show all config (JSON)
        hcom config <key>        Get single value
        hcom config <key> <val>  Set single value
        hcom config --edit       Open in $EDITOR
        hcom config --reset      Reset config to defaults

    Instance-level settings (-i <name>):
        hcom config -i <name>                 Show instance settings
        hcom config -i <name> tag <value>     Set instance tag (changes display name)
        hcom config -i <name> timeout <secs>  Set instance timeout
        hcom config -i <name> hints <text>    Set instance hints (injected with messages)
        hcom config -i <name> <key> ""        Clear setting
        hcom config -i self ...               Current instance (requires Claude context)
    """
    import subprocess
    from ..core.config import (
        load_config_snapshot,
        save_config_snapshot,
        hcom_config_to_dict,
        dict_to_hcom_config,
        HcomConfigError,
        KNOWN_CONFIG_KEYS,
        get_config_sources,
    )
    from ..core.paths import hcom_path, CONFIG_FILE
    from .utils import validate_flags, parse_name_flag
    from .reset import reset_config

    # Config doesn't use identity; direct calls may still pass --name.
    if ctx is None:
        _, argv = parse_name_flag(argv)

    # Validate flags
    if error := validate_flags("config", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Parse flags
    json_output = "--json" in argv
    edit_mode = "--edit" in argv
    reset_mode = "--reset" in argv
    setup_mode = "--setup" in argv
    argv = [a for a in argv if a not in ("--json", "--edit", "--reset", "--setup")]

    # --setup: only valid with 'terminal' key
    if setup_mode:
        positional = [a for a in argv if not a.startswith("-")]
        if not positional or positional[0] != "terminal":
            print(format_error("--setup is only valid with: hcom config terminal --setup"), file=sys.stderr)
            return 1

    # Parse -i <name> for instance-level config
    instance_target = None
    if "-i" in argv:
        idx = argv.index("-i")
        if idx + 1 >= len(argv):
            print(format_error("-i requires name (or 'self')"), file=sys.stderr)
            return 1
        instance_target = argv[idx + 1]
        argv = argv[:idx] + argv[idx + 2 :]

    # Handle instance-level config
    if instance_target is not None:
        return _config_instance(instance_target, argv, json_output)

    config_path = hcom_path(CONFIG_FILE)

    # --reset: archive and reset to defaults
    if reset_mode:
        return reset_config()

    # --edit: open in editor
    if edit_mode:
        editor = os.environ.get("EDITOR") or os.environ.get("VISUAL")
        if not editor:
            # Try common editors
            for ed in ["code", "vim", "nano", "vi"]:
                if shutil.which(ed):
                    editor = ed
                    break
        if not editor:
            print("No editor found. Set $EDITOR or install code/vim/nano", file=sys.stderr)
            return 1

        # Ensure config exists
        if not config_path.exists():
            load_config_snapshot()  # Creates default

        return subprocess.call(shlex.split(editor) + [str(config_path)])

    # Load current config
    snapshot = load_config_snapshot()
    # Get effective config (includes env overrides) for display
    from ..core.config import HcomConfig

    effective_config = HcomConfig.load()
    effective_dict = hcom_config_to_dict(effective_config)

    # No args: show all
    if not argv:
        if json_output:
            # JSON output: effective values + extras (mask sensitive values)
            output = {**effective_dict, **snapshot.extras}
            if output.get("HCOM_RELAY_TOKEN"):
                v = output["HCOM_RELAY_TOKEN"]
                output["HCOM_RELAY_TOKEN"] = f"{v[:4]}***" if len(v) > 4 else "***"
            print(json.dumps(output, indent=2))
        else:
            # Pretty output with source indicators
            sources = get_config_sources()
            print(f"Config: {config_path}\n")

            # Check for runtime overrides (instance-level settings from DB)
            # Mapping: DB column -> config key
            RUNTIME_KEYS = {
                "tag": "HCOM_TAG",
                "wait_timeout": "HCOM_TIMEOUT",
                "hints": "HCOM_HINTS",
                "subagent_timeout": "HCOM_SUBAGENT_TIMEOUT",
            }
            runtime_overrides = {}
            from .utils import resolve_identity
            from ..core.instances import load_instance_position

            try:
                identity = resolve_identity()
                instance_name = identity.name
                instance_data = load_instance_position(instance_name)
                if instance_data:
                    for db_col, config_key in RUNTIME_KEYS.items():
                        val = instance_data.get(db_col)
                        global_val = effective_dict.get(config_key, "")
                        # Only mark as runtime if different from global
                        if val is not None and str(val) != str(global_val):
                            runtime_overrides[config_key] = str(val)
            except Exception:
                pass  # Not in Claude context, no runtime overrides

            # Source legend
            SOURCE_LABELS = {
                "env": "[env]",
                "file": "[file]",
                "default": "",
                "runtime": "[runtime]",
            }

            # Core hcom settings (show effective values)
            print("hcom Settings:")
            for key in KNOWN_CONFIG_KEYS:
                # Check runtime override first
                if key in runtime_overrides:
                    value = runtime_overrides[key]
                    source_label = "[runtime]"
                else:
                    value = effective_dict.get(key, "")
                    source = sources.get(key, "default")
                    source_label = SOURCE_LABELS.get(source, "")
                # Mask sensitive values, truncate long values
                if key == "HCOM_RELAY_TOKEN" and value:
                    display_val = f"{value[:4]}***" if len(value) > 4 else "***"
                else:
                    display_val = value if len(value) <= 60 else value[:57] + "..."
                # Right-align source label
                if source_label:
                    print(f"  {key}={display_val}  {source_label}")
                else:
                    print(f"  {key}={display_val}")

            # Extra env vars
            if snapshot.extras:
                print("\nExtra Environment Variables:")
                for key in sorted(snapshot.extras.keys()):
                    value = snapshot.extras[key]
                    display_val = value if len(value) <= 60 else value[:57] + "..."
                    # Check if from env or file
                    src = "[env]" if key in os.environ else "[file]"
                    print(f"  {key}={display_val}  {src}")

            print("\n[env] = environment, [file] = config.env, [runtime] = agent override, (blank) = default")
            print("\nEdit: hcom config --edit")
        return 0

    # Get file-based config dict for get/set operations
    core_dict = hcom_config_to_dict(snapshot.core)

    # Handle "key --info", "key info", or "key ?" for detailed help
    # Check for --info flag anywhere in argv
    show_info = "--info" in argv
    if show_info:
        argv = [a for a in argv if a != "--info"]

    # Also support "key info" or "key ?" syntax
    if len(argv) == 2 and argv[1] in ("?", "info"):
        show_info = True
        argv = [argv[0]]

    if show_info and len(argv) == 1:
        key = argv[0].upper()
        if not key.startswith("HCOM_"):
            key = f"HCOM_{key}"
        current_val = core_dict.get(key, "") or effective_dict.get(key, "")
        help_text = _get_config_help(key, current_val)
        if help_text:
            print(help_text)
            return 0
        else:
            print(f"No detailed help available for {key}", file=sys.stderr)
            print(f"Use 'hcom config {argv[0]}' to see current value", file=sys.stderr)
            return 1

    # Single arg: get value
    if len(argv) == 1:
        key = argv[0].upper()
        if not key.startswith("HCOM_"):
            key = f"HCOM_{key}"

        # Rich output for HCOM_TERMINAL bare get
        if key == "HCOM_TERMINAL" and not json_output:
            if setup_mode:
                return _kitty_setup()
            current = core_dict.get(key, "") or effective_dict.get(key, "")
            return _show_terminal_status(current)

        if key in core_dict:
            value = core_dict[key]
        elif key in snapshot.extras:
            value = snapshot.extras[key]
        else:
            # Check if it's a known key with empty value
            if key in KNOWN_CONFIG_KEYS:
                value = ""
            else:
                from .utils import get_command_help

                print(f"Unknown config key: {key}", file=sys.stderr)
                print(f"Valid keys: {', '.join(KNOWN_CONFIG_KEYS)}\n", file=sys.stderr)
                print(get_command_help("config"), file=sys.stderr)
                return 1

        # Mask sensitive values in display
        display_value = value
        if key == "HCOM_RELAY_TOKEN" and value:
            display_value = f"{value[:4]}***" if len(value) > 4 else "***"
        if json_output:
            print(json.dumps({key: display_value}))
        else:
            print(display_value)
        return 0

    # Two args: set value
    if len(argv) >= 2:
        key = argv[0].upper()
        if not key.startswith("HCOM_"):
            key = f"HCOM_{key}"

        value = " ".join(argv[1:])  # Allow spaces in value

        # Validate key - must be a known HCOM config key
        if key not in KNOWN_CONFIG_KEYS:
            from .utils import get_command_help

            print(f"Unknown config key: {key}", file=sys.stderr)
            print(f"Valid keys: {', '.join(sorted(KNOWN_CONFIG_KEYS))}\n", file=sys.stderr)
            print(get_command_help("config"), file=sys.stderr)
            return 1

        # Update config
        new_core_dict = {**core_dict, key: value}
        try:
            new_core = dict_to_hcom_config(new_core_dict)
            snapshot.core = new_core
        except HcomConfigError as e:
            print(f"Invalid value: {e}", file=sys.stderr)
            return 1

        # Save
        save_config_snapshot(snapshot)
        if key == "HCOM_TERMINAL":
            if _is_managed_preset(value):
                print(f"Set {key}={value} (managed — kill closes panes)")
            elif value == "default" or value == "":
                print(f"Set {key}={value} ({_resolve_default_terminal_name()})")
            else:
                print(f"Set {key}={value} (open only — kill can't close windows)")
            # Kitty: check socket / run setup
            if value in ("kitty", "kitty-tab", "kitty-split"):
                if setup_mode:
                    _kitty_setup()
                else:
                    _show_kitty_status(value)
        else:
            print(f"Set {key}={value}")

        # Handle HCOM_AUTO_APPROVE changes - update permissions in all tools
        if key == "HCOM_AUTO_APPROVE":
            enabled = value not in ("0", "false", "False", "no", "off", "")
            from ..tools.claude.settings import setup_claude_hooks
            from ..tools.gemini.settings import setup_gemini_hooks
            from ..tools.codex.settings import setup_codex_hooks

            # Update permissions in all 3 tools
            try:
                setup_claude_hooks(include_permissions=enabled)
            except Exception:
                pass  # Claude hooks may not be set up
            try:
                setup_gemini_hooks(include_permissions=enabled)
            except Exception:
                pass  # Gemini hooks may not be set up
            try:
                setup_codex_hooks(include_permissions=enabled)
            except Exception:
                pass  # Codex hooks may not be set up

            if enabled:
                print("Auto-approve enabled for safe hcom commands in Claude/Gemini/Codex")
            else:
                print("Auto-approve disabled - safe hcom commands will require approval")

        return 0

    return 0
