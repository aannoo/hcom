"""OpenCode integration for hcom.

Provides hook handlers for OpenCode plugin commands that shell out to hcom.

Components:
    hooks.py: Hook command handlers (opencode-start, opencode-status, opencode-read)
        - opencode-start: Bind session to process, set listening status
        - opencode-status: Update instance status
        - opencode-read: Fetch pending messages, check for messages, advance cursor

Architecture Note:
    OpenCode hooks use argv flags (--session-id, --name, --status), not JSON payload.
    No HookPayload -- argv is parsed directly via _parse_flag() helper.
    Commands return JSON to stdout for the TypeScript plugin to parse.
"""
