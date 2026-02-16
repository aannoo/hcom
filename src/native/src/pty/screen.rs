//! Screen tracking using vt100 terminal emulator
//!
//! Provides gate conditions for safe injection:
//! - is_ready(): Ready pattern visible on screen
//! - is_waiting_approval(): OSC9 approval notification detected
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

/// OSC9 approval notification patterns.
///
/// Codex emits these escape sequences when user approval is needed:
/// - `OSC9_APPROVAL`: "Approval requested" - for exec or MCP elicitation
/// - `OSC9_EDIT`: "Codex wants to edit" - for file edits
///
/// We detect these in the raw output buffer (before vt100 parsing strips them)
/// to set DB status to "blocked" for TUI visibility. Injection is already gated
/// by hook-set status, but OSC9 detection provides immediate status feedback.
const OSC9_APPROVAL: &[u8] = b"\x1b]9;Approval requested";
const OSC9_EDIT: &[u8] = b"\x1b]9;Codex wants to edit";

/// Check if haystack contains needle (simple O(n) search)
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
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

/// Screen tracker with vt100 emulation
pub struct ScreenTracker {
    parser: vt100::Parser,
    ready_pattern: String,
    waiting_approval: bool,
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
    pub fn new_with_instance(rows: u16, cols: u16, ready_pattern: &[u8], instance_name: Option<&str>) -> Self {
        let config = Config::get();
        let debug_flag_path = config.hcom_dir.join(".tmp").join("pty_debug_on");
        // Enable if env var set OR runtime flag file exists
        let debug_enabled = config.pty_debug || debug_flag_path.exists();
        let debug_file = if debug_enabled {
            Self::open_debug_file(instance_name)
        } else {
            None
        };

        let mut tracker = Self {
            parser: vt100::Parser::new(rows, cols, 0),
            ready_pattern: String::from_utf8_lossy(ready_pattern).into_owned(),
            waiting_approval: false,
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

        // Check for OSC9 approval notifications (fix #8: use simple contains check)
        if contains_bytes(&self.output_buffer, OSC9_APPROVAL)
            || contains_bytes(&self.output_buffer, OSC9_EDIT)
        {
            self.waiting_approval = true;
        }

        // Feed to vt100 parser
        self.parser.process(data);

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
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Clear approval state (user typed something)
    pub fn clear_approval(&mut self) {
        self.waiting_approval = false;
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

    /// Check if waiting for approval (OSC9 detected)
    pub fn is_waiting_approval(&self) -> bool {
        self.waiting_approval
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
        } else if !flag_on && self.debug_enabled && !Config::get().pty_debug {
            // Toggle OFF (only if not also set via env var)
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
            if let Some(cell) = screen.cell(row, col) {
                if cell.contents() == prompt_char {
                    prompt_col = Some(col);
                    break;
                }
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
                if contents.is_empty() || contents.chars().all(|c| c.is_whitespace() || c == '\u{00A0}') {
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
            Err(_) => None,
        }
    }

    /// Get all screen lines as strings
    fn get_screen_lines(&self) -> Vec<String> {
        let screen = self.parser.screen();
        let (_rows, cols) = screen.size();
        screen.rows(0, cols).collect()
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

        for (row_idx, line) in lines.iter().enumerate() {
            // Find ❯ at start of line (Claude's prompt character)
            let trimmed = line.trim_start();
            if !trimmed.starts_with('❯') {
                continue;
            }

            // Check for borders above and below (input box frame)
            if row_idx == 0 {
                continue;
            }
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

            // Extract text after ❯ (trim NBSP too - Claude uses \xa0 after prompt)
            let prompt_pos = line.find('❯')?;
            let after_prompt = &line[prompt_pos + '❯'.len_utf8()..];
            let text = trim_with_nbsp(after_prompt);

            if text.is_empty() {
                return Some(String::new());
            }

            // Dim text = placeholder, not real input
            let is_placeholder = self.is_dim_after_prompt(row_idx as u16, "❯")
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
    /// - New (2025+): `▀` top border with ` > ` prompt line and `▄` bottom border
    /// - Dash: `─` top/bottom borders with ` > ` prompt line (expanded/newer format)
    ///
    /// Multi-line: when text wraps, continuation lines appear between prompt and
    /// bottom border. All lines are collected and joined with spaces.
    ///
    /// The "Type your message" placeholder disappears instantly when user types.
    fn get_gemini_input_text(&self) -> Option<String> {
        let lines = self.get_screen_lines();
        let num_lines = lines.len();

        // Search bottom-to-top for input box top border
        for row_idx in (0..num_lines.saturating_sub(1)).rev() {
            let line = &lines[row_idx];

            // New format (▀ border) or dash format (─ border)
            let is_top_border = line.contains('▀') || is_dash_border(line);

            if is_top_border {
                let next_line = &lines[row_idx + 1];
                // Prompt line starts with " > " (space, >, space)
                if let Some(start) = next_line.find(" > ") {
                    let after = &next_line[start + " > ".len()..];
                    let first_line = after.trim();
                    // Ready pattern visible = prompt is empty (placeholder text)
                    if first_line.is_empty() || self.is_ready() {
                        return Some(String::new());
                    }
                    // Collect continuation lines until bottom border
                    let mut text = first_line.to_string();
                    for k in (row_idx + 2)..num_lines {
                        let cont = &lines[k];
                        if cont.contains('▄') || is_dash_border(cont) {
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
                if next_line.contains("│ >") && next_line.contains('│') {
                    if let Some(start) = next_line.find("│ >") {
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
        }

        // Fallback: if ready pattern visible but box not found, assume empty
        if self.is_ready() {
            return Some(String::new());
        }

        None // Prompt not found
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

    // ==================== Debug Methods ====================

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
        output.push_str(&format!("\n=== SCREEN DUMP {}: {} ===\n", self.debug_counter, label));
        output.push_str(&format!("Tool: {}\n", tool));
        output.push_str(&format!("Ready pattern: {:?}\n", self.ready_pattern));
        output.push_str(&format!("Inject port: {}\n", inject_port));
        output.push_str(&format!("Screen size: {}x{}\n", rows, cols));
        output.push_str(&format!("Cursor: ({}, {})\n", cursor.0, cursor.1));
        output.push_str(&format!("Waiting approval: {}\n", self.waiting_approval));
        output.push_str(&format!("Last output: {}ms ago\n", self.last_output.elapsed().as_millis()));

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
                    _ => None,
                };
                if let Some(pc) = prompt_char {
                    let should_dump = match pc {
                        ">" => trimmed.contains("│ >"),
                        _ => trimmed.contains(pc),
                    };
                    if should_dump {
                        let row = i as u16;
                        let mut attrs_info = format!("       Cell attrs: [{}] ", pc);
                        let mut found_prompt = false;
                        for col in 0..cols {
                            if let Some(cell) = screen.cell(row, col) {
                                let contents = cell.contents();
                                if contents == pc {
                                    found_prompt = true;
                                    continue;
                                }
                                if found_prompt && !contents.is_empty() && !contents.chars().all(|c| c.is_whitespace()) {
                                    let dim_marker = if cell.dim() { "D" } else { "-" };
                                    attrs_info.push_str(&format!("{}:{} ", contents.chars().next().unwrap_or('?'), dim_marker));
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
        output.push_str(&format!("is_output_stable(1000): {}\n", self.is_output_stable(1000)));
        output.push_str(&format!("is_prompt_empty({}): {}\n", tool, self.is_prompt_empty(tool)));
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

        let lines: Vec<String> = self.get_screen_lines()
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect();

        let input_text = self.get_input_box_text(tool);

        // Manual JSON — no serde dependency needed
        let mut j = String::from("{\n");
        // lines array
        j.push_str("  \"lines\": [");
        for (i, line) in lines.iter().enumerate() {
            if i > 0 { j.push_str(", "); }
            j.push_str(&json_escape(line));
        }
        j.push_str("],\n");
        j.push_str(&format!("  \"size\": [{}, {}],\n", rows, cols));
        j.push_str(&format!("  \"cursor\": [{}, {}],\n", cursor.0, cursor.1));
        j.push_str(&format!("  \"ready\": {},\n", self.is_ready()));
        j.push_str(&format!("  \"prompt_empty\": {},\n", self.is_prompt_empty(tool)));
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
            ready_pattern: ready_pattern.to_string(),
            waiting_approval: false,
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

    // ---- OSC9 approval detection ----

    #[test]
    fn detects_osc9_approval() {
        let mut t = make_tracker(24, 80, "");
        assert!(!t.is_waiting_approval());
        t.process(b"\x1b]9;Approval requested\x07");
        assert!(t.is_waiting_approval());
    }

    #[test]
    fn detects_osc9_codex_edit() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]9;Codex wants to edit\x07");
        assert!(t.is_waiting_approval());
    }

    #[test]
    fn clear_approval_resets() {
        let mut t = make_tracker(24, 80, "");
        t.process(b"\x1b]9;Approval requested\x07");
        assert!(t.is_waiting_approval());
        t.clear_approval();
        assert!(!t.is_waiting_approval());
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
        assert_eq!(t.get_codex_input_text(), Some("<hcom>test message</hcom>".to_string()));
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
    fn gemini_dash_border_empty_with_ready() {
        let border = "─".repeat(80);
        let mut t = make_tracker(24, 80, "Type your message");
        t.process(format!("{}\r\n", border).as_bytes());
        t.process(b" >   Type your message or @path/to/file\r\n");
        t.process(format!("{}\r\n", border).as_bytes());
        assert_eq!(t.get_gemini_input_text(), Some(String::new()));
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

    // ---- contains_bytes ----

    #[test]
    fn contains_bytes_basic() {
        assert!(contains_bytes(b"hello world", b"world"));
        assert!(!contains_bytes(b"hello", b"world"));
        assert!(contains_bytes(b"abc", b""));
        assert!(!contains_bytes(b"", b"abc"));
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
