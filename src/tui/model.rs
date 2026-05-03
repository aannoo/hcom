use crate::tui::theme::Theme;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViewMode {
    Inline,   // agents only, no messages panel
    Vertical, // agents left, messages right
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputMode {
    Navigate,
    Compose,
    CommandOutput,
    Launch,
    Relay,
}

pub struct CommandResult {
    pub label: String,
    pub output: Vec<String>,
}

pub struct RelayPopupState {
    pub enabled: bool,
    pub toggling: bool, // true while relay toggle RPC is in-flight
    pub cursor: u8,     // 0=toggle, 1=status, 2=new, 3=connect
    pub editing_token: bool,
    pub token_input: String,
    pub token_cursor: usize,
}

impl RelayPopupState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            toggling: false,
            cursor: 0,
            editing_token: false,
            token_input: String::new(),
            token_cursor: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AgentStatus {
    Active,
    Listening,
    Blocked,
    Launching,
    Inactive,
}

impl AgentStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Active => "\u{25b6}",
            Self::Listening => "\u{25c9}",
            Self::Blocked => "\u{25a0}",
            Self::Launching => "\u{25ce}",
            Self::Inactive => "\u{25cb}",
        }
    }

    pub fn style(&self) -> Style {
        match self {
            Self::Active => Theme::active(),
            Self::Listening => Theme::listening(),
            Self::Blocked => Theme::blocked(),
            Self::Launching => Theme::launching(),
            Self::Inactive => Theme::inactive(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Claude,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Cline,
    Adhoc,
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Kilo => "kilocode",
            Self::Cline => "cline",
            Self::Adhoc => "adhoc",
        }
    }

    /// Cycle forward (for launch panel). Adhoc is not launchable.
    pub fn next(&self) -> Self {
        match self {
            Self::Claude => Self::Gemini,
            Self::Gemini => Self::Codex,
            Self::Codex => Self::OpenCode,
            Self::OpenCode => Self::Kilo,
            Self::Kilo => Self::Cline,
            Self::Cline => Self::Claude,
            Self::Adhoc => Self::Adhoc,
        }
    }

    /// Cycle backward (for launch panel). Adhoc is not launchable.
    pub fn prev(&self) -> Self {
        match self {
            Self::Claude => Self::Cline,
            Self::Gemini => Self::Claude,
            Self::Codex => Self::Gemini,
            Self::OpenCode => Self::Codex,
            Self::Kilo => Self::OpenCode,
            Self::Cline => Self::Kilo,
            Self::Adhoc => Self::Adhoc,
        }
    }
}

#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub tool: Tool,
    pub status: AgentStatus,
    pub status_context: String,
    pub status_detail: String,
    pub created_at: f64,
    pub status_time: f64,
    pub last_heartbeat: f64,
    pub has_tcp: bool,
    pub directory: String,
    pub tag: String,
    pub project: String,
    pub unread: usize,
    pub last_event_id: Option<u64>,
    pub device_name: Option<String>,
    pub sync_age: Option<String>,
    pub headless: bool,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub terminal_preset: Option<String>,
}

impl Agent {
    /// Full display name: `{tag}-{name}` if tagged, else `{name}`.
    /// Appends `:{device}` for remote agents.
    pub fn display_name(&self) -> String {
        let base = if self.tag.is_empty() {
            self.name.clone()
        } else {
            format!("{}-{}", self.tag, self.name)
        };
        if let Some(ref device) = self.device_name {
            format!("{}:{}", base, device)
        } else {
            base
        }
    }

    pub fn is_remote(&self) -> bool {
        self.device_name.is_some()
    }

    pub fn context_display(&self) -> String {
        // Strip known internal prefixes for cleaner display
        let ctx = strip_context_prefix(&self.status_context);
        if self.status_detail.is_empty() {
            ctx.to_string()
        } else {
            format!("{}: {}", ctx, self.status_detail)
        }
    }

    /// Time since last status change (falls back to created_at).
    pub fn age_display(&self) -> String {
        let now = epoch_now();
        let base = if self.status_time > 0.0 {
            self.status_time
        } else {
            self.created_at
        };
        format_duration_short((now - base).max(0.0) as u64)
    }

    /// Age since creation (total session duration).
    pub fn created_display(&self) -> String {
        format_duration_short((epoch_now() - self.created_at).max(0.0) as u64)
    }

    /// True when PTY delivery is gate-blocked (daemon wrote a `tui:*` context).
    pub fn is_pty_blocked(&self) -> bool {
        self.status_context.starts_with("tui:")
    }
}

