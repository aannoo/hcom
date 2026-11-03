# Claude pipe clients

On Unix, `hcom claude --stdio --go -- -p ...` runs one foreground Claude
print process with the caller's stdin, stdout and stderr. This supports clients
such as the Claude Agent SDK, whose JSON control channel cannot run in a PTY.
The client must supply its own native Claude flags, including `-p`/`--print`.
hcom preserves the native arguments and does not add stream format defaults.

hcom prepares its usual per-launch hooks, reserves a fresh process identity,
and remains the native child's parent. It does not create a terminal, print a
launch banner, or redirect output to a background log. Native `SessionStart`
binds the real session ID. Signals reach the child's process group; hcom returns
its exit status and cleans up its registration if `SessionEnd` was not emitted.

The client owns turn completion and idle input. `Stop` drains messages already
queued, then returns immediately when none remain. Messages arriving while the
client is idle are delivered by the next native prompt or tool/Stop hook.
hcom does not inject synthetic SDK prompts. Pending delivery is acknowledged
only after hook output has been flushed, as in the other native modes.

`--stdio` cannot be combined with multiple agents, another tool, a remote
device, a terminal preset, `--headless`, `--json`, or `--no-run-here`. It is a
launch transport; clients resume the native conversation by passing Claude's
`--resume <session-id>` under a fresh `hcom claude --stdio` launch. Existing
interactive and detached print modes retain their behavior.
