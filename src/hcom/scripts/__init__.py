"""Script discovery and execution.

Scripts are Python or shell files that implement multi-agent workflows.
- Bundled scripts: src/hcom/scripts/bundled/ (run directly from package)
- User scripts: ~/.hcom/scripts/ (shadow bundled by name)
- Launch profiles: ~/.hcom/launches/ (auto-captured, run via hcom run)
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Any

from ..core.paths import hcom_path

SCRIPTS_DIR = "scripts"


def get_bundled_dir() -> Path:
    """Get path to bundled scripts in package."""
    return Path(__file__).parent / "bundled"


def get_user_dir() -> Path:
    """Get path to user scripts directory."""
    return hcom_path(SCRIPTS_DIR)


def discover_scripts() -> list[dict[str, Any]]:
    """Find all available scripts.

    Returns list of dicts with keys: name, path, source, description

    Source values:
    - "bundled": shipped with hcom
    - "user": user-created script (shadows bundled if same name)
    """
    user_dir = get_user_dir()
    bundled_dir = get_bundled_dir()
    scripts = []
    seen = set()

    # User scripts first (they shadow bundled)
    if user_dir.exists():
        for f in sorted(user_dir.glob("*.py")):
            if f.name.startswith(("_", ".")):
                continue
            seen.add(f.stem)
            scripts.append(
                {
                    "name": f.stem,
                    "path": f,
                    "source": "user",
                    "description": _extract_description(f),
                }
            )
        for f in sorted(user_dir.glob("*.sh")):
            if f.name.startswith(("_", ".")):
                continue
            seen.add(f.stem)
            scripts.append(
                {
                    "name": f.stem,
                    "path": f,
                    "source": "user",
                    "description": _extract_description(f),
                }
            )

    # Bundled scripts (if not shadowed)
    for f in sorted(bundled_dir.glob("*.py")):
        if f.name.startswith("_"):
            continue
        if f.stem not in seen:
            scripts.append(
                {
                    "name": f.stem,
                    "path": f,
                    "source": "bundled",
                    "description": _extract_description(f),
                }
            )

    return scripts


def _extract_description(path: Path) -> str:
    """Extract first line of docstring or comment as description."""
    try:
        content = path.read_text()
        lines = content.split("\n")

        if path.suffix == ".py":
            in_docstring = False
            for line in lines:
                stripped = line.strip()
                if stripped.startswith('"""') or stripped.startswith("'''"):
                    if stripped.count('"""') >= 2 or stripped.count("'''") >= 2:
                        return stripped.strip("\"'").strip()
                    in_docstring = True
                    rest = stripped[3:].strip()
                    if rest:
                        return rest.rstrip("\"'").strip()
                elif in_docstring:
                    if stripped.endswith('"""') or stripped.endswith("'''"):
                        return stripped.rstrip("\"'").strip()
                    if stripped:
                        return stripped

        elif path.suffix == ".sh":
            for line in lines:
                stripped = line.strip()
                if stripped.startswith("#!"):
                    continue
                if stripped.startswith("#"):
                    return stripped[1:].strip()
                if stripped:
                    break

        return ""
    except Exception:
        return ""


def find_script(name: str) -> dict[str, Any] | None:
    """Find a script by name."""
    for script in discover_scripts():
        if script["name"] == name:
            return script
    return None


def _generate_cli_docs() -> str:
    """Generate CLI documentation from existing help registry."""
    from ..commands.utils import get_help_text, COMMAND_HELP, get_command_help

    lines = [get_help_text(), ""]

    # Add detailed help for each command
    for cmd in COMMAND_HELP:
        lines.append(f"\n## {cmd}\n")
        lines.append(get_command_help(cmd))

    return "\n".join(lines)