/// Strip known internal prefixes from status_context for display.
/// e.g. "tool:Bash" → "Bash", "tui:not-ready" → "not-ready"
fn strip_context_prefix(ctx: &str) -> &str {
    const PREFIXES: &[&str] = &["tool:", "deliver:", "approved:", "exit:", "stale:", "tui:"];
    for p in PREFIXES {
        if let Some(rest) = ctx.strip_prefix(p) {
            return rest;
        }
    }
    ctx
}

/// Format a duration in seconds as a short human-readable string (e.g. "now", "5s", "3m", "2h", "1d").
pub fn format_duration_short(secs: u64) -> String {
    crate::shared::time::format_age(secs as i64)
}

/// Current Unix epoch as f64.
pub fn epoch_now() -> f64 {
    crate::shared::time::now_epoch_f64()
}

/// Format timestamp as "HH:MM" in local timezone.
pub fn format_time(t: f64) -> String {
    use chrono::{Local, TimeZone};
    if let Some(dt) = Local.timestamp_opt(t as i64, 0).single() {
        return dt.format("%H:%M").to_string();
    }
    "--:--".into()
}

// ── Message ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MessageScope {
    Broadcast,
    Mentions,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SenderKind {
    External,
    Instance,
    System,
}

#[derive(Clone)]
pub struct Message {
    pub event_id: u64,
    pub sender: String,
    pub recipients: Vec<String>,
    pub body: String,
    pub time: f64,
    #[allow(dead_code)] // read in tests
    pub delivered: Vec<String>,
    #[allow(dead_code)] // read in tests
    pub scope: MessageScope,
    pub sender_kind: SenderKind,
    pub intent: Option<String>,
    pub reply_to: Option<u64>,
}

impl Message {
    pub fn is_system(&self) -> bool {
        self.sender_kind == SenderKind::System
    }
}

// ── Unified Event (replaces ToolEvent + ActivityEvent) ───────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    Tool,
    Activity(ActivityKind),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActivityKind {
    Started,
    Active,
    Listening,
    Stopped,
    Blocked,
    StateChange,
}

#[derive(Clone)]
pub struct Event {
    pub row_id: u64, // DB row id — monotonic, used as ejection watermark
    pub agent: String,
    pub time: f64,
    pub kind: EventKind,
    pub tool: String,           // for Tool events: "Read", "Edit", etc.
    pub detail: String,         // for Tool: target path; for Activity: description
    pub sub_lines: Vec<String>, // extra detail lines (e.g. stopped snapshot)
}

#[derive(Clone)]
pub struct OrphanProcess {
    pub pid: u32,
    pub tool: Tool,
    pub names: Vec<String>,
    pub launched_at: f64,
    pub directory: String,
}

impl OrphanProcess {
    pub fn age_display(&self) -> String {
        format_duration_short((epoch_now() - self.launched_at).max(0.0) as u64)
    }

