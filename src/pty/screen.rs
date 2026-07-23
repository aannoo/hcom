//! Screen tracking using vt100 terminal emulator
//!
//! Provides gate conditions for safe injection:
//! - is_ready(): Ready pattern visible on screen
//! - is_waiting_approval(): OSC terminal title reports action required
//! - is_output_stable(ms): Screen unchanged for N milliseconds
//! - is_prompt_empty(tool): Input box has no user text
//! - get_input_box_text(tool): Extract text from input box

use std::fs::{File, OpenOptions, create_dir_all};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use crate::config::Config;

/// Escape a string as a JSON string literal (with quotes).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

const OSC_TITLE_0: &[u8] = b"\x1b]0;";
const OSC_TITLE_2: &[u8] = b"\x1b]2;";
const CODEX_ACTION_REQUIRED: &str = "Action Required";

/// Return the last complete OSC 0/2 terminal title in a raw output buffer.
///
/// OSC strings may end with BEL or ST and may be split across PTY reads. Calling
/// this on the rolling output buffer handles both cases without matching ordinary
/// terminal body text.
fn last_osc_title(buffer: &[u8]) -> Option<String> {
    let mut offset = 0;
    let mut last_title = None;

    while offset < buffer.len() {
        let remaining = &buffer[offset..];
        let start_0 = remaining
            .windows(OSC_TITLE_0.len())
            .position(|w| w == OSC_TITLE_0);
        let start_2 = remaining
            .windows(OSC_TITLE_2.len())
            .position(|w| w == OSC_TITLE_2);
        let (start, prefix_len) = match (start_0, start_2) {
            (Some(a), Some(b)) if a <= b => (a, OSC_TITLE_0.len()),
            (Some(_), Some(b)) => (b, OSC_TITLE_2.len()),
            (Some(a), None) => (a, OSC_TITLE_0.len()),
            (None, Some(b)) => (b, OSC_TITLE_2.len()),
            (None, None) => break,
        };

        let content_start = offset + start + prefix_len;
        let content = &buffer[content_start..];
        let bel_end = content.iter().position(|&b| b == b'\x07');
        let st_end = content.windows(2).position(|w| w == b"\x1b\\");
        let (end, terminator_len) = match (bel_end, st_end) {
            (Some(a), Some(b)) if a <= b => (a, 1),
            (Some(_), Some(b)) => (b, 2),
            (Some(a), None) => (a, 1),
            (None, Some(b)) => (b, 2),
            (None, None) => break,
        };

        last_title = Some(String::from_utf8_lossy(&content[..end]).into_owned());
        offset = content_start + end + terminator_len;
    }

    last_title
}

/// Max `char`s of a wrapped tool's title to embed in hcom's own title.
/// Codex/gemini cap their own titles at 80–240; this keeps the combined string
/// readable in tab bars after hcom's `{icon} name [tool]` prefix.
const MAX_CHILD_TITLE_CHARS: usize = 160;

/// Normalize a wrapped tool's raw title into a single bounded line safe to embed
/// inside hcom's own OSC sequence.
///
/// The input is untrusted display text (model output, project paths, etc.). We
/// drop control characters (which could terminate or reshape our OSC) and other
/// C0/C1 codepoints, collapse whitespace runs to a single space, trim the ends,
/// and bound the result to [`MAX_CHILD_TITLE_CHARS`]. Mirrors codex's own
/// `sanitize_terminal_title` so passthrough matches what the tool would render.
fn sanitize_child_title(title: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in title.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        // Strip C0/C1 controls and invisible/bidi format chars — anything that
        // could break the OSC framing or visually reorder the title.
        if ch.is_control() || matches!(ch, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}') {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if out.chars().count() >= MAX_CHILD_TITLE_CHARS {
            break;
        }
        out.push(ch);
    }
    out
}

/// Trim whitespace including NBSP (U+00A0) from both ends
fn trim_with_nbsp(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{00A0}')
}

/// Check if a line is a Gemini dash border (all ─ chars, at least 20 wide)
fn is_dash_border(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 20 && trimmed.chars().all(|c| c == '─')
}

/// Check if a line is a Gemini half-block border (all ▀ or ▄ chars, at least 20 wide).
/// v0.27+ renders the input box with ▄ above prompt and ▀ below; older builds had
/// the inverse. Either is accepted.
fn is_block_border(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 20 && trimmed.chars().all(|c| c == '▀' || c == '▄')
}

/// Screen tracker with vt100 emulation
pub struct ScreenTracker {
    parser: vt100::Parser,
    // Current terminal dimensions, tracked independently of the parser so a
    // panicked parser can be rebuilt from scratch at the right size (see
    // `process`/`resize`).
    rows: u16,
    cols: u16,
    ready_pattern: String,
    waiting_approval: bool,
    // Last complete, sanitized OSC 0/2 title the wrapped tool set, cached for the
    // Combined title passthrough. Only ever holds a fully-terminated title (see
    // `process`), so a title evicted mid-scan from `output_buffer` leaves the last
    // good value intact rather than showing a fragment.
    last_child_title: Option<String>,
    last_output: Instant,
    last_change: Instant,
    output_buffer: Vec<u8>,
    // Debug mode fields
    debug_enabled: bool,
    debug_file: Option<File>,
    debug_counter: u32,
    debug_last_dump: Instant,
    debug_last_flag_check: Instant,
    debug_flag_path: PathBuf,
    instance_name: Option<String>,
}

impl ScreenTracker {
    /// Create a new screen tracker with instance name (for debug logging)
    pub fn new_with_instance(
        rows: u16,
        cols: u16,
        ready_pattern: &[u8],
        instance_name: Option<&str>,
    ) -> Self {
        let config = Config::get();
        let debug_flag_path = config.hcom_dir.join(".tmp").join("pty_debug_on");
        // Enable if runtime flag file exists
        let debug_enabled = debug_flag_path.exists();
        let debug_file = if debug_enabled {
            Self::open_debug_file(instance_name)
        } else {
            None
        };

        let mut tracker = Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
            ready_pattern: String::from_utf8_lossy(ready_pattern).into_owned(),
            waiting_approval: false,
            last_child_title: None,
            last_output: Instant::now(),
            last_change: Instant::now(),
            output_buffer: Vec::with_capacity(4096),
            debug_enabled,
            debug_file,
            debug_counter: 0,
            debug_last_dump: Instant::now(),
            debug_last_flag_check: Instant::now(),
            debug_flag_path,
            instance_name: instance_name.map(|s| s.to_owned()),
        };

        if tracker.debug_enabled {
            tracker.debug_log(&format!(
                "PTY Debug log started for {}\nReady pattern: {:?}\nWill dump screen state every 5 seconds",
                instance_name.unwrap_or("unknown"),
                String::from_utf8_lossy(ready_pattern)
            ));
        }

