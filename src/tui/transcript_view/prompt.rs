//! A numbered choice prompt on a blocked agent's terminal screen (Claude's
//! `AskUserQuestion` or a permission prompt), and the keys that answer it.
//!
//! Claude writes a question to its transcript only after it is answered, so
//! the screen is the one place to read it while it waits. Measured on Claude
//! Code 2.x: a digit picks an option; one single-select question is answered
//! by it. With several questions or a multi-select one the prompt has tabs:
//! a single-select digit moves to the next tab, multi-select digits toggle
//! and → moves on, and the last tab submits on `1`. Options with previews
//! draw a preview box to their right; there a digit only moves the cursor,
//! and Enter picks the option under it.
use crate::commands::term::{ScreenCheck, ScreenStep};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenOption {
    pub number: usize,
    pub label: String,
    pub description: String,
    /// `Some` for a multi-select checkbox.
    pub checked: Option<bool>,
}

impl ScreenOption {
    /// Options that open a text box in the agent's terminal.
    pub fn needs_typing(&self) -> bool {
        let label = self.label.trim_end_matches('.');
        label == "Type something" || label == "Chat about this"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenPrompt {
    /// The tab row (`☒ fruit  ☐ drink  ✔ Submit`), when there is one.
    pub tabs: Option<String>,
    pub question: Vec<String>,
    pub options: Vec<ScreenOption>,
    /// Text that is on screen only while this prompt waits.
    anchor: String,
    /// `Some` when the options carry previews.
    preview: Option<Preview>,
}

/// The parts of a preview prompt that decide where Enter lands.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Preview {
    /// The option under the cursor.
    cursor: usize,
    /// The notes line after `Notes:`. While notes are open or written, a
    /// digit is typed into them.
    notes: Option<String>,
}

const RULE: char = '─';
const DASH: char = '╌';
const FOOTER: &str = "Esc to cancel";
const NOTES: &str = "Notes:";
const NOTES_EMPTY: &str = "press n to add notes";

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 8 && t.chars().all(|c| c == RULE)
}

/// The dashed lines a permission prompt draws around a command.
fn is_dashed(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 8 && t.chars().all(|c| c == DASH)
}

/// `❯ 1. label` or `  2. label`: (number, label, under the cursor).
fn option_line(line: &str) -> Option<(usize, &str, bool)> {
    let t = line.trim_start();
    let (cursor, t) = match t.strip_prefix('❯') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, t),
    };
    let dot = t.find(". ")?;
    let number: usize = t[..dot].parse().ok()?;
    (1..=9)
        .contains(&number)
        .then(|| (number, t[dot + 2..].trim(), cursor))
}

/// The display column of `corner` in `line`.
fn column_of(line: &str, corner: char) -> Option<usize> {
    line.find(corner).map(|i| line[..i].width())
}

/// The column the preview box starts at: its top corner is on option 1's
/// line, its bottom corner at the same column further down.
fn preview_column(block: &[String]) -> Option<usize> {
    let column = column_of(block.first()?, '┌')?;
    block[1..]
        .iter()
        .any(|l| column_of(l, '└') == Some(column))
        .then_some(column)
}

/// `line` left of display column `column`.
fn left_of(line: &str, column: usize) -> &str {
    let mut width = 0;
    for (i, c) in line.char_indices() {
        width += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width > column {
            return &line[..i];
        }
    }
    line
}

fn checkbox(label: &str) -> (Option<bool>, &str) {
    if let Some(rest) = label.strip_prefix("[ ] ") {
        (Some(false), rest)
    } else if let Some(rest) = label.strip_prefix("[✔] ") {
        (Some(true), rest)
    } else {
        (None, label)
    }
}