    /// Display string for the names column (e.g. "nova, kira" or "—").
    pub fn names_display(&self) -> String {
        if self.names.is_empty() {
            "\u{2014}".into()
        } else {
            self.names.join(", ")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorTarget {
    None,
    Agent(usize),
    RemoteHeader,
    RemoteAgent(usize),
    StoppedHeader,
    StoppedAgent(usize),
    OrphanHeader,
    Orphan(usize),
}

pub struct Flash {
    pub text: String,
    pub style: Style,
    pub expires_at: std::time::Instant,
}

impl Flash {
    pub fn new(text: String, style: Style) -> Self {
        Self {
            text,
            style,
            expires_at: std::time::Instant::now() + std::time::Duration::from_millis(1600),
        }
    }

    pub fn is_expired(&self) -> bool {
        std::time::Instant::now() >= self.expires_at
    }
}

pub const RELAY_ACTIONS: &[&str] = &["status", "new", "connect"];

// ── Overlay (Navigate-mode text inputs) ──────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayKind {
    Search,
    Command,
    Tag,
    Project,
    ProjectFilter,
}

pub struct Overlay {
    pub kind: OverlayKind,
    pub input: String,
    pub cursor: usize,
    /// For Tag overlay: which agents are being tagged.
    pub targets: Vec<String>,
    /// For Command overlay: browsable suggestion palette.
    pub palette: Option<CommandPalette>,
}

impl Overlay {
    pub fn new(kind: OverlayKind) -> Self {
        Self {
            kind,
            input: String::new(),
            cursor: 0,
            targets: Vec::new(),
            palette: None,
        }
    }

    pub fn with(kind: OverlayKind, targets: Vec<String>, input: String) -> Self {
        let cursor = input.len();
        Self {
            kind,
            input,
            cursor,
            targets,
            palette: None,
        }
    }

    pub fn command_with_palette(palette: CommandPalette) -> Self {
        Self {
            kind: OverlayKind::Command,
            input: String::new(),
            cursor: 0,
            targets: Vec::new(),
            palette: Some(palette),
        }
    }
}

// ── Command Palette ─────────────────────────────────────────────

#[derive(Clone)]
pub struct CommandSuggestion {
    pub command: String,
    pub description: &'static str,
}

/// Browsable, filterable suggestion list for the Command overlay.
pub struct CommandPalette {
    pub all: Vec<CommandSuggestion>,
    /// Indices into `all` matching current input filter.
    pub filtered: Vec<usize>,
    /// Position within `filtered`. None = no highlight (free-text input).
    pub cursor: Option<usize>,
}

impl CommandPalette {
    pub fn new(all: Vec<CommandSuggestion>) -> Self {
        let filtered = (0..all.len()).collect();
        Self {
            all,
            filtered,
            cursor: None,
        }
    }

    /// Rebuild filtered indices based on input text (case-insensitive substring).
    pub fn filter(&mut self, input: &str) {
        let q = input.to_lowercase();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                q.is_empty()
                    || s.command.to_lowercase().contains(&q)
                    || s.description.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        // Clamp cursor into bounds
        if let Some(c) = self.cursor {
            if c >= self.filtered.len() {
                self.cursor = if self.filtered.is_empty() {
                    None
                } else {
                    Some(self.filtered.len() - 1)
                };
            }
        }
    }

    pub fn cursor_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        match self.cursor {
            None => self.cursor = Some(0),
            Some(c) if c + 1 < self.filtered.len() => self.cursor = Some(c + 1),
            _ => {}
        }
    }

    pub fn cursor_up(&mut self) {
        match self.cursor {
            Some(0) => self.cursor = None,
            Some(c) => self.cursor = Some(c - 1),
            None => {}
        }
    }

    pub fn selected(&self) -> Option<&CommandSuggestion> {
        self.cursor
            .and_then(|c| self.filtered.get(c))
            .and_then(|&idx| self.all.get(idx))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LaunchField {
    Tool,
    Count,
    Tag,
    Project,
    Headless,
    Terminal,
}

pub struct LaunchState {
    pub tool: Tool,
    pub count: u8,
    pub options_cursor: Option<LaunchField>,
    pub tag: String,
    pub project: String,
    pub headless: bool,
    pub terminal: usize,
    pub terminal_presets: Vec<String>,
    pub editing: Option<LaunchField>,
    pub edit_cursor: usize,
    pub edit_snapshot: Option<String>,
}

impl Default for LaunchState {
    fn default() -> Self {
        Self::new()
    }
}

impl LaunchState {
    pub fn new() -> Self {
        use crate::tui::db::{get_available_presets, read_launch_defaults};
        let defaults = read_launch_defaults();
        let presets = get_available_presets();
        let terminal_idx = presets
            .iter()
            .position(|p| p == &defaults.terminal)
            .unwrap_or(0);
        Self {
            tool: Tool::Claude,
            count: 1,
            options_cursor: None,
            tag: defaults.tag,
            project: defaults.project,
            headless: false,
            terminal: terminal_idx,
            terminal_presets: presets,
            editing: None,
            edit_cursor: 0,
            edit_snapshot: None,
        }
    }

    /// Height of the inline panel.
    pub fn panel_height(&self) -> u16 {
        if self.tool == Tool::Claude {
            // tool + count + tag + project + headless + terminal
            6
        } else {
            // tool + count + tag + project + terminal
            5
        }
    }

    /// Ordered navigable fields in the settings area.
    pub fn settings_fields(&self) -> &'static [LaunchField] {
        if self.tool == Tool::Claude {
            &[
                LaunchField::Tool,
                LaunchField::Count,
                LaunchField::Tag,
                LaunchField::Project,
                LaunchField::Headless,
                LaunchField::Terminal,
            ]
        } else {
            &[
                LaunchField::Tool,
                LaunchField::Count,
                LaunchField::Tag,
                LaunchField::Project,
                LaunchField::Terminal,
            ]
        }
    }

    /// Move cursor up. At top, wraps to None (input focus).
    pub fn cursor_up(&mut self) {
        // Auto-save tag when navigating away
        if self.editing.is_some() {
            self.stop_editing();
        }
        match self.options_cursor {
            None => {
                let fields = self.settings_fields();
                self.options_cursor = fields.last().copied();
            }
            Some(current) => {
                let fields = self.settings_fields();
                if let Some(pos) = fields.iter().position(|f| *f == current) {
                    if pos == 0 {
                        self.options_cursor = None;
                    } else {
                        self.options_cursor = Some(fields[pos - 1]);
                    }
                }
            }
        }
        self.auto_edit_text_field();
    }

    /// Move cursor down. At bottom, wraps to None (input focus).
    pub fn cursor_down(&mut self) {
        // Auto-save tag when navigating away
        if self.editing.is_some() {
            self.stop_editing();
        }
        match self.options_cursor {
            None => {
                let fields = self.settings_fields();
                self.options_cursor = fields.first().copied();
            }
            Some(current) => {
                let fields = self.settings_fields();
                if let Some(pos) = fields.iter().position(|f| *f == current) {
                    if pos + 1 >= fields.len() {
                        self.options_cursor = None;
                    } else {
                        self.options_cursor = Some(fields[pos + 1]);
                    }
                }
            }
        }
        self.auto_edit_text_field();
    }

    /// Auto-enter editing mode when landing on a text field (Tag, Project).
    fn auto_edit_text_field(&mut self) {
        if self.is_text_field() && self.editing.is_none() {
            self.start_editing();
        }
    }

    pub fn adjust_left(&mut self) {
        match self.options_cursor {
            Some(LaunchField::Tool) => self.tool = self.tool.prev(),
            Some(LaunchField::Count) => {
                if self.count > 1 {
                    self.count -= 1;
                }
            }
            Some(LaunchField::Terminal) => {
                if self.terminal == 0 {
                    self.terminal = self.terminal_presets.len().saturating_sub(1);
                } else {
                    self.terminal -= 1;
                }
            }
            _ => {}
        }
    }

    pub fn adjust_right(&mut self) {
        match self.options_cursor {
            Some(LaunchField::Tool) => self.tool = self.tool.next(),
            Some(LaunchField::Count) => {
                if self.count < 99 {
                    self.count += 1;
                }
            }
            Some(LaunchField::Terminal) => {
                self.terminal = (self.terminal + 1) % self.terminal_presets.len();
            }
            _ => {}
        }
    }

    pub fn toggle_or_select(&mut self) {
        if self.options_cursor == Some(LaunchField::Headless) {
            self.headless = !self.headless;
        }
    }

    pub fn is_text_field(&self) -> bool {
        matches!(
            self.options_cursor,
            Some(LaunchField::Tag) | Some(LaunchField::Project)
        )
    }

    pub fn start_editing(&mut self) {
        if self.is_text_field() {
            let field = self.options_cursor.unwrap();
            let val = self.field_value(field);
            let len = val.len();
            let snapshot = val.to_string();
            self.editing = Some(field);
            self.edit_cursor = len;
            self.edit_snapshot = Some(snapshot);
        }
    }

    pub fn stop_editing(&mut self) {
        self.editing = None;
        self.edit_cursor = 0;
        self.edit_snapshot = None;
    }

    pub fn cancel_editing(&mut self) {
        if let (Some(field), Some(snapshot)) = (self.editing, self.edit_snapshot.take()) {
            if let Some(s) = self.field_value_mut(field) {
                *s = snapshot;
            }
        }
        self.editing = None;
        self.edit_cursor = 0;
    }

    pub fn edit_cursor_left(&mut self) {
        match self.editing {
            Some(LaunchField::Tag) => cursor_left(&self.tag, &mut self.edit_cursor),
            Some(LaunchField::Project) => cursor_left(&self.project, &mut self.edit_cursor),
            _ => {}
        }
    }

    pub fn edit_cursor_right(&mut self) {
        match self.editing {
            Some(LaunchField::Tag) => cursor_right(&self.tag, &mut self.edit_cursor),
            Some(LaunchField::Project) => cursor_right(&self.project, &mut self.edit_cursor),
            _ => {}
        }
    }

    pub fn field_value(&self, field: LaunchField) -> &str {
        match field {
            LaunchField::Tag => &self.tag,
            LaunchField::Project => &self.project,
            _ => "",
        }
    }

    pub fn field_value_mut(&mut self, field: LaunchField) -> Option<&mut String> {
        match field {
            LaunchField::Tag => Some(&mut self.tag),
            LaunchField::Project => Some(&mut self.project),
            _ => None,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        match self.editing {
            Some(LaunchField::Tag) => insert_at(&mut self.tag, &mut self.edit_cursor, c),
            Some(LaunchField::Project) => insert_at(&mut self.project, &mut self.edit_cursor, c),
            _ => {}
        }
    }

    pub fn delete_char(&mut self) {
        match self.editing {
            Some(LaunchField::Tag) => delete_back(&mut self.tag, &mut self.edit_cursor),
            Some(LaunchField::Project) => delete_back(&mut self.project, &mut self.edit_cursor),
            _ => {}
        }
    }

    pub fn delete_word(&mut self) {
        match self.editing {
            Some(LaunchField::Tag) => delete_word_back(&mut self.tag, &mut self.edit_cursor),
            Some(LaunchField::Project) => delete_word_back(&mut self.project, &mut self.edit_cursor),
            _ => {}
        }
    }

    pub fn delete_to_start(&mut self) {
        match self.editing {
            Some(LaunchField::Tag) => {
                crate::tui::model::delete_to_start(&mut self.tag, &mut self.edit_cursor);
            }
            Some(LaunchField::Project) => {
                crate::tui::model::delete_to_start(&mut self.project, &mut self.edit_cursor);
            }
            _ => {}
        }
    }
}

// ── Shared text-input helpers ─────────────────────────────────────

/// Move cursor one grapheme cluster left.
pub fn cursor_left(s: &str, cursor: &mut usize) {
    if *cursor > 0 {
        let preceding = &s[..*cursor];
        if let Some(g) = preceding.graphemes(true).next_back() {
            *cursor -= g.len();
        }
    }
}

/// Move cursor one grapheme cluster right.
pub fn cursor_right(s: &str, cursor: &mut usize) {
    if *cursor < s.len() {
        let remaining = &s[*cursor..];
        if let Some(g) = remaining.graphemes(true).next() {
            *cursor += g.len();
        }
    }
}

/// Delete the grapheme cluster before the cursor.
pub fn delete_back(s: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        let preceding = &s[..*cursor];
        if let Some(g) = preceding.graphemes(true).next_back() {
            let start = *cursor - g.len();
            s.drain(start..*cursor);
            *cursor = start;
        }
    }
}

/// Delete the word before the cursor (Ctrl+W).
pub fn delete_word_back(s: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let before = &s[..*cursor];
    let trimmed = before.trim_end_matches(' ');
    if trimmed.is_empty() {
        s.drain(0..*cursor);
        *cursor = 0;
        return;
    }
    let word_start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
    s.drain(word_start..*cursor);
    *cursor = word_start;
}

/// Delete everything before the cursor (Ctrl+U).
pub fn delete_to_start(s: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        s.drain(..*cursor);
        *cursor = 0;
    }
}

/// Insert a character at cursor position.
pub fn insert_at(s: &mut String, cursor: &mut usize, c: char) {
    s.insert(*cursor, c);
    *cursor += c.len_utf8();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent(name: &str) -> Agent {
        Agent {
            name: name.into(),
            tool: Tool::Claude,
            status: AgentStatus::Active,
            status_context: String::new(),
            status_detail: String::new(),
            created_at: 1000.0,
            status_time: 1000.0,
            last_heartbeat: 1000.0,
            has_tcp: true,
            directory: String::new(),
            tag: String::new(),
            project: String::new(),
            unread: 0,
            last_event_id: None,
            device_name: None,
            sync_age: None,
            headless: false,
            session_id: None,
            pid: None,
            terminal_preset: None,
        }
    }

    // ── format_duration_short ─────────────────────────────────────

    #[test]
    fn duration_zero_is_now() {
        assert_eq!(format_duration_short(0), "now");
    }

    #[test]
    fn duration_seconds_boundary() {
        assert_eq!(format_duration_short(1), "1s");
        assert_eq!(format_duration_short(59), "59s");
    }

    #[test]
    fn duration_minutes_boundary() {
        assert_eq!(format_duration_short(60), "1m");
        assert_eq!(format_duration_short(119), "1m"); // truncates, not rounds
        assert_eq!(format_duration_short(3599), "59m");
    }

    #[test]
    fn duration_hours_boundary() {
        assert_eq!(format_duration_short(3600), "1h");
        assert_eq!(format_duration_short(86399), "23h");
    }

    #[test]
    fn duration_days() {
        assert_eq!(format_duration_short(86400), "1d");
        assert_eq!(format_duration_short(172800), "2d");
    }

    // ── cursor_left / cursor_right ────────────────────────────────

    #[test]
    fn cursor_left_ascii() {
        let s = "abc";
        let mut c = 3;
        cursor_left(s, &mut c);
        assert_eq!(c, 2);
        cursor_left(s, &mut c);
        assert_eq!(c, 1);
    }

    #[test]
    fn cursor_left_at_zero_is_noop() {
        let mut c = 0;
        cursor_left("abc", &mut c);
        assert_eq!(c, 0);
    }

    #[test]
    fn cursor_right_ascii() {
        let s = "abc";
        let mut c = 0;
        cursor_right(s, &mut c);
        assert_eq!(c, 1);
        cursor_right(s, &mut c);
        assert_eq!(c, 2);
    }

    #[test]
    fn cursor_right_at_end_is_noop() {
        let s = "abc";
        let mut c = 3;
        cursor_right(s, &mut c);
        assert_eq!(c, 3);
    }

    #[test]
    fn cursor_moves_by_grapheme_multibyte() {
        let s = "aéb"; // é is 2 bytes
        let mut c = 0;
        cursor_right(s, &mut c);
        assert_eq!(c, 1); // past 'a'
        cursor_right(s, &mut c);
        assert_eq!(c, 3); // past 'é' (2 bytes)
        cursor_left(s, &mut c);
        assert_eq!(c, 1); // back to before 'é'
    }

    #[test]
    fn cursor_on_empty_string() {
        let mut c = 0;
        cursor_left("", &mut c);
        assert_eq!(c, 0);
        cursor_right("", &mut c);
        assert_eq!(c, 0);
    }

    // ── delete_back ───────────────────────────────────────────────

    #[test]
    fn delete_back_ascii() {
        let mut s = "abc".to_string();
        let mut c = 3;
        delete_back(&mut s, &mut c);
        assert_eq!(s, "ab");
        assert_eq!(c, 2);
    }

    #[test]
    fn delete_back_at_zero_is_noop() {
        let mut s = "abc".to_string();
        let mut c = 0;
        delete_back(&mut s, &mut c);
        assert_eq!(s, "abc");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_back_multibyte() {
        let mut s = "aé".to_string();
        let mut c = s.len(); // 3
        delete_back(&mut s, &mut c);
        assert_eq!(s, "a");
        assert_eq!(c, 1);
    }

    // ── delete_word_back ──────────────────────────────────────────

    #[test]
    fn delete_word_back_single_word() {
        let mut s = "hello".to_string();
        let mut c = 5;
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_word_back_two_words() {
        let mut s = "hello world".to_string();
        let mut c = 11;
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "hello ");
        assert_eq!(c, 6);
    }

    #[test]
    fn delete_word_back_trailing_spaces() {
        let mut s = "hello   ".to_string();
        let mut c = 8;
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_word_back_at_zero_is_noop() {
        let mut s = "hello".to_string();
        let mut c = 0;
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "hello");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_word_back_mid_string() {
        let mut s = "one two three".to_string();
        let mut c = 7; // after "two"
        delete_word_back(&mut s, &mut c);
        assert_eq!(s, "one  three");
        assert_eq!(c, 4);
    }

    // ── delete_to_start ───────────────────────────────────────────

    #[test]
    fn delete_to_start_from_middle() {
        let mut s = "hello world".to_string();
        let mut c = 5;
        delete_to_start(&mut s, &mut c);
        assert_eq!(s, " world");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_to_start_at_zero_is_noop() {
        let mut s = "hello".to_string();
        let mut c = 0;
        delete_to_start(&mut s, &mut c);
        assert_eq!(s, "hello");
        assert_eq!(c, 0);
    }

    // ── insert_at ─────────────────────────────────────────────────

    #[test]
    fn insert_at_start() {
        let mut s = "bc".to_string();
        let mut c = 0;
        insert_at(&mut s, &mut c, 'a');
        assert_eq!(s, "abc");
        assert_eq!(c, 1);
    }

    #[test]
    fn insert_at_end() {
        let mut s = "ab".to_string();
        let mut c = 2;
        insert_at(&mut s, &mut c, 'c');
        assert_eq!(s, "abc");
        assert_eq!(c, 3);
    }

    #[test]
    fn insert_multibyte_char() {
        let mut s = "ab".to_string();
        let mut c = 1;
        insert_at(&mut s, &mut c, 'é');
        assert_eq!(s, "aéb");
        assert_eq!(c, 3); // é is 2 bytes
    }

    // ── Agent::display_name ───────────────────────────────────────

    #[test]
    fn display_name_plain() {
        let a = test_agent("nova");
        assert_eq!(a.display_name(), "nova");
    }

    #[test]
    fn display_name_with_tag() {
        let mut a = test_agent("nova");
        a.tag = "dev".into();
        assert_eq!(a.display_name(), "dev-nova");
    }

    #[test]
    fn display_name_remote() {
        let mut a = test_agent("nova");
        a.device_name = Some("BOXE".into());
        assert_eq!(a.display_name(), "nova:BOXE");
    }

    #[test]
    fn display_name_tag_and_remote() {
        let mut a = test_agent("nova");
        a.tag = "dev".into();
        a.device_name = Some("BOXE".into());
        assert_eq!(a.display_name(), "dev-nova:BOXE");
    }

    // ── Agent::context_display ────────────────────────────────────

    #[test]
    fn context_display_strips_prefix() {
        let mut a = test_agent("nova");
        a.status_context = "tool:Bash".into();
        assert_eq!(a.context_display(), "Bash");
    }

    #[test]
    fn context_display_with_detail() {
        let mut a = test_agent("nova");
        a.status_context = "tool:Read".into();
        a.status_detail = "/tmp/foo.rs".into();
        assert_eq!(a.context_display(), "Read: /tmp/foo.rs");
    }

    #[test]
    fn context_display_no_prefix() {
        let mut a = test_agent("nova");
        a.status_context = "listening".into();
        assert_eq!(a.context_display(), "listening");
    }

    #[test]
    fn context_display_all_prefixes_stripped() {
        for (input, expected) in [
            ("tool:Edit", "Edit"),
            ("deliver:pending", "pending"),
            ("approved:yes", "yes"),
            ("exit:0", "0"),
            ("stale:active", "active"),
            ("tui:not-ready", "not-ready"),
        ] {
            let mut a = test_agent("nova");
            a.status_context = input.into();
            assert_eq!(
                a.context_display(),
                expected,
                "prefix not stripped from {input}"
            );
        }
    }

    // ── Agent::is_pty_blocked ─────────────────────────────────────

    #[test]
    fn pty_blocked_with_tui_prefix() {
        let mut a = test_agent("nova");
        a.status_context = "tui:not-ready".into();
        assert!(a.is_pty_blocked());
    }

    #[test]
    fn pty_not_blocked_without_tui_prefix() {
        let mut a = test_agent("nova");
        a.status_context = "tool:Bash".into();
        assert!(!a.is_pty_blocked());
    }

    // ── Tool cycling ──────────────────────────────────────────────

    #[test]
    fn tool_next_cycles_through_launchable() {
        assert_eq!(Tool::Claude.next(), Tool::Gemini);
        assert_eq!(Tool::Gemini.next(), Tool::Codex);
        assert_eq!(Tool::Codex.next(), Tool::OpenCode);
        assert_eq!(Tool::OpenCode.next(), Tool::Kilo);
        assert_eq!(Tool::Kilo.next(), Tool::Cline);
        assert_eq!(Tool::Cline.next(), Tool::Claude);
    }

    #[test]
    fn tool_prev_cycles_backward() {
        assert_eq!(Tool::Claude.prev(), Tool::Cline);
        assert_eq!(Tool::Cline.prev(), Tool::Kilo);
        assert_eq!(Tool::Kilo.prev(), Tool::OpenCode);
        assert_eq!(Tool::OpenCode.prev(), Tool::Codex);
        assert_eq!(Tool::Codex.prev(), Tool::Gemini);
        assert_eq!(Tool::Gemini.prev(), Tool::Claude);
    }

    #[test]
    fn tool_adhoc_does_not_cycle() {
        assert_eq!(Tool::Adhoc.next(), Tool::Adhoc);
        assert_eq!(Tool::Adhoc.prev(), Tool::Adhoc);
    }

    // ── CommandPalette ────────────────────────────────────────────

    fn test_palette() -> CommandPalette {
        CommandPalette::new(vec![
            CommandSuggestion {
                command: "list".into(),
                description: "Show agents",
            },
            CommandSuggestion {
                command: "kill".into(),
                description: "Stop agent",
            },
            CommandSuggestion {
                command: "send".into(),
                description: "Send message",
            },
        ])
    }

    #[test]
    fn palette_filter_narrows_by_command() {
        let mut p = test_palette();
        p.filter("ki");
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.all[p.filtered[0]].command, "kill");
    }

    #[test]
    fn palette_filter_matches_description() {
        let mut p = test_palette();
        p.filter("agent");
        assert_eq!(p.filtered.len(), 2); // "Show agents" and "Stop agent"
    }

    #[test]
    fn palette_filter_empty_shows_all() {
        let mut p = test_palette();
        p.filter("");
        assert_eq!(p.filtered.len(), 3);
    }

    #[test]
    fn palette_cursor_down_from_none() {
        let mut p = test_palette();
        assert_eq!(p.cursor, None);
        p.cursor_down();
        assert_eq!(p.cursor, Some(0));
    }

    #[test]
    fn palette_cursor_up_to_none() {
        let mut p = test_palette();
        p.cursor = Some(0);
        p.cursor_up();
        assert_eq!(p.cursor, None);
    }

    #[test]
    fn palette_cursor_clamps_on_filter() {
        let mut p = test_palette();
        p.cursor = Some(2); // last item
        p.filter("list"); // only 1 result
        assert_eq!(p.cursor, Some(0)); // clamped
    }

    #[test]
    fn palette_selected_returns_highlighted() {
        let mut p = test_palette();
        p.cursor_down();
        let sel = p.selected().unwrap();
        assert_eq!(sel.command, "list");
    }

    #[test]
    fn palette_selected_none_when_no_cursor() {
        let p = test_palette();
        assert!(p.selected().is_none());
    }

    // ── LaunchState navigation ────────────────────────────────────

    fn test_launch() -> LaunchState {
        LaunchState {
            tool: Tool::Claude,
            count: 1,
            options_cursor: None,
            tag: String::new(),
            project: String::new(),
            headless: false,
            terminal: 0,
            terminal_presets: vec!["default".into(), "kitty".into()],
            editing: None,
            edit_cursor: 0,
            edit_snapshot: None,
        }
    }

    #[test]
    fn launch_panel_height_claude_vs_others() {
        let mut ls = test_launch();
        assert_eq!(ls.panel_height(), 6); // claude has headless field
        ls.tool = Tool::Gemini;
        assert_eq!(ls.panel_height(), 5);
    }

    #[test]
    fn launch_settings_fields_claude_has_headless() {
        let ls = test_launch();
        assert!(ls.settings_fields().contains(&LaunchField::Headless));
    }

    #[test]
    fn launch_settings_fields_gemini_no_headless() {
        let mut ls = test_launch();
        ls.tool = Tool::Gemini;
        assert!(!ls.settings_fields().contains(&LaunchField::Headless));
    }

    #[test]
    fn launch_cursor_down_from_none_goes_to_first() {
        let mut ls = test_launch();
        ls.cursor_down();
        assert_eq!(ls.options_cursor, Some(LaunchField::Tool));
    }

    #[test]
    fn launch_cursor_down_wraps_to_none() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Terminal); // last field
        ls.cursor_down();
        assert_eq!(ls.options_cursor, None);
    }

    #[test]
    fn launch_cursor_up_from_none_goes_to_last() {
        let mut ls = test_launch();
        ls.cursor_up();
        assert_eq!(ls.options_cursor, Some(LaunchField::Terminal));
    }

    #[test]
    fn launch_cursor_up_wraps_to_none() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Tool); // first field
        ls.cursor_up();
        assert_eq!(ls.options_cursor, None);
    }

    #[test]
    fn launch_count_bounds() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Count);
        ls.count = 1;
        ls.adjust_left();
        assert_eq!(ls.count, 1); // can't go below 1

        ls.count = 99;
        ls.adjust_right();
        assert_eq!(ls.count, 99); // can't go above 99
    }

    #[test]
    fn launch_tool_cycles_with_adjust() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Tool);
        assert_eq!(ls.tool, Tool::Claude);
        ls.adjust_right();
        assert_eq!(ls.tool, Tool::Gemini);
        ls.adjust_left();
        assert_eq!(ls.tool, Tool::Claude);
    }

    #[test]
    fn launch_terminal_wraps() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Terminal);
        assert_eq!(ls.terminal, 0);
        ls.adjust_left(); // wraps to last
        assert_eq!(ls.terminal, 1);
        ls.adjust_right(); // wraps to first
        assert_eq!(ls.terminal, 0);
    }

    #[test]
    fn launch_headless_toggles() {
        let mut ls = test_launch();
        ls.options_cursor = Some(LaunchField::Headless);
        assert!(!ls.headless);
        ls.toggle_or_select();
        assert!(ls.headless);
        ls.toggle_or_select();
        assert!(!ls.headless);
    }

    #[test]
    fn launch_tag_auto_edits() {
        let mut ls = test_launch();
        // Navigate to Tag field
        ls.options_cursor = Some(LaunchField::Count);
        ls.cursor_down(); // → Tag
        assert_eq!(ls.options_cursor, Some(LaunchField::Tag));
        assert_eq!(ls.editing, Some(LaunchField::Tag));
    }
}