def _generate_script_creation_docs() -> str:
    """Generate documentation for creating custom scripts."""
    user_dir = get_user_dir()
    bundled_dir = get_bundled_dir()

    # Generate reference examples dynamically
    bundled = [s for s in discover_scripts() if s["source"] == "bundled"]
    examples_lines = []
    for s in bundled:
        name = s["name"]
        desc = s["description"] or name
        agents = BUNDLED_AGENTS.get(name, "")
        comment = f"{desc} ({agents})" if agents else desc
        examples_lines.append(f"  hcom run {name} --source    # {comment}")
    examples_text = "\n".join(examples_lines)

    return f'''## Location

User scripts (shadow bundled by name):
  {user_dir}

Bundled scripts (reference examples):
  {bundled_dir}

File types:
  *.py   Python scripts (executable with python3)
  *.sh   Shell scripts (executable with bash)

## Script Structure Template

```python
#!/usr/bin/env python3
"""Brief one-line description shown in hcom run list."""
import argparse
import sys
import hcom

def main():
    parser = argparse.ArgumentParser(description="...")
    parser.add_argument('--target', help='target instance')
    parser.add_argument('--name', help='instance identity (optional)')

    args = parser.parse_args()

    s = hcom.session(name=args.name) if args.name else hcom.session()
    s.send("@target hello")

    return 0

if __name__ == '__main__':
    sys.exit(main())
```

## Common Workflow Patterns

Launch agents:
  instances = hcom.launch(count=3, tool='claude', tag='worker',
                          prompt='your task here')

Send messages:
  s = hcom.session()
  s.send("@luna check this")               # to specific instance
  s.send("@group- broadcast to group")    # to all in group
  s.send("message", intent="request")     # with envelope

Subscribe to events:
  s.subscribe("instance='luna' AND type='status'")
  s.wait()  # block until event matches

Read transcripts:
  exchanges = s.transcript('luna', last=5, detailed=True)    # specific agent
  timeline = s.transcript('timeline', last=20)               # all agents (timeline)

Check messages:
  for msg in s.messages():
      print(msg['text'])

## Identity Handling

Auto-detect (when run from hcom instance):
  s = hcom.session()

Explicit (for testing or specific workflows):
  s = hcom.session(name="watcher-luna")

External (script not run by an instance):
  s = hcom.session(name="ci-bot", external=True)

Support --name flag in your script:
  parser.add_argument('--name', help='instance identity')
  s = hcom.session(name=args.name) if args.name else hcom.session()

## Reference Examples

View bundled script sources as working examples:
{examples_text}

## Best Practices

- Use hcom API for all hcom operations (not CLI commands in subprocess)
- Support --name flag for identity (allows external callers)
- Scripts can call other scripts: hcom.launch() or subprocess to hcom run
'''


def _generate_config_details() -> str:
    """Generate detailed config documentation for all keys."""
    from ..commands.config_cmd import _get_config_help

    # Intro explaining config.env
    intro = """Config is stored in ~/.hcom/config.env (or $HCOM_DIR/config.env).

Commands:
  hcom config                 Show all values
  hcom config <key> <val>     Set value
  hcom config <key> --info    Detailed help for a setting
  hcom config --edit          Open in $EDITOR

Precedence: defaults < config.env < shell environment variables"""

    keys = [
        "HCOM_TAG",
        "HCOM_TERMINAL",
        "HCOM_HINTS",
        "HCOM_NOTES",
        "HCOM_SUBAGENT_TIMEOUT",
        "HCOM_CLAUDE_ARGS",
        "HCOM_GEMINI_ARGS",
        "HCOM_CODEX_ARGS",
        "HCOM_RELAY",
        "HCOM_RELAY_TOKEN",
        "HCOM_RELAY_ENABLED",
        "HCOM_AUTO_APPROVE",
        "HCOM_AUTO_SUBSCRIBE",
        "HCOM_NAME_EXPORT",
    ]

    sections = [intro]
    for key in keys:
        help_text = _get_config_help(key)
        if help_text:
            sections.append(f"## {key}\n\n{help_text.strip()}")

    return "\n\n".join(sections)


def print_docs(*, cli: bool = False, config: bool = False, api: bool = False) -> int:
    """Print generated API reference + example commands.

    Args:
        cli: Show only CLI reference
        config: Show only Config Settings reference
        api: Show only Python API reference (includes scripts guide and examples)
    """
    # If no filter specified, show all
    show_all = not (cli or config or api)

    if show_all:
        # Table of contents
        print("# hcom Documentation\n")
        print("Contents:")
        print("  1. CLI Reference - All commands and options")
        print("  2. Config Settings Reference - Detailed config documentation")
        print("  3. Creating Custom Scripts - Script templates and patterns")
        print("  4. Python API Reference - hcom module functions")
        print("  5. Examples - Workflow script sources")
        print()

    # CLI reference
    if show_all or cli:
        print("# hcom CLI Reference\n")
        print(_generate_cli_docs())

    # Config settings reference
    if show_all or config:
        print("\n# Config Settings Reference\n")
        print(_generate_config_details())

    # Python API (includes scripts guide and examples)
    if show_all or api:
        # Script creation guide
        print("\n# Creating Custom Scripts\n")
        print(_generate_script_creation_docs())

        # Python API reference
        print("\n# hcom Python API Reference\n")
        print(_generate_api_docs())

        # Example commands
        print("\n# Examples\n")
        print("View workflow script sources:\n")
        for script in discover_scripts():
            print(f"  hcom run {script['name']} --source")

    return 0