/// The prompt at the bottom of `lines`, if one is waiting there.
pub fn parse_screen(lines: &[String]) -> Option<ScreenPrompt> {
    let cursor = lines
        .iter()
        .rposition(|l| option_line(l).is_some_and(|(_, _, c)| c))?;
    // The cursor can sit on an option below a rule (`4. Chat about this`):
    // the block starts at option 1, and the question above its rule.
    let first = lines[..=cursor]
        .iter()
        .rposition(|l| option_line(l).is_some_and(|(n, _, _)| n == 1))?;
    // A permission prompt draws its command between dashed lines, and a
    // command line can look like a rule. The top is the nearest rule outside
    // a dashed pair. A dashed line above it means the pairs cannot be told
    // apart (a command can draw dashed lines too), so nothing is read.
    let mut inside = false;
    let mut top = None;
    for (i, line) in lines[..first].iter().enumerate().rev() {
        if is_dashed(line) {
            inside = !inside;
        } else if !inside && is_rule(line) {
            top = Some(i);
            break;
        }
    }
    let top = top?;
    if lines[..top].iter().any(|l| is_dashed(l)) {
        return None;
    }
    // The top is only taken outside a pair, so the count below it is even.
    // More than one pair means an unpaired line may have been paired off by
    // one above the real top, and the top may be a scrollback rule.
    if lines[top + 1..first]
        .iter()
        .filter(|l| is_dashed(l))
        .count()
        > 2
    {
        return None;
    }
    let top = top + 1;

    let mut tabs = None;
    let mut question = Vec::new();
    for line in &lines[top..first] {
        let t = line.trim();
        if t.is_empty() || is_dashed(t) {
            continue;
        }
        if t.contains("✔ Submit") && (t.starts_with('←') || t.ends_with('→')) {
            tabs = Some(t.trim_matches(['←', '→', ' ']).to_string());
        } else {
            question.push(t.to_string());
        }
    }

    let end = lines[first..]
        .iter()
        .position(|l| l.contains(FOOTER))
        .map_or(lines.len(), |i| first + i);
    let footer = end < lines.len();
    // Below a preview box only the options are read: the box, the notes
    // line and an unnumbered `Chat about this` are not descriptions.
    let column = preview_column(&lines[first..end]);
    let mut notes = None;
    let mut options: Vec<ScreenOption> = Vec::new();
    for line in &lines[first..end] {
        if column.is_some()
            && let Some(rest) = line.trim().strip_prefix(NOTES)
        {
            notes = Some(rest.trim().to_string());
            continue;
        }
        let line = column.map_or(line.as_str(), |c| left_of(line, c));
        let t = line.trim();
        if let Some((number, label, _)) = option_line(line) {
            let (checked, label) = checkbox(label);
            options.push(ScreenOption {
                number,
                label: label.to_string(),
                description: String::new(),
                checked,
            });
        } else if column.is_none()
            && !t.is_empty()
            && !is_rule(t)
            && t != "Submit"
            && let Some(last) = options.last_mut()
        {
            if !last.description.is_empty() {
                last.description.push(' ');
            }
            last.description.push_str(t);
        }
    }
    let numbered = options.iter().enumerate().all(|(i, o)| o.number == i + 1);
    if options.is_empty() || !numbered {
        return None;
    }
    let anchor = if footer {
        FOOTER.to_string()
    } else {
        lines[cursor]
            .trim()
            .trim_start_matches('❯')
            .trim_start()
            .to_string()
    };
    let preview = column.map(|_| Preview {
        cursor: option_line(&lines[cursor]).map_or(0, |(n, _, _)| n),
        notes,
    });
    Some(ScreenPrompt {
        tabs,
        question,
        options,
        anchor,
        preview,
    })
}

impl ScreenPrompt {
    pub fn is_multi(&self) -> bool {
        self.options.iter().any(|o| o.checked.is_some())
    }

    /// Notes are open or written on a preview prompt, so a digit would be
    /// typed into them.
    pub fn notes_in_use(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|p| p.notes.as_deref() != Some(NOTES_EMPTY))
    }

    /// Digit `n`: the steps that pick option `n`. `None` for an option that
    /// needs typing in the terminal, no such option, or notes in use.
    pub fn pick(&self, n: usize) -> Option<Vec<ScreenStep<ScreenPrompt>>> {
        let option = self.options.iter().find(|o| o.number == n)?;
        if option.needs_typing() {
            return None;
        }
        let digit = ScreenStep {
            expect: self.clone(),
            keys: vec![n.to_string().into_bytes()],
        };
        if self.preview.is_none() {
            return Some(vec![digit]);
        }
        if self.is_multi() || self.notes_in_use() {
            return None;
        }
        // The digit only moves the cursor. Enter picks the option under it,
        // so it goes out only once the screen shows the cursor on `n`.
        let mut moved = self.clone();
        if let Some(preview) = &mut moved.preview {
            preview.cursor = n;
        }
        Some(vec![
            digit,
            ScreenStep {
                expect: moved,
                keys: vec![b"\r".to_vec()],
            },
        ])
    }

    /// Enter on a multi-select question: move to the next tab.
    pub fn next(&self) -> Option<ScreenStep<ScreenPrompt>> {
        self.is_multi().then(|| ScreenStep {
            expect: self.clone(),
            keys: vec![b"\x1b[C".to_vec()],
        })
    }
}

