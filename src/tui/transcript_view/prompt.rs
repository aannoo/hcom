//! A numbered choice prompt on a blocked agent's terminal screen (Claude's
//! `AskUserQuestion` or a permission prompt), and the keys that answer it.
//!
//! Claude writes a question to its transcript only after it is answered, so
//! the screen is the one place to read it while it waits. Measured on Claude
//! Code 2.x: a digit picks an option; one single-select question is answered
//! by it. With several questions or a multi-select one the prompt has tabs:
//! a single-select digit moves to the next tab, multi-select digits toggle
//! and → moves on, and the last tab submits on `1`.
use crate::commands::term::{ScreenCheck, ScreenStep};

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
}

const RULE: char = '─';
const DASH: char = '╌';
const FOOTER: &str = "Esc to cancel";

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

    let mut options: Vec<ScreenOption> = Vec::new();
    let mut footer = false;
    for line in &lines[first..] {
        let t = line.trim();
        if t.contains(FOOTER) {
            footer = true;
            break;
        }
        if let Some((number, label, _)) = option_line(line) {
            let (checked, label) = checkbox(label);
            options.push(ScreenOption {
                number,
                label: label.to_string(),
                description: String::new(),
                checked,
            });
        } else if !t.is_empty()
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
    Some(ScreenPrompt {
        tabs,
        question,
        options,
        anchor,
    })
}

impl ScreenPrompt {
    pub fn is_multi(&self) -> bool {
        self.options.iter().any(|o| o.checked.is_some())
    }

    /// Digit `n`: the step that sends it. `None` for an option that needs
    /// typing in the terminal, or no such option.
    pub fn pick(&self, n: usize) -> Option<ScreenStep<ScreenPrompt>> {
        let option = self.options.iter().find(|o| o.number == n)?;
        (!option.needs_typing()).then(|| ScreenStep {
            expect: self.clone(),
            keys: vec![n.to_string().into_bytes()],
        })
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

과일?

❯ 1. 사과
     사과를 선택합니다
  2. 배
  3. Type something.
────────────────────────────────────────────────────────────────────────────────
  4. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel";

    #[test]
    fn reads_a_tabbed_question() {
        let p = parse_screen(&screen(TABBED)).unwrap();
        assert_eq!(p.tabs.as_deref(), Some("☐ fruit  ☐ drink  ✔ Submit"));
        assert_eq!(p.question, vec!["과일?"]);
        let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["사과", "배", "Type something.", "Chat about this"]);
        assert_eq!(p.options[0].description, "사과를 선택합니다");
        assert!(!p.is_multi());
        assert_eq!(
            p.pick(2),
            Some(ScreenStep {
                expect: p.clone(),
                keys: vec![b"2".to_vec()],
            })
        );
        assert_eq!(p.anchor, FOOTER);
        assert!(p.pick(2).unwrap().expect.shown_on(&screen(TABBED)));
        assert_eq!(p.pick(3), None, "typing happens in the terminal");
        assert_eq!(p.pick(4), None);
        assert_eq!(p.pick(7), None);
        assert_eq!(p.next(), None);
    }

    #[test]
    fn reads_multi_select_checkboxes() {
        let text = "────────────────────────────────
←  ☒ fruit  ☐ drink  ✔ Submit  →

음료?

❯ 1. [✔] 물
         기본 생수
  2. [ ] 차
  3. [ ] Type something
     Submit
────────────────────────────────
  4. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
        let p = parse_screen(&screen(text)).unwrap();
        assert!(p.is_multi());
        assert_eq!(p.options[0].checked, Some(true));
        assert_eq!(p.options[0].label, "물");
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
                .shown_on(&screen(&text.replace("[✔] 물", "[ ] 물")))
        );
        assert!(
            !next
                .expect
                .shown_on(&screen(&text.replace("[ ] 차", "[✔] 차")))
        );
    }

    #[test]
    fn reads_the_review_tab() {
        let text = "────────────────────────────────
←  ☒ fruit  ☒ drink  ✔ Submit  →

Review your answers

 ● 과일?
   → 배

Ready to submit your answers?

❯ 1. Submit answers
  2. Cancel";
        let p = parse_screen(&screen(text)).unwrap();
        assert_eq!(p.question.last().unwrap(), "Ready to submit your answers?");
        // No footer on this screen: the cursor line anchors it.
        let expect = p.pick(1).unwrap().expect;
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
        let expect = p.pick(1).unwrap().expect;
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
        assert!(!p.pick(1).unwrap().expect.shown_on(&screen(&other)));
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
        // The plain prompt still reads.
        assert!(parse_screen(&screen(&permission_with(" make build"))).is_some());
    }

    #[test]
    fn reads_with_the_cursor_below_the_rule() {
        let text = TABBED
            .replace("❯ 1. 사과", "  1. 사과")
            .replace("  4. Chat about this", "❯ 4. Chat about this");
        let p = parse_screen(&screen(&text)).unwrap();
        assert_eq!(p.question, vec!["과일?"]);
        assert_eq!(p.options.len(), 4);
        assert_eq!(p.options[0].description, "사과를 선택합니다");
        assert_eq!(p.pick(1).unwrap().expect.anchor, FOOTER);
    }

    #[test]
    fn nothing_waiting() {
        // Answered: the question remains in the scrollback, the selector does not.
        let answered = "⏺ User answered Claude's questions:
  ⎿  · 과일? → 배
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