def _generate_api_docs() -> str:
    """Generate API documentation from docstrings."""
    import hcom
    from hcom.api import Session

    lines = []

    # Module-level functions
    for name in ["session", "instances", "launch", "bundle"]:
        func = getattr(hcom, name, None)
        if func and func.__doc__:
            lines.append(f"## hcom.{name}()\n")
            lines.append(func.__doc__.strip())
            lines.append("")

    # Session class
    lines.append("## Session\n")
    if Session.__doc__:
        lines.append(Session.__doc__.strip())
        lines.append("")

    # Session methods
    for name in [
        "name",
        "info",
        "send",
        "messages",
        "events",
        "wait",
        "subscribe",
        "subscriptions",
        "unsubscribe",
        "transcript",
        "stop",
    ]:
        attr = getattr(Session, name, None)
        if attr:
            doc = getattr(attr, "__doc__", None) or (attr.fget.__doc__ if hasattr(attr, "fget") else None)
            if doc:
                lines.append(f"### Session.{name}\n")
                lines.append(doc.strip())
                lines.append("")

    # HcomError
    lines.append("## hcom.HcomError\n")
    lines.append("Exception raised for hcom errors.")

    return "\n".join(lines)


def run_script(argv: list[str]) -> int:
    """Run a script, show its source, or print docs.

    Usage:
        run_script(['confess', '--help'])       # Run with --help
        run_script(['confess', '--source'])     # Print source
        run_script(['docs'])                    # Print API docs
        run_script(['--name', 'X', 'confess'])  # Run with identity
    """
    if not argv:
        return list_scripts()

    # Handle --source flag anywhere in args
    show_source = "--source" in argv
    if show_source:
        argv = [a for a in argv if a != "--source"]

    # Find script name (skip --name flag and value)
    script_idx = None
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--name" and i + 1 < len(argv):
            i += 2
        elif arg.startswith("-"):
            i += 1
        else:
            script_idx = i
            break

    if script_idx is None:
        return list_scripts()

    name = argv[script_idx]
    args = argv[:script_idx] + argv[script_idx + 1 :]

    # Special: docs command
    if name == "docs":
        # Parse section flags
        show_cli = "--cli" in args
        show_config = "--config" in args
        show_api = "--api" in args
        return print_docs(cli=show_cli, config=show_config, api=show_api)

    # Check for script
    script = find_script(name)
    if not script:
        print(f"Unknown script: {name}")
        print("Run 'hcom run' to list available scripts")
        return 1

    # --source: print source and exit
    if show_source:
        print(script["path"].read_text())
        return 0

    # Run the script.
    # Pipe stdout/stderr so output goes through sys.stdout/sys.stderr (which may be
    # thread-local CaptureBuffers in daemon mode) instead of inheriting real fds.
    path = script["path"]
    cmd = [sys.executable, str(path)] + args if path.suffix == ".py" else ["bash", str(path)] + args
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    return result.returncode


# Bundled script agent counts
BUNDLED_AGENTS = {
    "clone": "1 fork",
    "confess": "3 agents",
    "debate": "2+ agents",
    "ensemble": "3-6 agents",
    "glue": "1 agent",
    "ralph": "1 agent loop",
    "watcher": "1 agent",
}


def list_scripts() -> int:
    """List all available scripts."""
    scripts = discover_scripts()

    # Separate bundled and user scripts
    bundled = [s for s in scripts if s["source"] == "bundled"]
    user = [s for s in scripts if s["source"] == "user"]

    if not scripts:
        print("No scripts available")
        return 0

    # Show examples (bundled scripts)
    if bundled:
        print("Examples:")
        print()
        for s in bundled:
            name = s["name"]
            agents = BUNDLED_AGENTS.get(name, "")
            desc = s["description"]

            agents_part = f"  ({agents})" if agents else ""
            print(f"  {name}{agents_part}")
            print(f"      {desc}")
        print()

    # Show user scripts
    if user:
        print("User Scripts:")
        print()
        for s in user:
            name = s["name"]
            desc = s["description"]
            print(f"  {name}")
            print(f"      {desc}")
        print()
    else:
        print("User Scripts:")
        print()
        print("  No custom scripts yet.")
        print()
        print("  Run 'hcom run docs' to create a custom script:")
        print("    - Multi-agent workflows")
        print("    - Background watchers")
        print("    - Task automation")
        print("    - etc...")
        print()

    print("Commands:")
    print("  hcom run <script>           Run workflow script")
    print("  hcom run <script> --source  View script source")
    print("  hcom run <script> --help    Script help")
    print("  hcom run docs               API reference + example commands")
    print("    --cli                     CLI reference only")
    print("    --config                  Config settings only")
    print("    --api                     Python API + scripts guide")

    return 0


__all__ = [
    "discover_scripts",
    "find_script",
    "run_script",
    "list_scripts",
    "print_docs",
    "get_user_dir",
    "get_bundled_dir",
]