/// The screen is read again just before a key goes out, and must parse to
/// this same prompt: question, options, tab row and anchor. A different
/// prompt that repeats this one's text in its own command does not match.
impl ScreenCheck for ScreenPrompt {
    fn shown_on(&self, lines: &[String]) -> bool {
        parse_screen(lines).as_ref() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(String::from).collect()
    }

    const TABBED: &str = "  ⎿  earlier output
────────────────────────────────────────────────────────────────────────────────
←  ☐ fruit  ☐ drink  ✔ Submit  →

Fruit?

❯ 1. Apple
     Pick an apple
  2. Pear
  3. Type something.
────────────────────────────────────────────────────────────────────────────────
  4. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel";

    #[test]
    fn reads_a_tabbed_question() {
        let p = parse_screen(&screen(TABBED)).unwrap();
        assert_eq!(p.tabs.as_deref(), Some("☐ fruit  ☐ drink  ✔ Submit"));
        assert_eq!(p.question, vec!["Fruit?"]);
        let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Apple", "Pear", "Type something.", "Chat about this"]
        );
        assert_eq!(p.options[0].description, "Pick an apple");
        assert!(!p.is_multi());
        assert_eq!(
            p.pick(2),
            Some(vec![ScreenStep {
                expect: p.clone(),
                keys: vec![b"2".to_vec()],
            }])
        );
        assert_eq!(p.anchor, FOOTER);
        assert!(p.pick(2).unwrap()[0].expect.shown_on(&screen(TABBED)));
        assert_eq!(p.pick(3), None, "typing happens in the terminal");
        assert_eq!(p.pick(4), None);
        assert_eq!(p.pick(7), None);
        assert_eq!(p.next(), None);
    }

    #[test]
    fn reads_multi_select_checkboxes() {
        let text = "────────────────────────────────
←  ☒ fruit  ☐ drink  ✔ Submit  →

Drink?

❯ 1. [✔] Water
         Still water
  2. [ ] Tea
  3. [ ] Type something
     Submit
────────────────────────────────
  4. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
        let p = parse_screen(&screen(text)).unwrap();
        assert!(p.is_multi());
        assert_eq!(p.options[0].checked, Some(true));
        assert_eq!(p.options[0].label, "Water");
        assert_eq!(p.options[1].checked, Some(false));
        assert_eq!(p.options[1].description, "");
        assert_eq!(p.pick(3), None);
        let next = p.next().unwrap();
        assert_eq!(next.keys, vec![b"\x1b[C".to_vec()]);
        // Checkbox state is part of the match: a toggle in the terminal
        // makes the step stale.
        assert!(next.expect.shown_on(&screen(text)));
        assert!(
            !next
                .expect
                .shown_on(&screen(&text.replace("[✔] Water", "[ ] Water")))
        );
        assert!(
            !next
                .expect
                .shown_on(&screen(&text.replace("[ ] Tea", "[✔] Tea")))
        );
    }

    #[test]
    fn reads_the_review_tab() {
        let text = "────────────────────────────────
←  ☒ fruit  ☒ drink  ✔ Submit  →

Review your answers

 ● Fruit?
   → Pear

Ready to submit your answers?

❯ 1. Submit answers
  2. Cancel";
        let p = parse_screen(&screen(text)).unwrap();
        assert_eq!(p.question.last().unwrap(), "Ready to submit your answers?");
        // No footer on this screen: the cursor line anchors it.
        let expect = p.pick(1).unwrap().remove(0).expect;
        assert_eq!(expect.tabs.as_deref(), Some("☒ fruit  ☒ drink  ✔ Submit"));
        assert!(expect.question.contains(&"Review your answers".to_string()));
        assert_eq!(expect.anchor, "1. Submit answers");
        assert!(expect.shown_on(&screen(text)));
        // The cursor moved to Cancel: no longer the screen the key was for.
        let moved = text
            .replace("❯ 1. Submit answers", "  1. Submit answers")
            .replace("  2. Cancel", "❯ 2. Cancel");
        assert!(!expect.shown_on(&screen(&moved)));
    }

    #[test]
    fn reads_a_permission_prompt() {
        let text = "────────────────────────────────
 Bash command
 Create probe-perm.txt in the working directory
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 touch probe-perm.txt
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 This command requires approval

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for touch commands in
      /private/tmp/scratchpad
   3. No

 Esc to cancel · Tab to amend";
        let p = parse_screen(&screen(text)).unwrap();
        assert_eq!(p.tabs, None);
        assert_eq!(
            p.question,
            vec![
                "Bash command",
                "Create probe-perm.txt in the working directory",
                "touch probe-perm.txt",
                "This command requires approval",
                "Do you want to proceed?",
            ]
        );
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[1].description, "/private/tmp/scratchpad");
        // The command line is matched, not only the generic first line.
        let expect = p.pick(1).unwrap().remove(0).expect;
        assert!(expect.shown_on(&screen(text)));
        let other = text.replace(" touch probe-perm.txt\n", " rm -rf build\n");
        assert_ne!(other, text);
        assert!(!expect.shown_on(&screen(&other)));
    }

    /// `PERMISSION` with `command` in place of its one command line.
    fn permission_with(command: &str) -> String {
        const PERMISSION: &str = "  ⎿  earlier output
────────────────────────────────
 Bash command
 Run the build
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
COMMAND
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 This command requires approval

 Do you want to proceed?
 ❯ 1. Yes
   2. No

 Esc to cancel · Tab to amend";
        PERMISSION.replace("COMMAND", command)
    }

    /// SEC-002: a rule-like line inside the command is not the prompt's
    /// top, so no command line is left out of what the user approves.
    #[test]
    fn a_rule_inside_the_command_keeps_the_whole_command() {
        let text = permission_with(
            " make clean
 ────────────────────────────────
 make build",
        );
        let p = parse_screen(&screen(&text)).unwrap();
        assert_eq!(
            p.question,
            vec![
                "Bash command",
                "Run the build",
                "make clean",
                "────────────────────────────────",
                "make build",
                "This command requires approval",
                "Do you want to proceed?",
            ]
        );
        // The line above the rule is part of the match as well.
        let other = text.replace(" make clean\n", " make other\n");
        assert!(!p.pick(1).unwrap()[0].expect.shown_on(&screen(&other)));
    }

    /// SEC-002: when the command draws its own rule and dashed line, the
    /// prompt's bounds are not known, and the prompt cannot be answered
    /// from the TUI.
    #[test]
    fn a_command_that_draws_prompt_lines_is_not_read() {
        // A rule and a dashed line that look like the start of a prompt.
        let forged = permission_with(
            " make clean
────────────────────────────────
 Bash command
 Run the build
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 make build",
        );
        assert_eq!(parse_screen(&screen(&forged)), None);
        // Dashed lines that do not pair up.
        let unpaired = permission_with(
            " make build
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌",
        );
        assert_eq!(parse_screen(&screen(&unpaired)), None);
        // An unpaired dashed line with a rule inside it.
        let unpaired_rule = permission_with(
            " make clean
────────────────────────────────
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 make build",
        );
        assert_eq!(parse_screen(&screen(&unpaired_rule)), None);
        // An unpaired dashed line in the command, paired off by one in the
        // scrollback: the top would land on a scrollback rule.
        let scrollback = format!(
            "  ⎿  older output
────────────────────────────────
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
{}",
            permission_with(
                " make build
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌"
            )
        );
        assert_eq!(parse_screen(&screen(&scrollback)), None);
        // The plain prompt still reads.
        assert!(parse_screen(&screen(&permission_with(" make build"))).is_some());
    }

    #[test]
    fn reads_with_the_cursor_below_the_rule() {
        let text = TABBED
            .replace("❯ 1. Apple", "  1. Apple")
            .replace("  4. Chat about this", "❯ 4. Chat about this");
        let p = parse_screen(&screen(&text)).unwrap();
        assert_eq!(p.question, vec!["Fruit?"]);
        assert_eq!(p.options.len(), 4);
        assert_eq!(p.options[0].description, "Pick an apple");
        assert_eq!(p.pick(1).unwrap()[0].expect.anchor, FOOTER);
    }

    /// Measured on Claude Code 2.1.309: two questions whose options carry
    /// previews, on the first tab.
    const PREVIEW: &str = "  ⎿  earlier output