        tracker
    }

    /// Open debug log file
    fn open_debug_file(instance_name: Option<&str>) -> Option<File> {
        let base = Config::get().hcom_dir;

        let debug_dir = base.join(".tmp").join("logs").join("pty_debug");
        if create_dir_all(&debug_dir).is_err() {
            return None;
        }

        let name = instance_name.unwrap_or("unknown");
        let pid = std::process::id();
        let debug_path = debug_dir.join(format!("{}_{}.log", name, pid));

        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&debug_path)
            .ok()
    }

    /// Write to debug log
    fn debug_log(&mut self, msg: &str) {
        if let Some(ref mut file) = self.debug_file {
            let _ = writeln!(file, "{}", msg);
            let _ = file.flush();
        }
    }

    /// Process output data from PTY
    pub fn process(&mut self, data: &[u8]) {
        // Update output buffer for pattern detection (rolling 4KB)
        self.output_buffer.extend_from_slice(data);
        if self.output_buffer.len() > 4096 {
            let excess = self.output_buffer.len() - 4096;
            self.output_buffer.drain(..excess);
        }

        // Codex emits an ungated OSC terminal title on every state refresh. Treat
        // approval as a level so a later Working/idle title clears it promptly.
        // last_osc_title only returns fully-terminated titles, so caching the
        // sanitized value here never stores a fragment (see `last_child_title`).
        if let Some(title) = last_osc_title(&self.output_buffer) {
            self.waiting_approval = title.contains(CODEX_ACTION_REQUIRED);
            let sanitized = sanitize_child_title(&title);
            // An empty, complete title is meaningful: tools use it to clear
            // their title on exit or when resetting state. Do not retain an
            // obsolete spinner forever in combined mode.
            self.last_child_title = Some(sanitized);
        }

        // Feed to vt100 parser. vt100 has known panics on malformed/edge-case
        // terminal frames (e.g. https://github.com/doy/vt100-rust/issues/28 —
        // a wide character orphaned by a resize, then erased). Catch rather
        // than let it unwind and kill the PTY wrapper (hcom issue #73); the
        // panic hook still logs the underlying panic, so this just contains
        // the blast radius. A parser that panicked mid-mutation may be left
        // in an inconsistent state, so rebuild it from scratch rather than
        // keep using it — this drops the current screen contents, but the
        // next output chunk repopulates it.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.process(data);
        }))
        .is_err()
        {
            crate::log::log_warn(
                "pty",
                "screen.parser_panic",
                &format!(
                    "vt100 parser panicked processing output for {}; resetting screen state",
                    self.instance_name.as_deref().unwrap_or("unknown")
                ),
            );
            self.parser = vt100::Parser::new(self.rows, self.cols, 0);
        }

        // Track output timing
        self.last_output = Instant::now();
        self.last_change = Instant::now();
    }

    /// Get terminal width in columns
    pub fn cols(&self) -> u16 {
        let (_rows, cols) = self.parser.screen().size();
        cols
    }

    /// Resize the screen
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.screen_mut().set_size(rows, cols);
        }))
        .is_err()
        {
            crate::log::log_warn(
                "pty",
                "screen.parser_panic",
                &format!(
                    "vt100 parser panicked resizing screen for {}; resetting screen state",
                    self.instance_name.as_deref().unwrap_or("unknown")
                ),
            );
            self.parser = vt100::Parser::new(rows, cols, 0);
        }
        self.rows = rows;
        self.cols = cols;
    }

    /// Clear approval state immediately when the user responds.
    /// The next complete title refresh remains authoritative.
    pub fn clear_approval(&mut self) {
        self.waiting_approval = false;
        self.output_buffer.clear();
    }

    /// Check if CLI is ready for input injection.
    ///
    /// Scans vt100 screen for ready pattern visibility. The pattern disappears when:
    /// - User types in input box (uncommitted input hides the status bar)
    /// - Slash menu or other overlay is shown
    /// - Claude is in accept-edits mode (pattern hidden entirely)
    ///
    /// Returns `true` if ready_pattern is currently visible on screen.
    /// Always returns `true` if no ready_pattern configured (no gating by pattern).
    pub fn is_ready(&self) -> bool {
        if self.ready_pattern.is_empty() {
            return true;
        }

        let screen = self.parser.screen();
        let (_rows, cols) = screen.size();

        for line in screen.rows(0, cols) {
            if line.contains(&self.ready_pattern) {
                return true;
            }
        }
        false
    }

    /// Check if the latest complete OSC terminal title requires action.
    pub fn is_waiting_approval(&self) -> bool {
        self.waiting_approval
    }

    /// The wrapped tool's last complete, sanitized terminal title, if any.
    /// Used by combined title mode to append the tool's own live title
    /// to hcom's `{icon} name [tool]` label.
    pub fn child_title(&self) -> Option<&str> {
        self.last_child_title.as_deref()
    }

    /// Codex approval fallback for blocker dialogs visible on screen.
    ///
    /// Terminal-title detection is primary. This catches dialog variants that
    /// render before or without the title update while excluding the transcript
    /// viewer, which is navigable but not an approval blocker.
    pub fn is_codex_approval_visible(&self) -> bool {
        let lines = self.get_screen_lines();
        let start = lines
            .iter()
            .rposition(|line| line.trim_start().starts_with('›'))
            .unwrap_or(0);
        let visible = lines[start..].join("\n").to_lowercase();

        if visible.contains("↑/↓ to scroll")
            && visible.contains("q to quit")
            && visible.contains("esc to edit prev")
        {
            return false;
        }

        visible.contains("allow command?")
            || visible.contains("press enter to confirm or esc to cancel")
            || visible.contains("enter to submit answer")
            || visible.contains("enter to submit all")
            || visible.contains("[y/n]")
            || visible.contains("yes (y)")
            || (visible.contains("do you want to")
                && (visible.contains("yes") || visible.contains('❯')))
    }

    /// Antigravity-specific approval detection: the agy TUI renders permission
    /// prompts as plain text in the prompt area ("Requesting permission for: …"
    /// with a "1. Yes / 4. No" menu). No OSC9 fires, so scrape the screen.
    /// Requires both the marker and either the question or the control footer
    /// to avoid flipping on stray occurrences of the marker in scrollback.
    pub fn is_antigravity_approval_visible(&self) -> bool {
        let screen = self.parser.screen();
        let (_rows, cols) = screen.size();
        let mut has_marker = false;
        let mut has_question = false;
        let mut has_footer = false;
        for line in screen.rows(0, cols) {
            if line.contains("Requesting permission for:") {
                has_marker = true;
            }
            if line.contains("Do you want to proceed?") {
                has_question = true;
            }
            if line.contains("tab Amend") && line.contains("edit command") {
                has_footer = true;
            }
        }
        has_marker && (has_question || has_footer)
    }

    /// Cursor-specific approval detection: cursor renders a shell-command
    /// permission prompt as plain text ("Run this command?" + a "Run (once) /
    /// Add … to allowlist / Auto-run everything / Skip (esc or n)" menu). No
    /// OSC9 fires, so scrape the screen. Require the question marker AND a menu
    /// footer option so a stray "Run this command?" in scrollback can't flip it.
    /// (File edits auto-apply by default and don't prompt — verified live.)
    pub fn is_cursor_approval_visible(&self) -> bool {
        let screen = self.parser.screen();
        let (_rows, cols) = screen.size();
        let mut has_question = false;
        let mut has_footer = false;
        for line in screen.rows(0, cols) {
            if line.contains("Run this command?") {
                has_question = true;
            }
            if line.contains("Auto-run everything") || line.contains("Skip (esc") {
                has_footer = true;
            }
        }
        has_question && has_footer
    }

    /// Check if output has been stable for N milliseconds
    /// Note: ms=0 returns true (always stable), which is valid for tools that skip stability check
    pub fn is_output_stable(&self, ms: u64) -> bool {
        if ms == 0 {
            return true; // No stability requirement
        }
        self.last_change.elapsed().as_millis() as u64 >= ms
    }

    /// Get the last output timestamp (for sharing with delivery thread)
    pub fn last_output_instant(&self) -> Instant {
        self.last_output
    }

    /// Check if debug mode is enabled
    #[cfg(unix)]
    pub fn debug_enabled(&self) -> bool {
        self.debug_enabled
    }

    /// Check runtime debug flag file and toggle debug on/off.
    /// Called from main loop on poll timeout (~10s) to allow runtime toggle.
    pub fn check_debug_flag(&mut self) {
        if self.debug_last_flag_check.elapsed().as_secs() < 5 {
            return;
        }
        self.debug_last_flag_check = Instant::now();

        let flag_on = self.debug_flag_path.exists();
        if flag_on && !self.debug_enabled {
            // Toggle ON
            self.debug_enabled = true;
            self.debug_file = Self::open_debug_file(self.instance_name.as_deref());
            self.debug_log("PTY Debug toggled ON at runtime via flag file");
        } else if !flag_on && self.debug_enabled {
            // Toggle OFF
            self.debug_log("PTY Debug toggled OFF at runtime (flag file removed)");
            self.debug_enabled = false;
            self.debug_file = None;
        }
    }

    /// Check if text after a prompt character is dim (placeholder styling).
    /// Returns `Some(true)` if majority dim (placeholder), `Some(false)` if real input.
    /// Returns `None` if the prompt glyph can't be located on the row.
    fn is_dim_after_prompt(&self, row: u16, prompt_char: &str) -> Option<bool> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();

        // Find the column where prompt char is located
        let mut prompt_col: Option<u16> = None;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col)
                && cell.contents() == prompt_char
            {
                prompt_col = Some(col);
                break;
            }
        }
        let prompt_col = prompt_col?;

        // Scan cells after prompt (skip prompt + space)
        let start_col = prompt_col + 2;
        let mut dim_count: u32 = 0;
        let mut non_dim_count: u32 = 0;

        for col in start_col..cols {
            if let Some(cell) = screen.cell(row, col) {
                let contents = cell.contents();
                if contents.is_empty()
                    || contents
                        .chars()
                        .all(|c| c.is_whitespace() || c == '\u{00A0}')
                {
                    continue;
                }
                if cell.dim() {
                    dim_count += 1;
                } else {
                    non_dim_count += 1;
                }
            }
        }

        Some(!(non_dim_count > 0 && non_dim_count > dim_count))
    }

    /// Check if prompt is empty (tool-specific)
    pub fn is_prompt_empty(&self, tool: &str) -> bool {
        match self.get_input_box_text(tool) {
            Some(text) => text.is_empty(),
            None => false, // Can't find prompt = not safe
        }
    }

    /// Get text currently in input box (tool-specific)
    pub fn get_input_box_text(&self, tool: &str) -> Option<String> {
        use crate::tool::Tool;
        use std::str::FromStr;

        match Tool::from_str(tool) {
            Ok(Tool::Claude) => self.get_claude_input_text(),
            Ok(Tool::Gemini) => self.get_gemini_input_text(),
            Ok(Tool::Codex) => self.get_codex_input_text(),
            Ok(Tool::OpenCode) => None, // OpenCode: plugin handles delivery, no PTY input detection needed
            Ok(Tool::Kilo) => None,     // Kilo shares OpenCode's plugin delivery model
            Ok(Tool::Pi) => None,       // Pi plugin handles delivery after bootstrap
            Ok(Tool::Omp) => None,      // Omp plugin handles delivery after bootstrap
            Ok(Tool::Antigravity) => self.get_antigravity_input_text(),
            Ok(Tool::Cursor) => self.get_cursor_input_text(),
            Ok(Tool::Kimi) => self.get_kimi_input_text(),
            Ok(Tool::Copilot) => self.get_copilot_input_text(),
            // Grok: prompt-empty gate uses generic scrape when available; no
            // tool-specific VT100 input parser yet.
            Ok(Tool::Grok) => None,
            Ok(Tool::Adhoc) => None,
            Err(_) => None,
        }
    }

    /// Get all screen lines as strings
    fn get_screen_lines(&self) -> Vec<String> {
        let screen = self.parser.screen();
        let (_rows, cols) = screen.size();
        screen.rows(0, cols).collect()
    }

    /// Return a compact tail of visible screen content for launch-blocked diagnostics.
    pub fn visible_tail(&self, max_lines: usize, max_chars: usize) -> Option<String> {
        let mut lines: Vec<String> = self
            .get_screen_lines()
            .into_iter()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            return None;
        }
        if lines.len() > max_lines {
            lines = lines.split_off(lines.len() - max_lines);
        }
        let mut text = lines.join("\n");
        if text.chars().count() > max_chars {
            text = text.chars().take(max_chars).collect::<String>();
            text.push_str("...");
        }
        Some(text)
    }

    /// Extract Claude input box text.
    ///
    /// Detection based on Claude Code TUI layout:
    /// - Find ❯ prompt character with ─ borders above and below (input box frame)
    /// - Placeholder text is rendered with dim attribute (faint/low intensity)
    /// - User input has normal intensity (not dim)
    ///
    /// Uses vt100's cell-level dim attribute to distinguish placeholder from user input.
    /// This enables 0.5s user_activity_cooldown (same as Gemini/Codex) instead of the
    /// previous 3s workaround needed when using text heuristics.
    fn get_claude_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        let num_lines = lines.len();

        // Search bottom-to-top to find the actual current input box,
        // not stale output lines that happen to match the ❯ + ─ border pattern.
        // `bypassPermissions` mode (require_ready_prompt=false) renders the
        // same bordered box with a plain `>` instead of the styled `❯` — try
        // the styled glyph first since it's the common case and less prone to
        // matching unrelated output.
        for row_idx in (1..num_lines).rev() {
            let line = &lines[row_idx];
            let trimmed = line.trim_start();
            let Some(prompt_char) = ['❯', '>'].into_iter().find(|c| trimmed.starts_with(*c))
            else {
                continue;
            };

            // Check for borders above and below (input box frame)
            let line_above = &lines[row_idx - 1];
            if !line_above.contains('─') {
                continue;
            }

            // Check for ─ border below (may be 1-3 rows down for multi-line input box)
            let mut has_border_below = false;
            for offset in 1..=3 {
                if row_idx + offset >= num_lines {
                    break;
                }
                if lines[row_idx + offset].contains('─') {
                    has_border_below = true;
                    break;
                }
            }
            if !has_border_below {
                continue;
            }

            // Extract text after the prompt char (trim NBSP too - Claude uses \xa0 after prompt)
            let prompt_pos = line.find(prompt_char)?;
            let after_prompt = &line[prompt_pos + prompt_char.len_utf8()..];
            let text = trim_with_nbsp(after_prompt);

            if text.is_empty() {
                return Some(String::new());
            }

            // Dim text = placeholder, not real input
            let is_placeholder = self
                .is_dim_after_prompt(row_idx as u16, &prompt_char.to_string())
                .unwrap_or(true); // Can't find prompt cell = treat as placeholder
            if is_placeholder {
                return Some(String::new());
            } else {
                return Some(text.to_string());
            }
        }

        None // Prompt not found
    }

    /// Extract Gemini input text.
    ///
    /// Gemini uses a bordered input box. Three formats supported:
    /// - Old: `╭` corner with `│ >` prompt line
    /// - Block: half-block borders (`▀` or `▄`) above and below ` > ` prompt line
    /// - Dash: `─` top/bottom borders with ` > ` prompt line (expanded/newer format)
    ///
    /// Multi-line: when text wraps, continuation lines appear between prompt and
    /// bottom border. All lines are collected and joined with spaces.
    ///
    /// The "Type your message" placeholder disappears instantly when user types.
    fn get_gemini_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        let num_lines = lines.len();

        // Search bottom-to-top for input box top border. Gemini renders the box
        // with half-block characters; either `▀` or `▄` may appear on the top
        // border depending on the version (▄ in v0.27+, ▀ in some older builds),
        // so accept either as a border row.
        for row_idx in (0..num_lines.saturating_sub(1)).rev() {
            let line = &lines[row_idx];

            let is_top_border = is_block_border(line) || is_dash_border(line);

            if is_top_border {
                let next_line = &lines[row_idx + 1];
                // Prompt line starts with " > " or " * " (YOLO mode)
                let prompt_match = next_line
                    .find(" > ")
                    .map(|pos| (pos, " > ".len()))
                    .or_else(|| next_line.find(" * ").map(|pos| (pos, " * ".len())));
                if let Some((start, prefix_len)) = prompt_match {
                    let after = &next_line[start + prefix_len..];
                    let first_line = after.trim();
                    // Ready pattern visible = prompt is empty (placeholder text)
                    if first_line.is_empty() || self.is_ready() {
                        return Some(String::new());
                    }
                    // Collect continuation lines until bottom border
                    let mut text = first_line.to_string();
                    for cont in &lines[(row_idx + 2)..num_lines] {
                        if is_block_border(cont) || is_dash_border(cont) {
                            break;
                        }
                        let trimmed = cont.trim();
                        if !trimmed.is_empty() {
                            text.push(' ');
                            text.push_str(trimmed);
                        }
                    }
                    return Some(text);
                }
            }

            // Old format: ╭ corner followed by │ > prompt on next row
            if line.contains('╭') {
                let next_line = &lines[row_idx + 1];
                if next_line.contains("│ >")
                    && next_line.contains('│')
                    && let Some(start) = next_line.find("│ >")
                {
                    let after = &next_line[start + "│ >".len()..];
                    if let Some(end) = after.find('│') {
                        let text = after[..end].trim();
                        if text.is_empty() || self.is_ready() {
                            return Some(String::new());
                        }
                        return Some(text.to_string());
                    }
                }
            }
        }

        // Fallback: if ready pattern visible but box not found, assume empty
        if self.is_ready() {
            return Some(String::new());
        }

        None // Prompt not found
    }

    /// Extract Kimi input box text.
    ///
    /// Kimi renders the prompt inside a rounded box:
    /// ```text
    ///   ╭───────────────╮
    ///   │ > <user text> │
    ///   ╰───────────────╯
    /// ```
    /// Multi-line input adds `│ … │` continuation rows before the bottom border.
    ///
    /// Unlike Gemini, Kimi's ready pattern (`> `) stays on screen even with user
    /// text present, so emptiness is decided purely from the box contents — there
    /// is no `is_ready()` shortcut. Searching bottom-to-top finds the input box
    /// (lowest on screen) before the welcome banner box.
    fn get_kimi_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        let num_lines = lines.len();

        for row_idx in (0..num_lines.saturating_sub(1)).rev() {
            if !lines[row_idx].contains('╭') {
                continue;
            }
            let prompt_line = &lines[row_idx + 1];
            let Some(open) = prompt_line.find('│') else {
                continue;
            };
            let after = &prompt_line[open + '│'.len_utf8()..];
            let Some(close) = after.rfind('│') else {
                continue;
            };
            let mut inner = after[..close].trim();
            // Strip the leading prompt marker (`>` normal mode, `*` yolo mode).
            if let Some(rest) = inner.strip_prefix('>').or_else(|| inner.strip_prefix('*')) {
                inner = rest.trim();
            }
            if inner.is_empty() {
                return Some(String::new());
            }
            // Collect wrapped continuation rows until the bottom border.
            let mut text = inner.to_string();
            for cont in &lines[(row_idx + 2)..num_lines] {
                if cont.contains('╰') || cont.contains('╭') {
                    break;
                }
                let t = cont
                    .trim()
                    .trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim();
                if !t.is_empty() {
                    text.push(' ');
                    text.push_str(t);
                }
            }
            return Some(text);
        }

        None // Input box not found
    }

    /// Extract Codex input text.
    ///
    /// Codex uses `›` (U+203A) as prompt character. Placeholder text is rendered
    /// with dim attribute, real user input is not dim.
    ///
    /// Uses vt100's cell-level dim attribute to distinguish placeholder from
    /// real input, avoiding race conditions where ready pattern is still visible
    /// during PTY injection.
    fn get_codex_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();

        // Search bottom-to-top for › prompt character
        // › (U+203A, SINGLE RIGHT-POINTING ANGLE QUOTATION MARK) = 3 bytes UTF-8 + 1 space = 4 bytes total
        for (row_idx, line) in lines.iter().enumerate().rev() {
            let trimmed = line.trim_start();
            if let Some(text) = trimmed.strip_prefix("› ") {
                let text = trim_with_nbsp(text);

                if text.is_empty() {
                    return Some(String::new());
                }

                // Dim text = placeholder, not real input
                match self.is_dim_after_prompt(row_idx as u16, "›") {
                    Some(true) => return Some(String::new()),
                    Some(false) => return Some(text.to_string()),
                    None => {
                        // Can't locate prompt glyph, fall back to ready-pattern logic
                        if self.is_ready() {
                            return Some(String::new());
                        }
                        return Some(text.to_string());
                    }
                }
            }
        }

        // Fallback: if ready pattern visible but prompt not found, assume empty
        if self.is_ready() {
            return Some(String::new());
        }

        None // Prompt not found
    }

    /// Extract Antigravity (`agy`) input text.
    ///
    /// The agy TUI uses a `>` prompt (with or without a trailing space). Only the
    /// bottommost prompt line is considered; scrollback may contain older `> …` lines.
    fn get_antigravity_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();

        if let Some((row_idx, text)) = lines.iter().enumerate().rev().find_map(|(row_idx, line)| {
            let trimmed = line.trim_start();
            let after = trimmed.strip_prefix('>')?.trim_start();
            Some((row_idx, trim_with_nbsp(after)))
        }) {
            if text.is_empty() {
                return Some(String::new());
            }

            return match self.is_dim_after_prompt(row_idx as u16, ">") {
                Some(true) => Some(String::new()),
                Some(false) => Some(text.to_string()),
                None => {
                    if self.is_ready() {
                        Some(String::new())
                    } else {
                        Some(text.to_string())
                    }
                }
            };
        }

        if self.is_ready() {
            return Some(String::new());
        }

        None
    }

    /// Extract Cursor Agent input text.
    ///
    /// Cursor renders a `→` prompt with dim placeholder text while idle.
    /// Submitted or user-entered text uses normal intensity.
    fn get_cursor_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        for (row_idx, line) in lines.iter().enumerate().rev() {
            let trimmed = line.trim_start();
            if let Some(text) = trimmed.strip_prefix("→ ") {
                let text = trim_with_nbsp(text);
                if text.is_empty() {
                    return Some(String::new());
                }
                return match self.is_dim_after_prompt(row_idx as u16, "→") {
                    Some(true) => Some(String::new()),
                    Some(false) => Some(text.to_string()),
                    None => Some(text.to_string()),
                };
            }
        }
        None
    }

    /// Extract GitHub Copilot CLI input text.
    ///
    /// Copilot uses `❯` as the prompt glyph and has no dim placeholder in the
    /// empty state: an empty prompt is just a bare `❯` line.
    fn get_copilot_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        for line in lines.iter().rev() {
            let trimmed = line.trim_start();
            if let Some(text) = trimmed.strip_prefix('❯') {
                return Some(trim_with_nbsp(text.trim_start()).to_string());
            }
        }
        None
    }

    /// Check and perform periodic dump if 5 seconds elapsed
    /// Returns true if dump was performed
    pub fn check_periodic_dump(&mut self, tool: &str, inject_port: u16, label: &str) -> bool {
        if !self.debug_enabled {
            return false;
        }

        if self.debug_last_dump.elapsed().as_secs() >= 5 {
            self.dump_screen(tool, inject_port, label);
            self.debug_last_dump = Instant::now();
            return true;
        }

        false
    }

    /// Dump screen state to debug log (when HCOM_PTY_DEBUG=1)
    pub fn dump_screen(&mut self, tool: &str, inject_port: u16, label: &str) {
        if !self.debug_enabled {
            return;
        }

        self.debug_counter += 1;

        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let cursor = screen.cursor_position();

        let mut output = String::new();
        output.push_str(&format!(
            "\n=== SCREEN DUMP {}: {} ===\n",
            self.debug_counter, label
        ));
        output.push_str(&format!("Tool: {}\n", tool));
        output.push_str(&format!("Ready pattern: {:?}\n", self.ready_pattern));
        output.push_str(&format!("Inject port: {}\n", inject_port));
        output.push_str(&format!("Screen size: {}x{}\n", rows, cols));
        output.push_str(&format!("Cursor: ({}, {})\n", cursor.0, cursor.1));
        output.push_str(&format!("Waiting approval: {}\n", self.waiting_approval));
        output.push_str(&format!(
            "Last output: {}ms ago\n",
            self.last_output.elapsed().as_millis()
        ));

        // Screen content (non-empty lines only)
        output.push_str("Screen content (non-empty lines):\n");
        let lines = self.get_screen_lines();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                output.push_str(&format!("  {:3}: {}\n", i, trimmed));

                // For Claude prompt lines, show cell attributes to verify dim detection
                use crate::tool::Tool;
                use std::str::FromStr;

                let prompt_char = match Tool::from_str(tool) {
                    Ok(Tool::Claude) => Some("❯"),
                    Ok(Tool::Codex) => Some("›"),
                    Ok(Tool::Gemini) => Some(">"),
                    Ok(Tool::Antigravity) => Some(">"),
                    Ok(Tool::Cursor) => Some("→"),
                    Ok(Tool::Copilot) => Some("❯"),
                    _ => None,
                };
                if let Some(pc) = prompt_char {
                    let should_dump = match (Tool::from_str(tool), pc) {
                        (Ok(Tool::Gemini), ">") => trimmed.contains("│ >"),
                        (Ok(Tool::Antigravity), ">") => trimmed.starts_with("> "),
                        _ => trimmed.contains(pc),
                    };
                    if should_dump {
                        let row = i as u16;
                        let prompt_marker = if matches!(Tool::from_str(tool), Ok(Tool::Antigravity))
                        {
                            "> "
                        } else {
                            pc
                        };
                        let mut attrs_info = format!("       Cell attrs: [{}] ", prompt_marker);
                        let mut found_prompt = false;
                        for col in 0..cols {
                            if let Some(cell) = screen.cell(row, col) {
                                let contents = cell.contents();
                                if contents == pc || (prompt_marker == "> " && contents == ">") {
                                    found_prompt = true;
                                    continue;
                                }
                                if found_prompt
                                    && !contents.is_empty()
                                    && !contents.chars().all(|c| c.is_whitespace())
                                {
                                    let dim_marker = if cell.dim() { "D" } else { "-" };
                                    attrs_info.push_str(&format!(
                                        "{}:{} ",
                                        contents.chars().next().unwrap_or('?'),
                                        dim_marker
                                    ));
                                }
                            }
                        }
                        output.push_str(&format!("{}\n", attrs_info));
                    }
                }
            }
        }

        // Status checks
        output.push_str(&format!("is_ready(): {}\n", self.is_ready()));
        output.push_str(&format!(
            "is_output_stable(1000): {}\n",
            self.is_output_stable(1000)
        ));
        output.push_str(&format!(
            "is_prompt_empty({}): {}\n",
            tool,
            self.is_prompt_empty(tool)
        ));
        if let Some(text) = self.get_input_box_text(tool) {
            output.push_str(&format!("get_input_box_text: {:?}\n", text));
        } else {
            output.push_str("get_input_box_text: None\n");
        }
        output.push('\n');

        self.debug_log(&output);
    }

    /// Get screen state as JSON for TCP query responses.
    pub fn get_screen_dump(&self, tool: &str, _inject_port: u16) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let cursor = screen.cursor_position();

        let lines: Vec<String> = self
            .get_screen_lines()
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect();

        let input_text = self.get_input_box_text(tool);

        // Manual JSON — no serde dependency needed
        let mut j = String::from("{\n");
        // lines array
        j.push_str("  \"lines\": [");
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                j.push_str(", ");
            }
            j.push_str(&json_escape(line));
        }
        j.push_str("],\n");
        j.push_str(&format!("  \"size\": [{}, {}],\n", rows, cols));
        j.push_str(&format!("  \"cursor\": [{}, {}],\n", cursor.0, cursor.1));
        j.push_str(&format!("  \"ready\": {},\n", self.is_ready()));
        j.push_str(&format!(
            "  \"prompt_empty\": {},\n",
            self.is_prompt_empty(tool)
        ));
        match input_text {
            Some(ref t) => j.push_str(&format!("  \"input_text\": {}\n", json_escape(t))),
            None => j.push_str("  \"input_text\": null\n"),
        }
        j.push_str("}\n");
        j
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create tracker without debug/config dependencies
    fn make_tracker(rows: u16, cols: u16, ready_pattern: &str) -> ScreenTracker {
        ScreenTracker {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
            ready_pattern: ready_pattern.to_string(),
            waiting_approval: false,
            last_child_title: None,
            last_output: Instant::now(),
            last_change: Instant::now(),
            output_buffer: Vec::new(),
            debug_enabled: false,
            debug_file: None,
            debug_counter: 0,
            debug_last_dump: Instant::now(),
            debug_last_flag_check: Instant::now(),
            debug_flag_path: std::path::PathBuf::new(),
            instance_name: None,
        }
    }

    #[test]
    fn sanitize_child_title_strips_controls_and_collapses_whitespace() {
        assert_eq!(
            sanitize_child_title("  Working\t\non   task  "),
            "Working on task"
        );
        // Embedded ESC / BEL (the only bytes that could break out of our OSC)
        // are dropped; the harmless leftover text stays.
        assert_eq!(sanitize_child_title("a\x1b]2;evil\x07b"), "a]2;evilb");
        assert_eq!(sanitize_child_title(""), "");
    }

    #[test]
    fn sanitize_child_title_bounds_length() {
        let long = "x".repeat(MAX_CHILD_TITLE_CHARS + 50);
        assert_eq!(
            sanitize_child_title(&long).chars().count(),
            MAX_CHILD_TITLE_CHARS
        );
    }

    #[test]
    fn process_captures_child_osc_title() {
        let mut t = make_tracker(24, 80, "");
        assert_eq!(t.child_title(), None);
        t.process(b"before\x1b]0;\xe2\xa0\x8b Working\x07after");
        assert_eq!(t.child_title(), Some("⠋ Working"));
    }

    #[test]
    fn child_title_keeps_last_complete_through_eviction() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]0;First title\x07");
        assert_eq!(t.child_title(), Some("First title"));
        // Flood past the 4KB rolling buffer with plain output containing no
        // complete title; the last good title must survive rather than clear.
        t.process(&vec![b'.'; 8192]);
        assert_eq!(t.child_title(), Some("First title"));
    }

    // ---- vt100 panic containment (issue #73) ----

    #[test]
    fn process_survives_vt100_wide_char_resize_panic() {
        // A double-width (wide) character whose continuation cell gets
        // truncated by a downward resize used to panic inside vt100
        // (upstream doy/vt100-rust#28: `Row::clear_wide` indexes one past
        // the row's new length) the next time that cell was erased. That
        // panic used to unwind straight through `process`/`resize` and kill
        // the PTY wrapper (hcom issue #73, observed as repeated
        // `stopped by pty: closed` on real Codex sessions). It must now be
        // contained: the tracker rebuilds its parser and stays usable.
        let mut t = make_tracker(3, 10, "");

        // Wide CJK char printed so it spans the last two columns (8, 9).
        t.process(b"\x1b[1;9H");
        t.process("\u{4e2d}".as_bytes());

        // Shrink to 9 columns: the continuation cell (old col 9) is
        // truncated away, orphaning the wide flag on the new last column.
        t.resize(3, 9);

        // Erase-in-line on that orphaned wide cell is what panicked upstream.
        t.process(b"\x1b[1;9H\x1b[K");

        // Tracker must have survived and still be fully usable.
        assert_eq!(t.cols(), 9);
        t.process(b"still alive\r\n");
    }

    // ---- is_ready ----

    #[test]
    fn is_ready_when_pattern_visible() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process(b"Some output\r\n? for shortcuts\r\n");
        assert!(t.is_ready());
    }

    #[test]
    fn is_ready_false_when_pattern_absent() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process(b"Some output\r\nno pattern here\r\n");
        assert!(!t.is_ready());
    }

    #[test]
    fn is_ready_true_when_no_pattern_configured() {
        let t = make_tracker(24, 80, "");
        assert!(t.is_ready());
    }

    // ---- Codex approval detection ----

    #[test]
    fn detects_action_required_title() {
        let mut t = make_tracker(24, 80, "");
        assert!(!t.is_waiting_approval());
        t.process(b"\x1b]0;[ ! ] Action Required | proj | Working\x07");
        assert!(t.is_waiting_approval());
    }

    #[test]
    fn detects_hidden_action_required_title() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]2;[ . ] Action Required | proj | Working\x1b\\");
        assert!(t.is_waiting_approval());
    }

    #[test]
    fn title_refresh_clears_approval() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]0;[ ! ] Action Required | proj | Working\x07");
        assert!(t.is_waiting_approval());
        t.process(b"\x1b]0;proj | Working\x07");
        assert!(!t.is_waiting_approval());
    }

    #[test]
    fn detects_title_split_across_process_calls() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]0;[ ! ] Action");
        assert!(!t.is_waiting_approval());
        t.process(b" Required | proj | Working\x07");
        assert!(t.is_waiting_approval());
    }

    #[test]
    fn action_required_body_text_does_not_trigger() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"Action Required is ordinary agent output\r\n");
        assert!(!t.is_waiting_approval());
    }

    #[test]
    fn clear_approval_resets_title_state() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]0;[ ! ] Action Required | proj | Working\x07");
        assert!(t.is_waiting_approval());
        t.clear_approval();
        assert!(!t.is_waiting_approval());
    }

    #[test]
    fn codex_detects_visible_approval_dialog() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"Run command\r\nAllow command?\r\nPress enter to confirm or esc to cancel\r\n");
        assert!(t.is_codex_approval_visible());
    }

    #[test]
    fn codex_transcript_viewer_is_not_approval() {
        let mut t = make_tracker(24, 80, "");
        t.process("Transcript\r\n↑/↓ to scroll  q to quit  esc to edit prev\r\n".as_bytes());
        assert!(!t.is_codex_approval_visible());
    }

    // ---- Antigravity approval detection ----

    #[test]
    fn antigravity_detects_approval_prompt() {
        let mut t = make_tracker(24, 80, "");
        assert!(!t.is_antigravity_approval_visible());
        t.process(
            b"Requesting permission for: hcom list --name lida\r\nDo you want to proceed?\r\n",
        );
        assert!(t.is_antigravity_approval_visible());
    }

    #[test]
    fn antigravity_no_false_positive_without_marker() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"> hello world\r\nrunning hcom list\r\n");
        assert!(!t.is_antigravity_approval_visible());
    }

    #[test]
    fn antigravity_marker_alone_does_not_trigger() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"agent said: Requesting permission for: something earlier\r\n> idle\r\n");
        assert!(!t.is_antigravity_approval_visible());
    }

    #[test]
    fn antigravity_detects_via_control_footer() {
        let mut t = make_tracker(24, 80, "");
        t.process(
            b"Requesting permission for: rm -rf /tmp/x\r\n  1. Yes\r\n  2. No\r\n  tab Amend . e edit command\r\n",
        );
        assert!(t.is_antigravity_approval_visible());
    }

    // ---- Cursor approval detection ----

    #[test]
    fn cursor_detects_approval_prompt() {
        let mut t = make_tracker(24, 80, "");
        assert!(!t.is_cursor_approval_visible());
        // Real cursor shell-approval menu (captured live).
        t.process(
            b"Run this command?\r\nNot in allowlist: uptime\r\n  Run (once) (y)\r\n  Auto-run everything (shift+tab)\r\n  Skip (esc or n)\r\n",
        );
        assert!(t.is_cursor_approval_visible());
    }

    #[test]
    fn cursor_question_alone_does_not_trigger() {
        // The question text in narration/scrollback without the menu footer
        // must not flip approval on.
        let mut t = make_tracker(24, 80, "");
        t.process(b"I'll ask: Run this command? then proceed.\r\n> idle\r\n");
        assert!(!t.is_cursor_approval_visible());
    }

    #[test]
    fn cursor_no_false_positive_without_question() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"> hello world\r\nrunning a build\r\n");
        assert!(!t.is_cursor_approval_visible());
    }

    // ---- Codex input extraction ----

    #[test]
    fn codex_extracts_text_after_prompt() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("› hello world\r\n".as_bytes());
        assert_eq!(t.get_codex_input_text(), Some("hello world".to_string()));
    }

    #[test]
    fn codex_empty_prompt() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("› \r\n".as_bytes());
        assert_eq!(t.get_codex_input_text(), Some(String::new()));
    }

    #[test]
    fn codex_no_prompt_no_ready() {
        let t = make_tracker(24, 80, "? for shortcuts");
        assert_eq!(t.get_codex_input_text(), None);
    }

    #[test]
    fn codex_dim_placeholder_with_ready_returns_empty() {
        // Codex shows dim placeholder text when idle + ready pattern visible
        // Should return empty (it's placeholder, not real input)
        let mut t = make_tracker(24, 80, "? for shortcuts");
        // SGR 2 = dim, SGR 0 = reset
        let mut data = Vec::new();
        data.extend_from_slice("› ".as_bytes());
        data.extend_from_slice(b"\x1b[2m"); // dim on
        data.extend_from_slice(b"Improve docs");
        data.extend_from_slice(b"\x1b[0m"); // reset
        data.extend_from_slice(b"\r\n? for shortcuts\r\n");
        t.process(&data);
        assert_eq!(t.get_codex_input_text(), Some(String::new()));
    }

    #[test]
    fn codex_non_dim_text_with_ready_returns_text() {
        // Injected text is NOT dim, even if ready pattern still visible (race condition)
        // Should return the text (it's real input, not placeholder)
        let mut t = make_tracker(24, 80, "? for shortcuts");
        // Non-dim text after prompt, ready pattern on next line
        t.process("› <hcom>test message</hcom>\r\n? for shortcuts\r\n".as_bytes());
        // Current bug: returns empty because is_ready()=true
        // After fix: should return the actual text
        assert_eq!(
            t.get_codex_input_text(),
            Some("<hcom>test message</hcom>".to_string())
        );
    }

    // ---- Cursor input extraction ----

    #[test]
    fn cursor_extracts_non_dim_text_after_prompt() {
        let mut t = make_tracker(24, 80, "");
        t.process("→ <hcom>\r\n".as_bytes());
        assert_eq!(t.get_cursor_input_text(), Some("<hcom>".to_string()));
    }

    #[test]
    fn cursor_dim_placeholder_returns_empty() {
        let mut t = make_tracker(24, 80, "");
        let mut data = Vec::new();
        data.extend_from_slice("→ ".as_bytes());
        data.extend_from_slice(b"\x1b[2mPlan, search, build anything\x1b[0m");
        data.extend_from_slice(b"\r\n");
        t.process(&data);
        assert_eq!(t.get_cursor_input_text(), Some(String::new()));
        assert!(t.is_prompt_empty("cursor"));
    }

    // ---- Gemini input extraction ----

    #[test]
    fn gemini_extracts_text_from_bordered_box() {
        let mut t = make_tracker(24, 80, "Type your message");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ > hello gemini           │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some("hello gemini".to_string()));
    }

    #[test]
    fn gemini_empty_box() {
        let mut t = make_tracker(24, 80, "Type your message");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ >                        │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some(String::new()));
    }

    #[test]
    fn gemini_no_box_but_ready_pattern() {
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(b"Type your message\r\n");
        // No box found, but ready pattern visible → fallback to empty
        assert_eq!(t.get_gemini_input_text(), Some(String::new()));
    }

    #[test]
    fn gemini_dash_border_single_line() {
        let border = "─".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", border).as_bytes());
        t.process(b" > hello gemini\r\n");
        t.process(format!("{}\r\n", border).as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some("hello gemini".to_string()));
    }

    #[test]
    fn gemini_dash_border_multi_line() {
        let border = "─".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", border).as_bytes());
        t.process(b" > first line of text\r\n");
        t.process(b"   second line of text\r\n");
        t.process(format!("{}\r\n", border).as_bytes());
        assert_eq!(
            t.get_gemini_input_text(),
            Some("first line of text second line of text".to_string())
        );
    }

    #[test]
    fn gemini_new_format_multi_line() {
        let top = "▀".repeat(80);
        let bottom = "▄".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", top).as_bytes());
        t.process(b" > first line\r\n");
        t.process(b"   second line\r\n");
        t.process(format!("{}\r\n", bottom).as_bytes());
        assert_eq!(
            t.get_gemini_input_text(),
            Some("first line second line".to_string())
        );
    }

    #[test]
    fn gemini_inverted_block_borders() {
        // Gemini CLI v0.40+ renders ▄ above the prompt and ▀ below it
        // (visually correct: ▄ fills bottom of its row → line above next row).
        let top = "▄".repeat(80);
        let bottom = "▀".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", top).as_bytes());
        t.process(b" > injected text\r\n");
        t.process(format!("{}\r\n", bottom).as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some("injected text".to_string()));
    }

    #[test]
    fn gemini_yolo_mode_extracts_text() {
        let top = "▀".repeat(80);
        let bottom = "▄".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", top).as_bytes());
        t.process(b" *   hello from yolo\r\n");
        t.process(format!("{}\r\n", bottom).as_bytes());
        assert_eq!(
            t.get_gemini_input_text(),
            Some("hello from yolo".to_string())
        );
    }

    #[test]
    fn gemini_yolo_mode_empty_with_ready() {
        let top = "▀".repeat(80);
        let bottom = "▄".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", top).as_bytes());
        t.process(b" *   Type your message or @path/to/file\r\n");
        t.process(format!("{}\r\n", bottom).as_bytes());
        // Ready pattern visible in prompt text → empty
        assert_eq!(t.get_gemini_input_text(), Some(String::new()));
    }

    // ---- Kimi input extraction ----

    #[test]
    fn kimi_empty_box_is_empty() {
        let mut t = make_tracker(24, 80, "> ");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ >                        │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_kimi_input_text(), Some(String::new()));
        assert!(t.is_prompt_empty("kimi"));
    }

    #[test]
    fn kimi_extracts_typed_text() {
        let mut t = make_tracker(24, 80, "> ");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ > hello kimi             │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_kimi_input_text(), Some("hello kimi".to_string()));
        // Crucial: ready pattern `> ` is still on screen, but the box has text,
        // so the prompt must NOT be reported empty (would clobber user input).
        assert!(!t.is_prompt_empty("kimi"));
    }

    #[test]
    fn kimi_picks_input_box_over_welcome_banner() {
        let mut t = make_tracker(30, 80, "> ");
        // Welcome banner box (also uses ╭ … ╰) above the input box.
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│  Welcome to Kimi Code!   │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ >                        │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_kimi_input_text(), Some(String::new()));
    }

    #[test]
    fn kimi_multi_line_input() {
        let mut t = make_tracker(24, 80, "> ");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ > first line             │\r\n".as_bytes());
        t.process("│   second line            │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(
            t.get_kimi_input_text(),
            Some("first line second line".to_string())
        );
    }

    #[test]
    fn kimi_yolo_marker_stripped() {
        let mut t = make_tracker(24, 80, "> ");
        t.process("╭──────────────────────────╮\r\n".as_bytes());
        t.process("│ *                        │\r\n".as_bytes());
        t.process("╰──────────────────────────╯\r\n".as_bytes());
        assert_eq!(t.get_kimi_input_text(), Some(String::new()));
    }

    #[test]
    fn gemini_dash_border_empty_with_ready() {
        let border = "─".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", border).as_bytes());
        t.process(b" >   Type your message or @path/to/file\r\n");
        t.process(format!("{}\r\n", border).as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some(String::new()));
    }

    // ---- Antigravity input extraction ----

    #[test]
    fn antigravity_extracts_text_after_prompt() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("> hello agy\r\n".as_bytes());
        assert_eq!(
            t.get_antigravity_input_text(),
            Some("hello agy".to_string())
        );
        assert_eq!(
            t.get_input_box_text("antigravity"),
            Some("hello agy".to_string())
        );
    }

    #[test]
    fn antigravity_prompt_without_trailing_space() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process(">\r\n".as_bytes());
        assert_eq!(t.get_antigravity_input_text(), Some(String::new()));
        assert!(t.is_prompt_empty("antigravity"));
    }

    #[test]
    fn antigravity_dim_placeholder_with_ready_returns_empty() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        let mut data = Vec::new();
        data.extend_from_slice(b"> ");
        data.extend_from_slice(b"\x1b[2mType your message\x1b[0m");
        data.extend_from_slice(b"\r\n? for shortcuts\r\n");
        t.process(&data);
        assert_eq!(t.get_antigravity_input_text(), Some(String::new()));
    }

    #[test]
    fn antigravity_empty_prompt_with_ready() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("> \r\n? for shortcuts\r\n".as_bytes());
        assert_eq!(t.get_antigravity_input_text(), Some(String::new()));
    }

    #[test]
    fn antigravity_injected_text_with_ready_footer() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("> <hcom>test</hcom>\r\n? for shortcuts\r\n".as_bytes());
        assert_eq!(
            t.get_antigravity_input_text(),
            Some("<hcom>test</hcom>".to_string())
        );
    }

    #[test]
    fn antigravity_uses_bottommost_prompt_only() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("> <hcom>old message</hcom>\r\n".as_bytes());
        t.process("some agent output\r\n".as_bytes());
        t.process("> \r\n? for shortcuts\r\n".as_bytes());
        assert_eq!(t.get_antigravity_input_text(), Some(String::new()));
    }

    // ---- Claude input extraction ----
    // Claude uses dim attribute detection which requires proper VT100 SGR sequences

    #[test]
    fn claude_no_prompt_returns_none() {
        let t = make_tracker(24, 80, "? for shortcuts");
        assert_eq!(t.get_claude_input_text(), None);
    }

    #[test]
    fn claude_prompt_with_borders_and_empty_text() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        t.process("❯ \r\n".as_bytes());
        t.process("────────────────────\r\n".as_bytes());
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    #[test]
    fn claude_prompt_with_non_dim_user_text() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        t.process("❯ hello\r\n".as_bytes());
        t.process("────────────────────\r\n".as_bytes());
        let result = t.get_claude_input_text();
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn claude_prompt_with_dim_placeholder() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        // ❯ followed by dim text (SGR 2 = dim)
        let mut data = Vec::new();
        data.extend_from_slice("❯ ".as_bytes());
        data.extend_from_slice(b"\x1b[2m"); // SGR dim on
        data.extend_from_slice(b"placeholder text");
        data.extend_from_slice(b"\x1b[0m"); // SGR reset
        data.extend_from_slice(b"\r\n");
        t.process(&data);
        t.process("────────────────────\r\n".as_bytes());
        // Dim text should be treated as empty (placeholder)
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    #[test]
    fn claude_prompt_with_multiline_input_box_dim_placeholder() {
        // Claude Code sometimes shows a 2-line input box with the bottom border
        // 2 rows below the prompt (empty continuation row in between).
        // Dim placeholder text should still be detected as empty.
        let mut t = make_tracker(24, 52, "? for shortcuts");
        t.process("────────────────────────────────────────────────────\r\n".as_bytes());
        let mut data = Vec::new();
        data.extend_from_slice("❯ ".as_bytes());
        data.extend_from_slice(b"\x1b[2m"); // dim on
        data.extend_from_slice(b"tell the implementation agent to fix those");
        data.extend_from_slice(b"\x1b[0m"); // reset
        data.extend_from_slice(b"\r\n");
        t.process(&data);
        t.process(b"\r\n"); // empty continuation row
        t.process("────────────────────────────────────────────────────\r\n".as_bytes());
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    #[test]
    fn claude_prompt_picks_bottom_input_box_over_stale_output() {
        // Regression: cargo build output can produce ❯ + ─ patterns in the
        // scrollback that look like an input box. The parser must find the
        // *bottom-most* match (the real input box), not the first one.
        let mut t = make_tracker(30, 69, "? for shortcuts");
        // Stale output with ❯ between ─ lines
        t.process(
            "─────    Finished `dev` profile [unoptimized + debuginfo] target   ──\r\n".as_bytes(),
        );
        t.process("❯    (s) in 1.36s\r\n".as_bytes());
        t.process(
            " ─   Stale entry added, SessionStart groups: 3───────────────────────\r\n".as_bytes(),
        );
        // Some output in between
        t.process("Some other output\r\n".as_bytes());
        t.process("\r\n".as_bytes());
        // Real input box at the bottom
        t.process(
            "─────────────────────────────────────────────────────────────────────\r\n".as_bytes(),
        );
        t.process("❯\r\n".as_bytes());
        t.process(
            "─────────────────────────────────────────────────────────────────────\r\n".as_bytes(),
        );
        // Real prompt is empty — parser should find this, not the stale one
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    #[test]
    fn claude_bypass_permissions_ascii_prompt_with_empty_text() {
        // `--permission-mode bypassPermissions` renders the same bordered box
        // with a plain `>` instead of the styled `❯`.
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        t.process("> \r\n".as_bytes());
        t.process("────────────────────\r\n".as_bytes());
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    #[test]
    fn claude_bypass_permissions_ascii_prompt_with_non_dim_user_text() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        t.process("> hello\r\n".as_bytes());
        t.process("────────────────────\r\n".as_bytes());
        assert_eq!(t.get_claude_input_text(), Some("hello".to_string()));
    }

    #[test]
    fn claude_bypass_permissions_ascii_prompt_with_dim_placeholder() {
        let mut t = make_tracker(24, 80, "? for shortcuts");
        t.process("────────────────────\r\n".as_bytes());
        let mut data = Vec::new();
        data.extend_from_slice("> ".as_bytes());
        data.extend_from_slice(b"\x1b[2m"); // SGR dim on
        data.extend_from_slice(b"placeholder text");
        data.extend_from_slice(b"\x1b[0m"); // SGR reset
        data.extend_from_slice(b"\r\n");
        t.process(&data);
        t.process("────────────────────\r\n".as_bytes());
        assert_eq!(t.get_claude_input_text(), Some(String::new()));
    }

    // ---- trim_with_nbsp ----

    #[test]
    fn trim_nbsp() {
        assert_eq!(trim_with_nbsp(" hello\u{00A0}"), "hello");
        assert_eq!(trim_with_nbsp("\u{00A0}\u{00A0}"), "");
    }

    // ---- output stability ----

    #[test]
    fn output_stable_zero_always_true() {
        let t = make_tracker(24, 80, "");
        assert!(t.is_output_stable(0));
    }
}
