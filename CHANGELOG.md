# Changelog

UTC release dates from [PyPI](https://pypi.org/project/hcom/#history).

## Unreleased

### Added

- Added a conversation tab to the TUI's vertical view (`c`): the selected agent's recent turns with the request, a tool summary and the reply, for local and remote agents. The latest turn is open and earlier ones are one line each; `e` shows every turn in full, `R` refreshes, and Enter messages the agent. Markdown headings, lists, bold, code and tables are rendered. Set `HCOM_TUI_TAB=chat` to open vertical view on it.
- Showed which account a Claude or Codex agent runs under (tool and logged-in e-mail, read from `CLAUDE_CONFIG_DIR` / `CODEX_HOME` when set) in the conversation tab header. Remote agents' e-mail is shared only when their device sets `hcom config relay_share_account 1`, and only for agents started after upgrading; a stopped agent's account is not shown.
- Showed a spinner with the current tool and elapsed time under the conversation while the agent is working.
- Answered a Claude agent's choice prompt from the conversation tab: while a local agent waits on a question (`AskUserQuestion`) or a permission prompt, its options are read off the agent's screen and listed under the conversation; a digit picks one and Enter moves on from a multi-select question. Keys are sent only while the same prompt is still on screen. Options that need typing are left to the agent's terminal.
- Marked a blocked agent in the conversation tab and in the compose footer: messages to it are held until its prompt is answered.

## 0.7.31 — 2026-10-21

### Breaking changes

- Dropped ephemeral `uvx hcom` support. Run `uv tool install hcom` and restart affected sessions; hooks and agents need a persistent `hcom` on PATH. Legacy hooks can still be removed with `uvx hcom hooks remove`.

### Added

- Added `hcom events --follow` to stream matching events as JSON Lines, with `--last N` to replay recent matches first and `--after-id ID` to resume from a saved cursor.
- Added `--json` output to launch, `hcom r`, `hcom f`, and `hcom kill`, so scripts no longer have to parse names out of the text output.
- Added `bootstrap_exclude` to leave parts of the agent bootstrap out (`identity`, `messages`, `capabilities`, `rules`, `handoff`, or `all`), for projects with their own orchestration rules. Inspect with `hcom config bootstrap show` / `render` [#167](https://github.com/aannoo/hcom/issues/167).
- Added longer agent names (6 letters, then more) once every 4-letter name has been used, instead of failing to launch. Names are still never reused [#166](https://github.com/aannoo/hcom/issues/166).

### Changed

- Made native delivery the default for idle Claude and Copilot agents: hcom hands messages to the agent directly through the Claude mod or Copilot extension instead of typing into the terminal. Switch back with `hcom config claude_delivery inject` / `copilot_delivery inject`.
- Showed the command's help page after a syntax error, with did-you-mean suggestions for subcommands.
- Showed how to unsubscribe when creating an `hcom events sub` subscription.
- Made a flag passed to `hcom r` / `hcom f` replace its saved copy for every tool, and dropped saved flags that conflict with resuming a session (Kimi `--agent`, Pi/OMP `--no-session`, OMP `--cwd`/`--goal`, Qoder remote-session flags, Kilo `--worktree`/`--cloud-fork`).
- Made `hcom term --json` report `prompt_empty: null` when hcom can't read the tool's input box, instead of `false` [#162](https://github.com/aannoo/hcom/issues/162).
- Showed session token usage in the TUI when context usage is unavailable.
- Asked for confirmation before `reset` in the TUI command palette; hid relay join tokens in the TUI.
- Made `hcom events --remote-fetch` return answers up to the relay's current size limit (about 1 MiB between up-to-date devices) instead of 96 KiB, and end a cut answer with a `truncated` line saying what was left out and how to narrow the query.
- Made remote agents' status details previews in `hcom list` and the TUI; `hcom list -v` shows the `hcom events --remote-fetch … --full` command for the full text. Relayed status events still carry the whole detail.
- Stopped writing `old_*`, `new_*` and `writer` fields into every status event; they stored each long command or prompt three times in the database and the relay.

### Fixed

- Fixed TUI command-palette launches, resumes, and forks blocking the TUI when they tried to run in its subprocess.
- Fixed launch, resume, and fork argument overrides accumulating duplicate singleton options, which broke later Codex resumes and forks [#173](https://github.com/aannoo/hcom/issues/173).
- Fixed Pi and OMP resumes replaying the original prompt after a boolean flag (`hcom pi --verbose "fix x"`).
- Fixed `hcom r <session-id>` resuming the agent's newest conversation instead of the one asked for, and `hcom r` not finding an agent named `resume`.
- Fixed `hcom r` dropping the agent's event subscriptions and thread memberships; a subscription that matched while the agent was stopped now notifies on resume.
- Fixed Antigravity and OMP agents losing their event subscriptions at the end of a turn while still running.
- Fixed a hook from the previous launch taking a resumed agent's messages.
- Fixed `hcom kill` closing an unrelated tmux pane on another tmux server, and `tmux-split` launches on systems without `/bin/bash` (Termux, NixOS) [#168](https://github.com/aannoo/hcom/issues/168), [#171](https://github.com/aannoo/hcom/pull/171) — @sean0407.
- Fixed Grok delivering a message twice when it was steered into a running turn [#169](https://github.com/aannoo/hcom/pull/169) — @zane668.
- Fixed Grok mail stuck or duplicated after Grok's leader restarted (e.g. auto-update) or the bridge dropped [#163](https://github.com/aannoo/hcom/pull/163) — @signet-bot-claude.
- Fixed hcom undoing a model or reasoning effort switch made in Grok's TUI.
- Fixed Claude delivery stalling after a wake-up found no pending messages [#165](https://github.com/aannoo/hcom/pull/165) — @signet-bot-claude.
- Fixed Codex and Cursor messages waiting forever behind an already answered approval while the screen kept redrawing (e.g. Codex `/voice`) [#164](https://github.com/aannoo/hcom/pull/164) — @signet-bot-claude.
- Fixed a race that could overwrite an agent's `blocked` approval status.
- Fixed concurrent `hcom config` changes overwriting each other; `hcom config -i` now errors on an ambiguous name prefix, and a broken config.toml is reported instead of silently showing defaults.
- Fixed deleted events still appearing in search.
- Fixed relay agents disappearing from other devices after a local `hcom reset`.
- Fixed Kimi agents not showing as active while running tools.
- Fixed `hcom term debug on` not applying to agents whose output never stops (e.g. Codex `/voice`).
- Fixed TUI messages without recipients going to an agent named `all` instead of everyone.
- Fixed TUI issues: multiline messages collapsed into one line, Ctrl+K opening the kill prompt, stopped agents always showing "just now", agent list overflow and a crash on non-ASCII session IDs, a local and remote agent with the same name selected together, a hung command freezing the TUI, the scrolled view jumping on new messages, and the last frame left on screen after quitting.
- Fixed the TUI launch wizard dropping pasted text and the prompt on failure, and overriding the configured terminal.
- Fixed a device dropping off the relay, with all its agents, once its agent list outgrew the relay size limit. Rows of Claude workflow agents stopped mid-run are now removed when the parent turn ends, long details are shortened, and devices raise the limit from 112 KiB to about 1 MiB once every peer on the relay runs this version [#172](https://github.com/aannoo/hcom/issues/172).
- Fixed a Claude subagent with no hcom record running `hcom` commands as its parent.

## 0.7.30 — 2026-10-15

### Added

- Added Hermes Agent, `hcom hermes`: messaging, status, resume, and transcripts.
- Added Hermes gateway chats as hcom agents: after `hcom hooks add hermes`, run `/hcom start` in a chat (Telegram, Discord, IRC, ...) to join and `/hcom stop` to leave.
- Added token usage to `hcom list <name>` and `hcom transcript --detailed`, and context usage for the selected agent in the TUI.

### Changed

- Changed `hcom transcript search` to match exchanges instead of raw transcript lines, with the command to open each hit.
- Made transcript summaries show what the agent concluded instead of its opening line, with full file paths.

### Fixed

- Fixed OpenCode 2 agents not receiving messages after about an hour idle [#161](https://github.com/aannoo/hcom/pull/161) — @pbarbuda.
- Fixed transcript search for OpenCode and Kilo.
- Fixed a stopped agent's old process interfering with the agent after `hcom r`.
- Fixed `--on-hit` subscriptions reacting to their own messages.
- Fixed `hcom update` upgrading the wrong pip or uv install.
- Fixed Codex counting as ready while a startup or update screen was open, and leaving `blocked` before its approval dialog closed.
- Fixed Claude native delivery errors after the working directory was deleted or the computer woke from sleep.
- Fixed Kitty launches into a window that no longer exists.
- Updated dependencies, including security fixes in Rustls.

## 0.7.29 — 2026-10-12

### Breaking changes

- Made relay settings read-only in `hcom config`. Use `hcom relay new`, `connect`, `on`, or `off` instead.
- Changed `hcom config <key> ""` to set an empty value instead of resetting the key. Reset with `--unset`, which now works for every non-relay key.

### Added

- Added experimental native delivery for Claude (`claude_delivery native`): a bundled Claude mod submits messages when the agent is idle, instead of hcom typing into the terminal.
- Added native delivery for Copilot (`copilot_delivery native`): a Copilot CLI extension sends messages to the session when the agent is idle, instead of hcom typing into the terminal.
- Added exit codes and last output for stopped agents in `hcom list --stopped`; counted early crashes as failed launches.
- Added a launch warning when agents would run a different `hcom` from PATH than yours.

### Changed

- Delivered messages sent mid-turn to Pi, Oh My Pi, and OpenCode 2 on the model's next request instead of after the turn.
- Made invalid config values fall back to their own defaults instead of ignoring the whole config; reported the bad setting during launch and in `hcom config` and `hcom status`.
- Raised the readiness wait for an agent launched from inside another to 15 seconds.
- Started the relay whenever enabled, including on devices without local agents, so those devices accept remote launches.
- Made `hcom relay new`, `connect`, and `on` wait for the broker and report whether it connected; listed each peer with its agents in `hcom relay`.
- Turned off auto-approval of hcom commands for OpenCode 1 and Kilo on Windows.

### Fixed

- Fixed lost messages in Pi, Oh My Pi, OpenCode, and Kilo when hcom marked them read before the agent accepted them.
- Prevented subagents in OpenCode, Kilo, and Oh My Pi from taking their parent's messages or changing its status.
- Fixed OpenCode 2 transcripts, search, resume, and auto-approval. Kept auto-approval from overriding your own `bash` permission setting on OpenCode 1 and Kilo.
- Marked OpenCode 1 and Kilo agents as `blocked` while asking you a question.
- Fixed Claude `-p` sessions and subagents ending after eight messages in a row and losing the next one.
- Fixed Claude hooks on Windows without Git Bash.
- Kept agents from appearing idle mid-turn during `hcom listen`.
- Fixed Windows mouse clicks in OpenCode and Pi, broken in 0.7.28 [#152](https://github.com/aannoo/hcom/issues/152); fixed delivery freezing when the console was paused and `hcom run` scripts picking WSL's `bash`.
- Fixed Herdr panes stuck on their first status: Herdr now detects the agent and its state itself, and hcom only sets the pane label [#156](https://github.com/aannoo/hcom/issues/156).
- Fixed `kitty-window` launches and launch hangs in foreground terminals (`wezterm start`, Alacritty, XTerm). Kept `hcom kill` from closing another WezTerm or Herdr session's pane; suggested `--headless` on Linux without a display.
- Applied `omp_args` and `kimi_args`; fixed `uv tool` installs writing `uvx hcom` into hooks.
- Fixed relay routing: `@name:DEVICE` also reached a local `name`, and bare `@name` reached every device's `name`. Removed relay credentials from launched agents' environments.
- Fixed the relay dropping out-of-order remote commands, redelivering old messages after a failed `relay new`, staying at `starting` on bad passwords, and opening remote launches in the wrong terminal.

## 0.7.28 — 2026-10-06

### Added

- Added Qoder CLI, `hcom qoder` (alias `qodercli`): messaging, status, resume, fork, transcripts, and workspace auto-trust [#150](https://github.com/aannoo/hcom/pull/150) — @HappyWalkers.
- Bundled Windows ConPTY to fix garbled OpenCode output and lost terminal-probe replies. Use `HCOM_CONPTY=system` for the system version [#119](https://github.com/aannoo/hcom/issues/119), [#152](https://github.com/aannoo/hcom/issues/152).

### Changed

- Changed `HCOM_DIR` to isolate hcom state while retaining tool logins and settings. Explicit tool overrides still apply.
- Split the largest pane in the launcher's WezTerm tab.
- Added the log path to every `hcom status` output.
- Reduced database writes on status updates; pruned orphaned PTY endpoints.
- Moved script-authoring docs from `skills/` to `hcom run docs --scripts`.

### Fixed

- Fixed Windows Ctrl+Z freezes and dropped control keys; enabled Shift+Enter newlines.
- Fixed Windows pane-close kill results; kept tracking after denied kills.
- Fixed PowerShell hook cleanup and updates [#152](https://github.com/aannoo/hcom/issues/152).
- Fixed crashed-agent resume, queued messages, duplicate live sessions, and tool errors [#153](https://github.com/aannoo/hcom/issues/153), [#154](https://github.com/aannoo/hcom/issues/154).
- Fixed Codex turn-end `listening` status, false `blocked` status during approval review, and Windows hook home-directory resolution [#151](https://github.com/aannoo/hcom/issues/151).
- Updated Antigravity prompt and approval-dialog detection.
- Kept Codex, Cursor, and Antigravity agents `active` until approved turns finish; deferred message injection until then.
- Restored transcript saving for Claude agents launched from inside Claude, fixing resume, fork, and identity lookup. Preserved each session's identity in the shared `/background` daemon; reported print-mode readiness after setup.
- Fixed Copilot status and identity across subagents, first turns, and `/clear`.
- Fixed message delivery after Claude `/clear` and `/resume`.
- Fixed caller identity and stopped-agent transcripts for `bundle prepare`.
- Validated workflow options and reported launch errors. Cleaned up failed and interrupted launches in `confess` and `debate`; isolated tags for concurrent runs of these and `fatcow`.

## 0.7.27 — 2026-10-01

### Security

- Restricted Copilot auto-approval to single safe hcom commands. With `auto_approve` enabled, versions 0.7.21–0.7.26 also approved chained commands.

### Breaking changes

- Removed `codex_sandbox_mode` and `HCOM_CODEX_SANDBOX_MODE`; ignored `[launch.codex] sandbox_mode`. Configure sandbox and approvals in Codex itself.
- Made plain Claude and Codex sessions using `hcom start` ad-hoc participants. Use `hcom listen` to receive messages or `hcom <tool>` for automatic delivery.

### Added

- Added Grok Build, `hcom grok` (alias `grok-build`): messaging, status, resume, fork, and transcripts [#81](https://github.com/aannoo/hcom/pull/81) — @kiala9.
- Added `hcom run onidle` to type into an agent's composer or launch an agent when another becomes idle.
- Warned when `--name` conflicts with shell identity; suggested `hcom start --as` [#140](https://github.com/aannoo/hcom/issues/140), [#148](https://github.com/aannoo/hcom/pull/148) — @gzur.
- Added `--headless` to supported resume and fork commands.

### Changed

- Moved Claude, Codex, Copilot, Pi, Oh My Pi, OpenCode, and Kilo hooks to each hcom launch. Removed legacy global hcom hooks and Codex's hook-trust bypass; kept other hooks. Cursor, Gemini, Kimi, and Antigravity retain persistent hooks [#146](https://github.com/aannoo/hcom/pull/146).
- Added agent-name and transcript-range error suggestions. Rejected lone bare words in `send` and unknown `config` keys.
- Fixed help routing; searched common install directories after PATH for tools.
- Reported OpenCode and Kilo tool calls; added launcher identity to instructions and removed the obsolete identity marker.

### Fixed

- Retired dead processes; verified PID ownership before stop or kill [#44](https://github.com/aannoo/hcom/pull/44) — @Solar2004.
- Kept messages beyond the 50-message limit or truncated output unread; reported the rest. Restricted inline `send` delivery to ad-hoc agents [#145](https://github.com/aannoo/hcom/pull/145), [#149](https://github.com/aannoo/hcom/issues/149) — @alepar.
- Fixed relay event recovery, payload limits, upgrade restarts, and IPv4-only network probes [#144](https://github.com/aannoo/hcom/pull/144) — @tbelajonas71.
- Reclaimed names when rejoining stopped PTYs [#139](https://github.com/aannoo/hcom/pull/139) — @gzur.
- Fixed Codex launches in read-only sandboxes and Windows; kept resume/fork settings, including reasoning effort. Fixed approval/interrupt status and delivery during active turns [#147](https://github.com/aannoo/hcom/issues/147).
- Restored readiness in additional Claude permission modes and OpenCode/Pi resume delivery.
- Kept Copilot identity when ending another session; separated nested-tool identities.
- Handled Unix write backpressure; added failed-resume diagnostics and kept kill reasons.
- Kept collision notifications responsive as history grows.
- Distinguished Codex placeholders from typed input in diagnostics [#110](https://github.com/aannoo/hcom/pull/110) — @qianfengzongheng.

## 0.7.26 — 2026-09-26

### Breaking changes

- Removed Codex's obsolete `untrusted` sandbox mode; normalized existing settings to `workspace`.

### Added

- Added `send --json` with `event_id` and `delivered_to` for programmatic callers [#107](https://github.com/aannoo/hcom/pull/107) — @BigCatMellow.
- Added event and reply IDs, intent, and thread to `listen --json` [#118](https://github.com/aannoo/hcom/issues/118).
- Added Ptyxis terminal preset and detection [#113](https://github.com/aannoo/hcom/pull/113) — @pzxy.

### Changed

- Supported OpenCode 2 plugins and server-based forks. Select agents and models with `HCOM_OPENCODE_AGENT` and `HCOM_OPENCODE_MODEL`; rejected malformed launch arguments [#143](https://github.com/aannoo/hcom/pull/143) — @arash16.
- Acknowledged Kimi messages only after delivery [#106](https://github.com/aannoo/hcom/pull/106) — @mmkzer0.
- Added Codex 0.157 startup waits and Ultra-effort prompt detection.
- Moved hcom's Codex hook-trust metadata out of `config.toml` into `hcom-hook-trust.toml`.
- Honored `CODEX_HOME` and kept the selected Node runtime's PATH precedence [#115](https://github.com/aannoo/hcom/pull/115), [#116](https://github.com/aannoo/hcom/issues/116), [#117](https://github.com/aannoo/hcom/pull/117) — @yasinozmeen and @markov-root.
- Avoided nested Windows PowerShell runners [#137](https://github.com/aannoo/hcom/pull/137).
- Reconnected the TUI after database replacement.

### Fixed

- Kept ad-hoc identities and idle timeouts during quiet `listen` polls [#118](https://github.com/aannoo/hcom/issues/118), [#132](https://github.com/aannoo/hcom/issues/132).
- Kept Pi identity across `/reload`, `/new`, `/resume`, and `/fork` [#141](https://github.com/aannoo/hcom/issues/141).
- Fixed Codex fork hook binding and inherited history.
- Fixed Copilot composer detection across terminal layouts.
- Rejected ambiguous transcript prefixes and reported unavailable history [#123](https://github.com/aannoo/hcom/pull/123), [#128](https://github.com/aannoo/hcom/issues/128), [#129](https://github.com/aannoo/hcom/pull/129) — @swaynehales and @marcelokarval.
- Fixed remote reply IDs and filtered waits with unrelated unread messages [#124](https://github.com/aannoo/hcom/issues/124), [#125](https://github.com/aannoo/hcom/pull/125), [#130](https://github.com/aannoo/hcom/issues/130), [#131](https://github.com/aannoo/hcom/pull/131) — @marcelokarval.
- Released hcom's database handle before Windows reset or archive [#126](https://github.com/aannoo/hcom/issues/126), [#127](https://github.com/aannoo/hcom/pull/127) — @marcelokarval.
- Kept live processes on cleanup and `killed` reasons after kills.
- Bound identities from native sessions and processes, excluding shell-output and transcript markers [#112](https://github.com/aannoo/hcom/issues/112), [#142](https://github.com/aannoo/hcom/issues/142).

## 0.7.25 — 2026-08-09

### Security

- Restricted Codex hook-trust fallback for unrelated project and plugin hooks. Versions 0.7.17–0.7.24 could bypass their trust checks if hcom trust setup failed. See [GHSA-pwv3-8r7h-p373](https://github.com/aannoo/hcom/security/advisories/GHSA-pwv3-8r7h-p373).

### Changed

- Removed confirmation from `hcom update`; `--check` only checks.
- Added PowerShell `-NoProfile` to Windows launches; preferred `pwsh` for updates.

### Fixed

- Restored Herdr 0.7.5 send, prompt, and focus commands [#102](https://github.com/aannoo/hcom/issues/102), [#103](https://github.com/aannoo/hcom/pull/103).
- Kept plain Claude identity across repeated `hcom start` calls [#105](https://github.com/aannoo/hcom/pull/105) — @alexandregressier.
- Fetched remote transcripts via relay.
- Fixed WezTerm TUI deadlocks on filtering and resize.
- Excluded unrelated processes from Windows kills after parent PID reuse.
- Fixed Termux scratch-directory creation on launch.

## 0.7.24 — 2026-07-26

### Added

- Added `terminal.title_mode` / `HCOM_TITLE_MODE`: `combined` shows agent/tool titles, `label` the agent label, `off` no hcom titles.
- Added transcript details, diffs, and errors; centered search snippets on matches.
- Documented native Claude subagent messaging with `hcom start --name <agent-id>`.

### Changed

- Updated the invoked binary with `hcom update`, even with `dev_root` set.
- Added terminal failures and OpenCode/Kilo plugin permission remedies to launch errors.

### Fixed

- Fixed Claude subagent delivery after blocked launches and resume, and command identity [#82](https://github.com/aannoo/hcom/pull/82), [#99](https://github.com/aannoo/hcom/pull/99) — @zane668.
- Paused Claude delivery behind overlays and reduced empty wakes. Mitigated [#100](https://github.com/aannoo/hcom/issues/100) and [#101](https://github.com/aannoo/hcom/issues/101).
- Refreshed Codex approval titles and resolved Windows transcript paths.
- Published the Android wheel to PyPI with its correct platform tag, so `pip install hcom` works in Termux.
- Restricted POSIX state-directory and database access to the owner; avoided `SQLITE_BUSY` during concurrent opens [#90](https://github.com/aannoo/hcom/pull/90) — @zane668.
- Kept parent participation through Oh My Pi task subagents [#97](https://github.com/aannoo/hcom/pull/97) — @mmkzer0.
- Sent Antigravity instructions on each invocation.
- Fixed Codex `HCOM_NOTES` delivery, configured precedence, and explicit empty values [#84](https://github.com/aannoo/hcom/pull/84) — @zane668.
- Honored `pane_id_env`, cleared inherited pane identities, and launched Kitty without a caller TTY [#96](https://github.com/aannoo/hcom/pull/96) — @1dr0z.

### Development

- Isolated unit tests from live hcom databases, including detached relay pushes [#88](https://github.com/aannoo/hcom/pull/88), [#89](https://github.com/aannoo/hcom/pull/89) — @zane668.
- Repaired Clippy and release-workflow checks; stabilized real-tool CI [#91](https://github.com/aannoo/hcom/pull/91), [#98](https://github.com/aannoo/hcom/pull/98) — @zane668.

## 0.7.23 — 2026-07-06

### Added

- Supported Windows x86_64: ConPTY, messaging, idle wake, process-tree kills, terminal presets, PowerShell installer, and wheels [#51](https://github.com/aannoo/hcom/issues/51), [#67](https://github.com/aannoo/hcom/pull/67) — @keinstn.
- Added Oh My Pi, `hcom omp` (alias `omp-agent`): messaging, transcripts, profiles, resume, and fork [#68](https://github.com/aannoo/hcom/pull/68) — @mmkzer0.
- Added `listen --sql stopped:<name>` to wait for an agent to stop.

### Changed

- Identified Pi and Oh My Pi transcripts by tool; rejected ambiguous files.
- Suppressed duplicate Pi and Oh My Pi status events.
- Tried graceful Windows daemon and relay shutdown before forced termination.
- Kept `--run-here` support; omitted it from launch help.

### Fixed

- Prevented terminal resize panics from killing Codex sessions [#73](https://github.com/aannoo/hcom/issues/73), [#75](https://github.com/aannoo/hcom/pull/75).
- Applied `HCOM_TIMEOUT` to plain and headless sessions and existing databases [#71](https://github.com/aannoo/hcom/issues/71), [#74](https://github.com/aannoo/hcom/pull/74).
- Fixed relay replay with clock skew; removed dead peers on shutdown [#63](https://github.com/aannoo/hcom/issues/63), [#76](https://github.com/aannoo/hcom/pull/76).
- Fixed successive Pi messages and queued wakes during delivery [#66](https://github.com/aannoo/hcom/pull/66), [#69](https://github.com/aannoo/hcom/pull/69) — @philosophicalsmith-beep.
- Fixed Oh My Pi rebinding after `/branch`.
- Fixed premature Windows session exits, Codex quoting, and input injection.
- Reported locked files on Windows reset; retried config writes after antivirus locks. Released file handles on relay shutdown and archive.

### Development

- Typechecked bundled TypeScript plugins in CI [#72](https://github.com/aannoo/hcom/pull/72).

## 0.7.22 — 2026-06-22

### Breaking changes

- Required exact local agent names or a full `tag-name`; use `@tag-` for group mentions. Remote mentions still accept unambiguous prefixes.
- Passed config arguments before CLI arguments, leaving conflict resolution to each tool. Rejected flags that prevent interactive delivery.

### Added

- Added `pi_args` and `copilot_args` to `hcom config`.
- Added process state and terminal output to launch diagnostics; added recent failures to `status` and JSON `instances.launch_failures`.
- Added terminal and headless suggestions to Termux/PRoot launch errors.

### Changed

- Discovered transcripts in relocated tool directories; filtered searches require exact agent names or aliases.
- Detected Codex approvals from titles and dialogs.
- Used canonical `tools` keys in `status --json`; added `settings_path`.
- Verified rendered input before Enter.
- Kept original resume and fork arguments without accumulating injected flags.

### Fixed

- Woke event listeners promptly and replayed OpenCode wakes received during delivery [#65](https://github.com/aannoo/hcom/pull/65) — @azkore.
- Acknowledged Claude messages after flushing output.
- Restored Pi default subscriptions and Kimi status and capability reporting.
- Matched mention filters and read receipts exactly; recognized supported edit tools in collision checks.
- Prevented Termux suspension during login-shell environment capture.
- Enforced tool launch limits; reported malformed arguments and unsupported Kimi initial prompts.

## 0.7.21 — 2026-06-06

### Breaking changes

- Changed Claude `--headless` to a live PTY by default; made `--pty` a no-op. Select print mode explicitly with `-p` / `--print` and a prompt.

### Added

- Added GitHub Copilot CLI, `hcom copilot`: hooks, messaging, transcripts, and resume.
- Added Kimi Code, `hcom kimi`: hooks, messaging, transcripts, resume, config, and approval tracking [#59](https://github.com/aannoo/hcom/pull/59) — @ekhodzitsky.
- Added Pi Coding Agent, `hcom pi` (alias `pi-agent`): extension delivery, transcripts, resume, and fork.
- Added Gemini, Codex, Copilot, and Cursor workspace auto-trust via `auto_trust_workspace` / `HCOM_AUTO_TRUST_WORKSPACE`.
- Added agent details via `hcom list <name>` and TUI resume.
- Added previews for agent broadcasts to over three recipients; `--go` authorizes sending.

### Changed

- Used the user's login-shell environment for nested and CI launches, retaining authentication and settings.
- Added `--hcom-prompt` across tools and inline readiness for agent-launched resume and fork.

### Fixed

- Restored forks and cleaned up placeholder records.
- Kept delivery and kill tracking on resume under a new name [#62](https://github.com/aannoo/hcom/pull/62) — @mmkzer0.
- Kept OpenCode model variants during delivery [#60](https://github.com/aannoo/hcom/issues/60), [#61](https://github.com/aannoo/hcom/pull/61) — @azkore.
- Fixed addressing remote agents by TUI names [#63](https://github.com/aannoo/hcom/issues/63).
- Fixed external terminals for TUI resume/fork and exit/kill outcome reporting.
- Applied configured OpenCode, Kilo, and Kimi arguments; restored Copilot and Pi session recognition.
- Parsed Gemini JSONL transcripts and recovered misplaced `--name` flags after `--` with a warning.

## 0.7.20 — 2026-06-01

- Added Cursor Agent (`hcom cursor`, alias `cursor-agent`): hooks, messaging, approval tracking, transcripts, and resume [#58](https://github.com/aannoo/hcom/pull/58).
- Added Kilo (`hcom kilo`, alias `kilocode`): messaging, transcripts, resume, and fork [#45](https://github.com/aannoo/hcom/pull/45) — @Solar2004.
- Required Antigravity to complete requests after acknowledgment; allowed help and discovery during delivery [#57](https://github.com/aannoo/hcom/issues/57).
- Applied OpenCode and Kilo auto-approval only when `auto_approve` is enabled, and only to safe hcom commands. OpenCode launches previously approved every `hcom` command, including `kill` and `reset`.
- Included message bodies in OpenCode's first delivery turn.
- Kept history when opening older databases.

## 0.7.19 — 2026-05-27

### Added

- Added Antigravity, `hcom antigravity` (alias `agy`): hooks, messaging, status, transcripts, and resume [#55](https://github.com/aannoo/hcom/pull/55) — @mmkzer0.
- Added a Herdr preset with pane labels and close-on-kill.
- Added terminal placeholders `{instance_name}`, `{tool}`, `{pane_title}`, and `{cwd}`.
- Added `launch_blocked` for trust and permission prompts and `ready` with context `launch_blocked_cleared` on recovery.
- Reported readiness inline for single-agent AI launches. `events launch` exits 0 for ready, 1 for failure or no launches, and 2 for timeout or blocked.
- Added Codex `--yolo` passthrough [#54](https://github.com/aannoo/hcom/pull/54) — @RichelynScott.

### Changed

- Required a usable screen for readiness; distinguished ready, failed, and blocked batch launches.
- Waited for safe input before delivery and paused after prompt submission.
- Used Antigravity's native permission settings; cleaned up only hcom entries.

### Fixed

- Restored Antigravity delivery, approval status, workspace lookup, and inactive resume.
- Fixed Herdr kills to target stable pane IDs.
- Made sandboxed Codex launches fail promptly with recovery advice.
- Ignored pending placeholders and late stop events when reporting batch failures.

## 0.7.18 — 2026-05-23

### Breaking changes

- Dropped non-launch options, including Claude `--init` / `--maintenance` and Codex `--json` / `--output-schema`. Run Codex utilities (`exec`, `review`, `mcp`) directly. Interactive launch, resume, and fork remain supported.

### Changed

- Drew names from a larger pool to reduce similarity.
- Allocated stable, unique relay device IDs.
- Reserved resume and fork names at launch; released abandoned reservations and kept live records.

### Fixed

- Applied default subscriptions after resume and fork.
- Verified Codex hooks alongside other tools' empty-matcher hooks [#53](https://github.com/aannoo/hcom/pull/53) — @mmkzer0.
- Parsed Gemini `--session-file` values.

## 0.7.17 — 2026-05-19

### Breaking changes

- Required Rust 1.88+ for source builds.

### Changed

- Supported Codex 0.131 hook trust: repaired hcom trust, warned on failure, and allowed a wrapped-launch bypass as fallback. Version 0.7.25 restricts this fallback.
- Removed deprecated Codex hook keys from all profiles; aligned status and verification with launches.
- Accepted current Claude, Codex, and Gemini launch flags.

### Fixed

- Prevented duplicate relay workers; backed off after failed connections.
- Excluded imported events and internal device records from relay queue counts.

## 0.7.16 — 2026-05-09

- Repaired Codex hook settings that passed verification but disabled hooks on Codex 0.129+.

## 0.7.15 — 2026-05-09

- Added macOS Warp support and pane closure on stop or kill for Zellij and Wave Term.
- Selected Codex's hooks feature key by CLI version [#43](https://github.com/aannoo/hcom/pull/43) — @mmkzer0.
- Rejected `HCOM_DIR` beneath sandbox-protected `.git`, `.codex`, `.claude`, or `.agents` directories.
- Woke message and event listeners promptly on relay arrivals.
- Suggested `@name:DEVICE` when a bare mention matches a remote agent.
- Restored collision notifications for edits 20–30 seconds apart.
- Excluded terminal-injection connections from wake notifications.
- Kept Codex's `[features].codex_hooks` on uninstall; skipped pane-close checks when the terminal lacks a close API.

## 0.7.14 — 2026-05-01

- Supported Codex 0.128+ after removal of `--full-auto`. User sandbox and approval flags take precedence; normalized legacy `full-auto` settings to `workspace` [#34](https://github.com/aannoo/hcom/pull/34) — @khoant94.
- Recognized Gemini's changed input-box layout.
- Allowed edited hook timeouts; verified installation and reported failed steps and settings paths [#32](https://github.com/aannoo/hcom/issues/32).
- Validated Codex sandbox settings.
- Gave UUID-based subagents correct identity-recovery advice [#29](https://github.com/aannoo/hcom/issues/29), [#30](https://github.com/aannoo/hcom/pull/30) — @toller892.
- Suggested `--from` for unregistered shell and cron senders [#33](https://github.com/aannoo/hcom/issues/33).
- Fixed event wakeups, unstable new relay IDs, and agent-list panics on multibyte text.

## 0.7.13 — 2026-04-24

### Added

- Added persistent background Claude sessions with messaging and terminal injection via `hcom claude --headless --pty` [#28](https://github.com/aannoo/hcom/pull/28).
- Added addressing for Claude subagents from spawn; direct mentions wake them, broadcasts do not.

### Changed

- Required a prompt for Claude `--headless` without `--pty`; used print mode [#27](https://github.com/aannoo/hcom/pull/27), [#28](https://github.com/aannoo/hcom/pull/28).
- Rejected `start --as` identities from another tool or directory [#25](https://github.com/aannoo/hcom/pull/25) — @bloodcarter.
- Merged shipped skills into `hcom-agent-messaging`.

### Fixed

- Fixed macOS background launches exiting or busy-looping on stdin EOF [#26](https://github.com/aannoo/hcom/pull/26) — @mmkzer0.
- Fixed duplicate OpenCode delivery, session status, and agent and model preservation [#23](https://github.com/aannoo/hcom/pull/23), [#24](https://github.com/aannoo/hcom/pull/24) — @mmkzer0.
- Kept Claude parent identities during subagent cleanup.
- Aligned hook status across commands and reduced macOS injection waits.

## 0.7.12 — 2026-04-18

- Resumed or forked sessions by UUID, OpenCode `ses_` ID, Claude `/rename`, or Codex thread name, including sessions created outside hcom. Showed ambiguous candidates and rejected running sessions [#22](https://github.com/aannoo/hcom/pull/22) — @bloodcarter.
- Added `--remote-fetch --device` for remote events and `events sub --device` / `events unsub --device` to manage peer subscriptions.
- Sent messages on subscription matches with `events sub --on-hit`.
- Printed plain terminal screens with `hcom term --clean`.
- Unified relay health reporting; retained sync progress when disabled.
- Fixed adoption exit codes, previews, and resume/fork flags.

## 0.7.11 — 2026-04-14

### Breaking changes

- Added an encryption key and new relay-token format; rejected old tokens. Run `hcom relay new` on one upgraded device, then reconnect the others with its new token.
- Replaced remote `stop` with `kill <name>:<DEVICE>`; rejected remote `start` and Gemini fork.

### Added

- Added remote launch, kill, resume, fork, config, terminal viewing/injection, and transcripts via relay.
- Encrypted relay end to end; added replay protection. Remote config excludes credentials; launches receive the join token and group key.
- Added `relay token`, `relay off --all`, and device join, leave, and reconnect events.
- Added `--dir` for launch, resume, and fork directories; required it for remote launch and fork.
- Added Codex lifecycle, status, and mid-turn delivery hooks.

### Fixed

- Installed hooks in custom tool config directories (`CLAUDE_CONFIG_DIR`, `GEMINI_CLI_HOME`, `CODEX_HOME`) [#19](https://github.com/aannoo/hcom/issues/19).
- Reported remote failures promptly; kept relay workers running without local agents and sped up outage recovery.
- Kept resume and fork options and identity without replaying old broadcasts.
- Deduplicated Codex status events and ignored stale Claude input boxes.
- Fixed orphaned hooks exiting after uninstall; kept other settings during cleanup.
- Fixed `bundle prepare` reading stopped snapshots [#20](https://github.com/aannoo/hcom/pull/20) — @bloodcarter.

## 0.7.10 — 2026-04-08

### Changed

- Updated `--hcom-prompt` to request results for the launcher.
- Required glibc 2.28+ for ARM64 Linux wheels; x86_64 still requires 2.17. musl wheels are unchanged.
- Showed the resolved terminal in status.

### Fixed

- Created the state directory on first use.
- Read `dev_root` even when the database is busy.

## 0.7.9 — 2026-04-05

- Added thread membership: explicitly addressed sends add recipients and the sending agent; later sends in that thread without explicit targets go to its members.
- Added persistent `hcom config dev_root` to select the latest Cargo build; removed `build.sh`.
- Resolved Kitty sockets and showed effective terminal presets in launch tips.
- Shipped `.tar.gz` archives and recognized Debian pip installations.
- Fixed Kitty pane and tab closure, late launch metadata, script help, and config paths.
- Fixed `fatcow` startup; cleaned up interrupted `confess`/`debate` agents.

## 0.7.8 — 2026-04-02

- Added Homebrew (`brew install aannoo/hcom/hcom`) and release shell installers.
- Shipped messaging and workflow skills in the Claude Code plugin [#17](https://github.com/aannoo/hcom/pull/17) — @yigitkonur.
- Labeled tool-only turns, normalized tool names, and removed duplicate or injected exchanges in Codex, Gemini, and OpenCode transcripts.
- Refreshed stale OpenCode plugins; recognized wrapped Gemini hooks without repeated reinstalls.
- Restored intent tips and reported unsupported Gemini hook versions.

## 0.7.7 — 2026-03-25

- Fixed Android/Termux relay TLS with empty native certificate stores.
- Repaired missing database columns without losing history; restored Claude startup context [#16](https://github.com/aannoo/hcom/issues/16).
- Replaced development binaries atomically to protect running PTY sessions.

## 0.7.6 — 2026-03-20

- Added `hcom update`: `--check` checks; `--go` installs unattended.
- Migrated the v16 database schema to v17 in place, preserving history; archived unsupported schemas.
- Recorded effective terminal presets and honored custom close commands on stop or kill [#13](https://github.com/aannoo/hcom/issues/13).
- Fixed first-use `start`, symlinked dev-build detection, and preset-column validation.

## 0.7.5 — 2026-03-15

- Validated event filter values and date formats before querying.
- Stabilized relay startup and accepted lowercase terminal presets.
- Restored visible TUI launches; added `--no-run-here` [#11](https://github.com/aannoo/hcom/issues/11).
- Resolved names with multi-part tags [#10](https://github.com/aannoo/hcom/pull/10) — @bloodcarter.
- Fixed Termux Codex wrappers and event-date parsing.

## 0.7.4 — 2026-03-11

- Reported launch failures promptly with terminal errors and recovery advice.
- Validated and deduplicated subscriptions while keeping one-shot subscriptions distinct.
- Kept macOS file-opening commands and prevented early config-read panics.

## 0.7.3 — 2026-03-08

- Added cmux, Zellij, and Wave Term presets; honored custom launch presets.
- Resumed in the recorded directory unless `--dir` overrides it [#7](https://github.com/aannoo/hcom/pull/7) — @bloodcarter.
- Included OpenCode in all-transcript queries and tracked approvals consistently.
- Cleared stale blocked status after approval prompts disappear [#6](https://github.com/aannoo/hcom/pull/6) — @bloodcarter.
- Fixed Kitty sockets, hook feedback, tagged names, and stale transcript bindings.
- Made repeated hook installation and removal report when hooks were already installed or removed.

## 0.7.2 — 2026-03-05

- Fixed Termux launches: resolved tools through `PATH` and ran Node-based tools with `node`.
- Raised the relay packet limit to 128 KiB.
- Ignored stale Codex approval markers in earlier PTY output.
- Used "agent" consistently in errors and listings; `hcom list` explains how to launch when none are active.

## 0.7.1 — 2026-03-02

- Added Android/Termux release binaries and shell-installer platform detection.

## 0.7.0 — 2026-03-02

### Breaking changes

- Replaced Python with a native Rust binary. `pip install hcom` still works; replace Python API scripts with CLI calls.
- Renamed `hcom daemon` to `hcom relay daemon`; removed `hcom run docs --api`.
- Kept `confess`, `debate`, `fatcow`, and user scripts; archived `clone`, `ensemble`, `glue`, and `watcher`.
- Dropped declared Windows support; native packages return in 0.7.23.
- Ignored `HCOM_RELAY`, `HCOM_RELAY_ID`, `HCOM_RELAY_TOKEN`, and `HCOM_RELAY_ENABLED` environment overrides; set relay only through `[relay]` in `config.toml` or `hcom relay`.

### Added

- Added a shell installer (`install.sh`) and GitHub release binaries for macOS and Linux; checked for updates daily.
- Added launch flags `--hcom-prompt`, `--hcom-system-prompt`, and `--batch-id`.
- Added support for killing several agents in one `hcom kill` call.
- Added multiline TUI compose.
- Added CLI validation, command help, and post-launch tips.
- Moved Gemini permissions from `tools.allowed` to policy files.
- Kept comments when editing `config.toml`.
- Preserved the existing database and history through the Rust rewrite; the schema remained at version 16.

### Fixed

- Fixed Gemini transcripts after kill or resume, and Gemini screen flicker.
- Fixed Termux launches for all tools.

## 0.6.22 — 2026-02-22

- Prevented lost or duplicate OpenCode messages.
- Handled broken pipes without panicking.

## 0.6.21 — 2026-02-22

### Breaking changes

- Changed `list --json` from newline-delimited objects to an array.
- Archived history on schema upgrade and created a new database; archives remain queryable.
- Removed `hcom shim`; required Rust 1.86+ to build.

### Added

- Added OpenCode plugin delivery, transcripts, resume, and fork.
- Added the default Rust dashboard: search, timeline, batch actions, config, orphan recovery, and headless agents.
- Recovered live agents after reset; `start --orphan` adopts orphaned processes.
- Added launch flags `--tag` / `--terminal`; `--go` replaces `HCOM_GO`.
- Added `list --names` / `--format` for scripts.
- Expanded the file-collision notification window from 20 to 30 seconds.

### Fixed

- Kept Claude identity on resume and compaction; recorded terminals for close-on-kill.
- Restarted outdated daemons instead of replacing newer databases.
- Fixed PTY CPU usage, partial transcript lines, and hook-settings preservation.
- Launched from OpenCode sessions in `/` and ran Android binaries from executable storage.

## 0.6.20 — 2026-02-16

- Added musl Linux wheels.
- Added an Android ARM64 wheel on GitHub releases for Termux; it was not published to PyPI.
- Named both agents and the file in collision notices; listed names after batch launch.
- Passed Codex resume and fork arguments through unchanged, including `--last`.
- Listed subscriptions with `events sub list`; bare `events sub` shows help.

## 0.6.19 — 2026-02-14

### Breaking changes

- Replaced Hugging Face HTTP relay with MQTT; supported private brokers and removed `relay hf`, `pull`, and `poll`. Create groups with `relay new`; join with `relay connect <token>`.
- Replaced `config.env` with `config.toml` and an `env` file for tool variables; migrated settings and terminal presets from `settings.toml` automatically.

### Changed

- Ran relay outside hooks in a background worker; reloaded settings without restart.
- Showed remote agents, device IDs, sync times, and queue counts in status.
- Showed each config value's source.
- Allowed excluding the caller's session from transcript search.

### Fixed

- Kept output across split title escape sequences.
- Removed subscriptions when their PTY exited.

## 0.6.18 — 2026-02-11

- Accepted extra resume arguments; kept tags and queued messages until stopped agents return.
- Notified request senders when recipients became idle or stopped without replying; replies cancel the watch.
- Added `fatcow` to answer questions about files.
- Added CLI-style event filters to the Python session API.
- Kept agents after laptop sleep, saved stopped snapshots reliably, and fixed pane-close fallback.
- Cleared inherited Terminal.app identities and delivery gates after interrupted listens.

## 0.6.17 — 2026-02-08

- Forwarded `HCOM_NOTES` through nested launches and kept Codex thread identity.
- Deferred terminal injection during `hcom listen`; parsed listen timeouts consistently.
- Refreshed tagged titles and distinguished Claude and Codex placeholders from typed input.

## 0.6.16.1 — 2026-02-08

- Republished yanked 0.6.16.

## 0.6.16 — 2026-02-08

Yanked on PyPI; superseded by 0.6.16.1.

- Added startup notes via `hcom config notes` / `HCOM_NOTES` [#4](https://github.com/aannoo/hcom/issues/4).
- Added terminal close commands and Kitty remote-control setup.
- Bound Codex sessions via `CODEX_THREAD_ID` for transcripts and resume without terminal scraping.
- Aligned Codex sandbox modes with Codex presets; allowed network access for hcom's socket.
- Showed tags in names and titles and simplified startup guidance.
- Reloaded config without daemon restarts, fixed listen/injection races, and disabled auto-approval on config parse errors.

## 0.6.15 — 2026-02-06

- Fixed narrow-terminal Codex readiness and idle delivery.
- Prevented duplicate injection during `listen`; resolved tagged agents by base-name mentions.
- Fixed Claude parents stuck tracking a subagent or marked `blocked` by one.
- Started the daemon from virtualenv and uv installations.

## 0.6.14 — 2026-02-05

### Breaking changes

- Required Gemini CLI 0.26.0+ and Python 3.11+; native builds require Rust 1.85+.
- Published only macOS and Linux platform wheels; no pure-Python wheel, source distribution, or Windows package.

### Added

- Added Rust CLI and PTY proxy backed by a Python daemon, bundled in platform wheels.
- Added `hcom term` for screen viewing, input injection, and delivery diagnostics.
- Added terminal detection and managed tmux, Kitty, and WezTerm pane closure.
- Added `hcom r` for Claude, Codex, and Gemini resume and `hcom f` for Claude and Codex forks.
- Added literal messages after `--`, plus `--file`, `--base64`, and stdin input.
- Added Python API batch launches and daemon commands.

### Fixed

- Corrected the Claude Code marketplace manifest [#3](https://github.com/aannoo/hcom/issues/3).

## 0.6.13 — 2026-01-24

### Breaking changes

- Replaced `send --bundle` / `--bundle-file` with inline flags or scripted `bundle create`.

### Added

- Added `bundle prepare`, `bundle cat`, transcript detail levels, and inline bundles with `send --title`.
- Added stopped-agent lists and source execution via `HCOM_DEV_ROOT`.

### Fixed

- Found transcripts in relocated Claude and Codex homes and Gemini directories.
- Injected into current Codex and Gemini prompts instead of scrollback.
- Kept Claude delivery working with prompt suggestions; waited 3 seconds after typing instead.
- Rejected acks of `inform` messages and mentions of system senders such as `@[hcom-events]`.
- Returned from filtered `listen` waits when an agent message arrived, so agents could respond while awaiting an event.

## 0.6.12 — 2026-01-21

### Breaking changes

- Replaced event presets with filters: `events sub idle:peso` becomes `events sub --idle peso`.
- Renamed `HCOM_DEFAULT_SUBSCRIPTIONS` to `HCOM_AUTO_SUBSCRIBE`.
- Archived history on schema upgrade and created a new database; archives remain queryable.

### Added

- Added bundles with event, file, transcript, and ancestry references.
- Shared query, subscription, and listen filters: repeated flags OR together; different flags AND together.
- Added `hcom transcript search` across tracked sessions.
- Targeted collision notices to the agents involved.

## 0.6.11 — 2026-01-18

### Breaking changes

- Replaced `--agentid` with global `--name`.
- Renamed `idle` to `listening`; added `launching` and `error` states.
- Archived history on schema upgrade and created a new database; archives remain queryable.
- Replaced the Python API's standalone messaging and query functions with identity-bound sessions via `hcom.session()`.

### Added

- Added Gemini and Codex launches, messaging, status, and transcripts alongside Claude.
- Added `hcom listen`, subscription presets, transcript timeline, diagnostics, and setting help.
- Added bundled workflows and user scripts in `~/.hcom/scripts/`.
- Added `hcom kill`, `hcom status`, `hcom hooks`, optional PATH shims (`hcom shim`), and participation from sessions launched outside hcom.
- Added configurable auto-approval.

### Changed

- Standardized envelopes and delivery feedback across tools.
- Consolidated structured logging and removed stopped agents from the live roster.

## 0.6.10 — 2025-12-20

- Prevented self-restart while an external stop is pending; identified the stopper.
- Added Claude Code marketplace installation and `/hcom`.
- Added Python API transcript ranges and system prompts; replace `launch(resume=, fork=)` with tool arguments.
- Showed tagged names in listings, messages, and read receipts; clarified sends without active recipients.
- Ran `reset`, `--help`, and `--version` without hook sync or subagent identity checks.

## 0.6.9 — 2025-12-15

### Breaking changes

- Renamed `hcom thread` to `hcom transcript`.
- Routed through broadcasts and mentions; removed parent-broadcast and subagent-group scopes.

### Added

- Added message intents, reply IDs, and threads.
- Added queryable archives; archived incompatible database schemas automatically.
- Added a public Python API, per-agent config, and separate tags and base names.
- Added the MIT license.

### Fixed

- Emitted stop notices once; distinguished self-stops from external stops.
- Reported invalid sends and unresolved identities.

## 0.6.8 — 2025-12-12

### Breaking changes

- Renamed the caller identity field in `list --json` from `_self.alias` to `_self.name`.

### Added

- Read numbered agent conversations with `hcom thread`, including full, detailed, and JSON output.
- Subscribed to SQL filters and file collisions; queried flattened `events_v` fields.
- Supported two-way messaging with background Claude Task subagents.
- Inspected caller identity and exported fields with `hcom list self`.
- Exported `HCOM_NAME` and `HCOM_SESSION_ID` to Claude sessions; added `HCOM_NAME_EXPORT` for a custom name variable.

### Fixed

- Fixed flag validation, hook errors, and interrupted-subagent cleanup.
- Excluded Claude session-memory edits from activity.

## 0.6.7 — 2025-12-06

### Breaking changes

- Removed `HCOM_AGENT`; pass native `--agent` via `HCOM_CLAUDE_ARGS` or CLI arguments.
- Launched exactly the requested count, without multiplying by configured agents.

### Fixed

- Made `relay status` pull fresh state and fail on pull errors.
- Recovered forked Claude session IDs without overwriting parents.
- Kept nested-launch config while clearing parent identity; fixed background orphan detection and multiline commands.

## 0.6.6 — 2025-12-04

### Breaking changes

- Renamed `watch` to `events` and `--_hcom_sender` to `--agentid`.
- Removed `reset logs` / `reset config`. Bare reset archives history; `reset hooks` removes hooks; `reset all` clears everything.
- Rejected unresolved identities instead of sharing a fallback sender.
- Renamed message-event fields from `recipients` to `delivered_to` and `mentioned_instances` to `mentions`.

### Added

- Added cross-device relay via private Hugging Face Spaces; address remote agents as `luna:BOXE`.
- Added remote participation controls through `hcom start` and `hcom stop`.
- Added config viewing, editing, and reset via `hcom config`.
- Added batch readiness queries and TUI launch and relay indicators.
- Added lazy Task subagent registration and external-stop notifications.

### Changed

- Kept plain mentions local; required device suffixes for remote recipients.
- Defaulted to participating agents; improved relay sync and read feedback.
- Opened Terminal.app without Accessibility permission; discovered native Claude installations.

## 0.6.5 — 2025-11-20

### Breaking changes

- Reduced statuses to `active`, `idle`, `blocked`, and `inactive`. In scripts, replace `delivered` with `active` + `deliver:` context, `waiting` with `idle`, and `exited` with `inactive` + `exit:` context.
- Required `hcom start` before plain Claude sessions send messages.
- Required `HCOM_DIR` to be an absolute path containing no `..`.

### Added

- Added TUI agent details.

### Fixed

- Kept identity across Windows and terminal session changes.
- Fixed turn-end delivery, saved wake ports, ANSI paste, CJK layout, and TUI stop and reset errors.

## 0.6.4 — 2025-11-18

### Breaking changes

- Replaced structured watch filters with `watch --sql`; removed `hcom done`.
- Archived and cleared the database on first run if it held messages without the new scope field.
- Stopped subagents individually; required new subagents to opt in.

### Added

- Added explicit message scopes: external broadcasts reach all participants, parent broadcasts reach other parents, and subagent broadcasts stay within their group. Mentions can cross groups; unmatched mentions fail instead of falling back to broadcast.
- Added `send --wait` so external senders, including those using `--from <name>`, can wait for replies.
- Added SQL event filters, batch tracking, and separate appended system prompts.
- Added TUI event filters and reader checkmarks on messages.

### Fixed

- Kept undelivered messages unread after stop hooks become orphaned.

## 0.6.3 — 2025-11-11

- Added read receipts, recipient previews, caller identity, and launch, start, and stop events.
- Changed `list --json` to include a leading `_self` record with caller identity and read receipts; replaced `status_age` with numeric `status_age_seconds`, and added `headless` and `wait_timeout`.
- Kept identity through terminal IDs and launch tokens on Windows, tmux, Kitty, and WezTerm.
- Unified start, stop, and send identity lookup and improved UTF-8 output.
- Removed `send_cli`; use `send` for external callers.

## 0.6.2 — 2025-11-09

### Breaking changes

- Replaced instance JSON and message logs with SQLite at `~/.hcom/hcom.db`, without importing old records. Update scripts that read the old files.
- Changed `watch` from the dashboard to a JSON event query; open the dashboard with bare `hcom` or `hcom --new-terminal`.

### Added

- Added `hcom list`, external sender names via `send --from`, and `hcom --new-terminal`.
- Added state-directory override via `HCOM_DIR`.

### Fixed

- Delivered messages after any tool, not only Bash and Task.
- Detected broken hook matchers, fixed stop-hook crashes, and handled narrow terminals.
- Allowed sends with no registered agents.

## 0.6.1 — 2025-11-02

- Added Task subagent `hcom done` and merged Claude arguments flag by flag.
- Backed up and reset configs containing `HCOM_PROMPT`; restore custom settings from the backup and put launch arguments in `HCOM_CLAUDE_ARGS`.
- Kept messages sent while a launched agent starts up.
- Fixed Termux shell discovery, multiline commands, and shell-escaped messages.
- Clarified start and stop feedback and fixed TUI sizing.

## 0.6.0 — 2025-10-30

### Breaking changes

- Changed bare `hcom` to open a tabbed dashboard instead of help.
- Moved prompts from `HCOM_PROMPT` to `HCOM_CLAUDE_ARGS`; removed `stop --force`.
- Removed `last_status`, `last_status_time`, and `last_status_context` from `watch --status` JSON; use `status`, `description`, and the new `enabled` field. Update dependent scripts.

### Added

- Added manage, launch, and log tabs with inline config editing.
- Added TCP wakes for idle delivery, with polling fallback.
- Added Task subagent identities and group messaging; `HCOM_SUBAGENT_TIMEOUT` sets their idle wait.
- Added cross-platform file locks for state and log writes.

### Changed

- Gave shell environment variables precedence over `config.env` in launched agents.
- Fell back to default config in hooks when `config.env` is invalid, so plain Claude sessions keep working.
- Auto-approved chained commands only when every part is an hcom command.

### Fixed

- Fixed launch controls, config sync, log truncation, and orphaned subagent cleanup.

## 0.5.0 — 2025-10-17

### Breaking changes

- Replaced `hcom open` with `hcom <count> [claude <args>]`; select agents with `HCOM_AGENT` and tags with `HCOM_TAG`.
- Replaced `config.json` with `config.env`; re-enter old settings manually.
- Renamed terminal modes: `new_window` → `new`, `same_terminal` → `here`, `show_commands` → `print`.
- Replaced `stop everything` and `stop hooking` with `reset [logs|hooks|config]`; `stop all` no longer archives the log.

### Added

- Added word-based names, tags, update notices, and `watch --launch` to open the dashboard in a new terminal.
- Added startup context after Bash calls.
- Reinstalled hooks automatically when they are missing or point at an old hcom path.

### Changed

- Skipped old history for new agents.
- Reduced config to six environment settings; fixed the human sender name as `bigboss`.
- Stored scratch files in `~/.hcom/.tmp/`.
- Installed hooks in global `~/.claude/settings.json` instead of project `.claude/settings.local.json`. Sessions opt in via `start` or hcom launch.

PyPI also lists yanked `0.4.2.post3`, a misnumbered build of this overhaul.

## 0.4.0 — 2025-10-11

### Breaking changes

- Changed launches to `hcom open [count] [-a agent]... [-t prefix] [-p] [-- claude-args]`; agents and team prefixes are now flags.
- Replaced `clear`, `cleanup`, and `kill` with `stop all`, `stop hooking [--all]`, and `stop everything`. `stop` leaves the Claude process running; later releases restore process management.

### Added

- Added `hcom start` / `stop` to join and leave chat, including from plain Claude sessions.
- Added `stop --force`, which denies Bash until `hcom start`.
- Added a SessionEnd hook that records why a session ended.

### Changed

- Attributed `hcom` commands to the calling session and kept names on resume.
- Asked unmatched resumes to recover their identity.
- Renamed the Stop hook's command to `poll`, leaving `stop` to users.

## 0.3.1 — 2025-10-06

- Reduced heartbeat writes; coordinated prompts and stops with timestamps.
- Described killed, timed-out, and unknown states.
- Required the UserPromptSubmit hook when verifying installs.
- Accepted human-sender mentions without warnings.

## 0.3.0 — 2025-10-05

### Breaking changes

- Required Python 3.10+.
- Removed PostToolUse delivery to avoid Claude API errors; delivered through Stop hooks or after sends.
- Replaced `HCOM_SEND:` echoes with direct `hcom send` calls.
- Stored one session ID per agent and dropped PID-based names; 0.2.0 instance files are no longer migrated.

### Changed

- Tracked status through hooks instead of transcripts; replaced thinking, responding, and executing with active and delivered.
- Added a UserPromptSubmit hook, `send --resume <alias>` recovery, and hook logs.
- Prevented orphan aliases on unmatched resumes; reported unknown status without activity.

## 0.2.3 — 2025-09-28

- Allowed only lowercase letters and hyphens in agent-file names; rejected agent files that resolve outside the agents directory.
- Sanitized instance filenames to prevent alias-based path traversal.

## 0.2.2 — 2025-09-27

- Added Android/Termux launches and setup docs.
- Scripted every launch mode to fix quoting, spaced paths, and custom-terminal injection.
- Kept Node and Claude paths in minimal environments.
- Reported partial launch success, missing agents, and unsupported platforms.

## 0.2.1 — 2025-09-25

### Breaking changes

- Required Python 3.7+.
- Changed `watch --status` to return JSON.
- Replaced terminal placeholders `{cmd}`, `{env}`, and `{cwd}` with `{script}`.

### Added

- Added session-start reminders and resume alias recovery.
- Added Windows and WSL launches, UTF-8 output, and locked-file retries.
- Added terminal script templates and automatic dashboard opening.

### Changed

- Derived agent names from Claude session IDs instead of transcript UUIDs, migrating 0.2.0 records.

### Fixed

- Kept resume identity and message positions; read complete log lines.
- Released hooks after terminal closure; held delivery while tool calls are pending.
- Raised the default message limit from 4,096 to 1,048,576 characters; rejected invalid control characters.

## 0.2.0 — 2025-09-10

- Added background/headless launches via `open --background` (alias `-p`), per-process logs, and `kill <name>` / `kill --all`.
- Archived cleared conversations in `~/.hcom/archive/session-<timestamp>/`.
- Moved agent state from `hcom.json` to per-agent files so concurrent updates don't clobber each other; old records are not migrated.
- Added a PreToolUse hook to auto-approve Bash commands containing `HCOM_SEND:`; polled for messages every 0.1 s instead of every second.
- Removed only hcom hooks during cleanup, preserving unrelated hooks in the same matcher.
- Fixed premature inactive status, process tracking, timeouts, and human-recipient filters.

## 0.1.8 — 2025-08-30

- Added expanded help for non-interactive callers and documented the existing `watch --wait` command.
- Corrected Claude's messaging permission entry.
- Warned about unmatched mentions; broadcast when none matched a known agent. Messages with any matching mention remained targeted. Version 0.6.4 replaces this fallback with an error.

## 0.1.7 — 2025-08-25

- Clarified named-agent launches and human and hook messaging in help and docs.

## 0.1.6 — 2025-08-24

- Kept hooks silent outside hcom-launched Claude sessions.
- Quoted spaced paths correctly and cleaned up older hook formats.

## 0.1.5 — 2025-08-08

- Honored agent-file model and tool settings unless launch arguments override them.
- Derived identity from the first user entry in resumed transcripts, preserving aliases; allowed participation before transcript identity is available.

## 0.1.4 — 2025-08-02

- Ran hooks through the launching Python and hcom script via `$HCOM` instead of probing for `hcom` or `uvx`.
- Raised the idle wait to 30 minutes and batches to 50 messages.
- Confirmed sends, clarified messages, and cleaned up older hooks.

## 0.1.3 — 2025-08-01

- Resolved hooks via installed `hcom`, `uvx`, then the absolute script path for uv tool and virtualenv installs.

## 0.1.2 — 2025-08-01

- Installed hooks from temporary uvx environments.
- Removed the explicit ten-second PostToolUse timeout; use the hook runner's default.
- Dropped tests from the source package and documented installation.

## 0.1.1 — 2025-08-01

- Fixed a startup syntax error on Python versions below 3.12.

## 0.1.0 — 2025-08-01

- Released on PyPI: replaced `chc` with `hcom` and stored state in `~/.hcom/`.
- Launched Claude sessions and named agents with `hcom open`.
- Installed PostToolUse, Stop, and Notification hooks in the project's `.claude/settings.local.json` for delivery, idle waiting, and status; agents sent with `echo HCOM_SEND:<message>`.
- Added shared chat, mentions, team prefixes, dashboard, config overrides, and archives.

## Before 0.1.0

Claude messaging with the single-file `chc.py`, which set up named group chats with `chc setup`.
