"""One-time tips shown on first use of commands.

Uses kv store to track which tips have been shown per instance.
"""

from __future__ import annotations

TIPS = {
    "list:status": (
        "[tip] Statuses: ▶ active (will read new msgs very soon)  ◉ listening (will read new msgs in <1s)"
        "  ■ blocked (needs human user approval)  ○ inactive (dead)  ◦ unknown (neutral)"
    ),
    "list:types": (
        "[tip] Types: [CLAUDE] [GEMINI] [CODEX] [claude] full features, automatic msg delivery"
        " | [AD-HOC] [gemini] [codex] limited"
    ),
    # Send-side (shown after send with --intent)
    "send:intent:request": "[tip] intent=request: You signaled you expect a response.",
    "send:intent:inform": "[tip] intent=inform: You signaled no response needed.",
    "send:intent:ack": "[tip] intent=ack: You acknowledged receipt. Recipient won't respond.",
    # Recv-side (appended to message on first receipt of each type)
    "recv:intent:request": "[tip] intent=request: Sender expects a response.",
    "recv:intent:inform": "[tip] intent=inform: Sender doesn't expect a response.",
    "recv:intent:ack": "[tip] intent=ack: Sender confirmed receipt. No response needed.",
    # @mention matching
    "mention:matching": "[tip] @targets: @api- matches all with tag 'api' | @luna matches prefix | underscore blocks: @luna won't match luna_sub_1",
}


def _tip_key(instance_name: str, command: str) -> str:
    """Get kv key for tip tracking."""
    return f"tip:{instance_name}:{command}"


def has_seen_tip(instance_name: str, command: str) -> bool:
    """Check if instance has seen this tip before."""
    if not instance_name:
        return True
    from .db import kv_get

    return kv_get(_tip_key(instance_name, command)) is not None


def mark_tip_seen(instance_name: str, command: str):
    """Mark tip as seen for this instance."""
    if not instance_name:
        return
    from .db import kv_set

    kv_set(_tip_key(instance_name, command), "1")


def maybe_show_tip(instance_name: str, command: str, *, json_output: bool = False):
    """Show one-time tip for command if not seen before."""
    if json_output or command not in TIPS:
        return
    if has_seen_tip(instance_name, command):
        return
    mark_tip_seen(instance_name, command)
    print(f"\n{TIPS[command]}")


def print_launch_tips(
    *,
    launched: int,
    tag: str | None,
    instance_names: list[str],
    launcher_name: str | None,
    launcher_participating: bool,
    background: bool,
) -> None:
    """Print contextual tips after launch. One-time tips tracked per launcher via kv."""
    if launched == 0:
        return

    from ..shared import is_inside_ai_tool

    inside_tool = is_inside_ai_tool()
    tips: list[str] = []

    # Identity key for one-time tracking (fallback for human launches)
    tip_id = launcher_name or "_global"

    name_ex = instance_names[0] if instance_names else "<name>"
    target = name_ex if launched == 1 and instance_names else (f"tag:{tag}" if tag else "<name>")

    def _once(key: str, text: str) -> None:
        """Append tip if not yet seen by this launcher."""
        if not has_seen_tip(tip_id, key):
            mark_tip_seen(tip_id, key)
            tips.append(text)

    # Terminal-mode awareness
    from .config import get_config
    from .settings import get_merged_presets

    terminal_mode = get_config().terminal
    merged = get_merged_presets()
    has_close = bool(merged.get(terminal_mode, {}).get("close"))
    if terminal_mode in ("kitty", "wezterm"):
        has_close = True
    is_tmux = terminal_mode.startswith("tmux")

    managed = "managed" if has_close else "unmanaged"
    tips.append(f"[info] Terminal: {terminal_mode} ({managed})")

    # --- Always-shown (batch-specific) ---

    if tag:
        tips.append(f"[tip] Send to {tag} team: hcom send '@{tag}- message'")

    if inside_tool and launcher_participating:
        _once("launch:notify", "[tip] You'll be automatically notified when instances are launched & ready")

    # --- One-time (kv-tracked) ---

    if inside_tool:
        if not launcher_participating:
            _once("launch:start", "[tip] Run 'hcom start' to receive notifications/messages from instances")

        _once("launch:stop", f"[tip] Disconnect: hcom stop {target}")

        if has_close:
            _once("launch:kill", f"[tip] Kill + close pane: hcom kill {target}")

        if not background:
            _once("launch:term", f"[tip] View screen: hcom term {name_ex} | Inject: hcom term inject {name_ex} [text] --enter")

        if is_tmux:
            _once("launch:sub-blocked", f"[tip] Subscribe to blocked: hcom events sub --blocked {name_ex}")
        else:
            _once("launch:sub-idle", f"[tip] Subscribe to idle: hcom events sub --idle {name_ex}")

        _once("list:status", TIPS["list:status"])
    else:
        _once("launch:send", f"[tip] Send message: hcom send '@{name_ex} hello'")
        _once("launch:list", "[tip] Check status: hcom list")

    if tips:
        print("\n" + "\n".join(tips))