────────────────────────────────────────────────────────────────────────────────
←  ☐ Wind unit  ☐ Temp unit  ✔ Submit  →

Which unit should the app use for wind speed?

❯ 1. km/h                         ┌──────────────────────────────────────────┐
  2. m/s                          │ Wind: 12 km/h NE, gusts up to 20 km/h    │
  3. mph                          └──────────────────────────────────────────┘

                                  Notes: press n to add notes

────────────────────────────────────────────────────────────────────────────────
  Chat about this

Enter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions ·
Esc to cancel";

    /// `PREVIEW` with the cursor on option 2, its preview redrawn.
    fn preview_on_two() -> String {
        PREVIEW
            .replace("❯ 1. km/h", "  1. km/h")
            .replace("  2. m/s ", "❯ 2. m/s ")
            .replace(
                "│ Wind: 12 km/h NE, gusts up to 20 km/h    │",
                "│ Wind: 3.3 m/s NE, gusts up to 5.6 m/s    │",
            )
    }

    #[test]
    fn reads_options_beside_a_preview() {
        let p = parse_screen(&screen(PREVIEW)).unwrap();
        assert_eq!(
            p.tabs.as_deref(),
            Some("☐ Wind unit  ☐ Temp unit  ✔ Submit")
        );
        assert_eq!(
            p.question,
            vec!["Which unit should the app use for wind speed?"]
        );
        let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["km/h", "m/s", "mph"]);
        assert!(p.options.iter().all(|o| o.description.is_empty()));
        assert!(!p.notes_in_use());
    }

    #[test]
    fn a_preview_pick_sends_enter_once_the_cursor_is_on_it() {
        let p = parse_screen(&screen(PREVIEW)).unwrap();
        let steps = p.pick(2).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].keys, vec![b"2".to_vec()]);
        assert_eq!(steps[1].keys, vec![b"\r".to_vec()]);
        assert!(steps[0].expect.shown_on(&screen(PREVIEW)));
        // Enter waits for the cursor on 2: not before the digit lands.
        assert!(!steps[1].expect.shown_on(&screen(PREVIEW)));
        assert!(steps[1].expect.shown_on(&screen(&preview_on_two())));
        // Nor on another option.
        let on_three = PREVIEW
            .replace("❯ 1. km/h", "  1. km/h")
            .replace("  3. mph ", "❯ 3. mph ");
        assert!(!steps[1].expect.shown_on(&screen(&on_three)));
        // The option under the cursor already: the digit changes nothing.
        let steps = p.pick(1).unwrap();
        assert!(steps[1].expect.shown_on(&screen(PREVIEW)));
        assert_eq!(p.pick(4), None);
    }

    #[test]
    fn a_preview_with_notes_in_use_is_not_answered() {
        // Open: a digit would be typed into the notes.
        let open = PREVIEW.replace(NOTES_EMPTY, "Add notes on this design…");
        let p = parse_screen(&screen(&open)).unwrap();
        assert!(p.notes_in_use());
        assert_eq!(p.pick(2), None);
        // Closed with text: Enter would send the notes along.
        let written = PREVIEW.replace(NOTES_EMPTY, "2");
        assert_eq!(parse_screen(&screen(&written)).unwrap().pick(2), None);
        // Notes opened between the digit and Enter.
        let steps = parse_screen(&screen(PREVIEW)).unwrap().pick(2).unwrap();
        let typed = preview_on_two().replace(NOTES_EMPTY, "");
        assert!(!steps[1].expect.shown_on(&screen(&typed)));
        // No notes line found: not answered.
        let gone = PREVIEW.replace("Notes: press n to add notes", "");
        assert_eq!(parse_screen(&screen(&gone)).unwrap().pick(2), None);
    }

    #[test]
    fn a_box_without_its_bottom_corner_is_not_a_preview() {
        let text = TABBED.replace("❯ 1. Apple", "❯ 1. Apple ┌─┐");
        let p = parse_screen(&screen(&text)).unwrap();
        assert_eq!(p.options[0].label, "Apple ┌─┐");
        assert_eq!(p.pick(1).unwrap().len(), 1);
    }

    #[test]
    fn nothing_waiting() {
        // Answered: the question remains in the scrollback, the selector does not.
        let answered = "⏺ User answered Claude's questions:
  ⎿  · Fruit? → Pear
────────────────────────────────
❯
────────────────────────────────";
        assert_eq!(parse_screen(&screen(answered)), None);
        assert_eq!(parse_screen(&[]), None);
        // A numbered list in agent output is not a prompt without a cursor.
        let list = "────────────────────────────────
  1. first
  2. second";
        assert_eq!(parse_screen(&screen(list)), None);
    }
}
