"""Standard return type from hook handlers.

HookResult replaces sys.exit()/print() patterns, enabling daemon integration
where output must be captured rather than written to global streams.

Design:
- exit_code: replaces sys.exit(N)
- stdout: replaces print() and json.dumps() to stdout
- stderr: replaces print(..., file=sys.stderr)
- Helper methods for common patterns (success, error, stop_with_messages)
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any


@dataclass
class HookResult:
    """Standard return from hook handlers.

    Captures what would normally be written to stdout/stderr and exit codes,
    enabling daemon integration where global streams aren't available.

    Attributes:
        exit_code: Process exit code (0=success, 1=error, 2=message delivered for Stop hook).
        stdout: Text/JSON to write to stdout.
        stderr: Text to write to stderr (errors/warnings).
        hook_output: Structured output for hook-specific responses (e.g., updatedInput).

    Usage:
        # In handler:
        return HookResult.success('{"hookSpecificOutput": {...}}')

        # In daemon:
        result = handle_hook_with_context(...)
        if result.stdout:
            send_response(result.stdout)
        return result.exit_code
    """

    exit_code: int = 0
    stdout: str = ""
    stderr: str = ""
    hook_output: dict[str, Any] | None = field(default=None)

    @classmethod
    def success(cls, stdout: str = "") -> "HookResult":
        """Create successful result with optional stdout.

        Args:
            stdout: Text/JSON to output (e.g., hook response JSON).

        Returns:
            HookResult with exit_code=0.
        """
        return cls(exit_code=0, stdout=stdout)

    @classmethod
    def error(cls, message: str, exit_code: int = 1) -> "HookResult":
        """Create error result.

        Args:
            message: Error message for stderr.
            exit_code: Exit code (default 1).

        Returns:
            HookResult with specified exit code and stderr message.
        """
        return cls(exit_code=exit_code, stderr=message)

    @classmethod
    def allow_with_context(cls, hook_event: str, context: str) -> "HookResult":
        """Create result that allows operation and injects additional context.

        Used for hooks like BeforeAgent/AfterTool that inject messages.

        Args:
            hook_event: Hook event name (e.g., "BeforeAgent", "AfterTool").
            context: Additional context to inject.

        Returns:
            HookResult with allow decision and context.
        """
        output = {
            "decision": "allow",
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "additionalContext": context,
            },
        }
        return cls(
            exit_code=0,
            stdout=json.dumps(output),
            hook_output=output,
        )

    def is_success(self) -> bool:
        """Check if result indicates success."""
        return self.exit_code == 0

    def is_error(self) -> bool:
        """Check if result indicates error."""
        return self.exit_code != 0 and self.exit_code != 2

    def is_message_delivered(self) -> bool:
        """Check if result indicates message was delivered (Stop hook)."""
        return self.exit_code == 2


__all__ = ["HookResult"]
