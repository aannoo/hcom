//! Rewrites CSI sequences the `vt100` crate ignores into ones it handles.
//!
//! The screen tracker's parser only sees this rewritten copy; the terminal
//! receives the tool's original bytes. Without it, the tracker's cursor drifts
//! from the real terminal for tools whose renderers emit these sequences
//! (Antigravity uses REP for runs of spaces and box rules), and every later
//! relative cursor move lands in the wrong cell: injected text renders
//! scrambled on the tracked screen while the real terminal is correct.
//!
//! - HPA `CSI n \`` (horizontal position absolute) → CHA `CSI n G`
//! - REP `CSI n b` (repeat preceding graphic character) → the character n times
//! - CHT `CSI n I` (cursor forward tabulation) → n HT
//! - CBT `CSI n Z` (cursor backward tabulation) → CHA to the n-th earlier tab
//!   stop, computed from the parser's cursor at that point (vt100 and the
//!   tools both use fixed 8-column stops; agy resets them with `CSI ? 5 W`)

/// Upper bound on a single REP expansion, so a hostile or corrupt count can't
/// balloon memory. Far wider than any real terminal row.
const MAX_REPEAT: usize = 4096;

/// Longest CSI sequence buffered before giving up and passing it through.
const MAX_CSI_LEN: usize = 64;

/// vt100's fixed tab stop interval.
const TAB_WIDTH: u16 = 8;

/// Rewritten output: bytes for the parser, or a backward tab that needs the
/// parser's cursor position at that point.
#[derive(Debug, PartialEq, Eq)]
enum Chunk {
    Bytes(Vec<u8>),
    BackTab(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    /// OSC/DCS/SOS/PM/APC payload, terminated by BEL or ST (`ESC \`).
    String,
    StringEscape,
}

/// Streaming rewriter. Sequences split across reads are buffered until complete.
pub(crate) struct VtCompat {
    state: State,
    csi: Vec<u8>,
    /// UTF-8 bytes of the last graphic character printed in ground state.
    last_char: Vec<u8>,
}

impl VtCompat {
    pub(crate) fn new() -> Self {
        Self {
            state: State::Ground,
            csi: Vec::with_capacity(MAX_CSI_LEN),
            last_char: Vec::with_capacity(4),
        }
    }

    /// Rewrite `data` and feed it to `parser`.
    pub(crate) fn feed(&mut self, parser: &mut vt100::Parser, data: &[u8]) {
        for chunk in self.rewrite(data) {
            match chunk {
                Chunk::Bytes(bytes) => parser.process(&bytes),
                Chunk::BackTab(count) => {
                    let (_, col) = parser.screen().cursor_position();
                    let target = (0..count)
                        .fold(col, |col, _| col.saturating_sub(1) / TAB_WIDTH * TAB_WIDTH);
                    parser.process(format!("\x1b[{}G", target + 1).as_bytes());
                }
            }
        }
    }

    fn rewrite(&mut self, data: &[u8]) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        // Emitted together with the byte that follows it.
                        self.state = State::Escape;
                        continue;
                    } else if b >= 0x80 && b & 0xC0 == 0x80 {
                        // UTF-8 continuation byte of the current character.
                        if !self.last_char.is_empty() && self.last_char.len() < 4 {
                            self.last_char.push(b);
                        }
                    } else if b >= 0x20 && b != 0x7f {
                        self.last_char.clear();
                        self.last_char.push(b);
                    }
                    out.push(b);
                }
                State::Escape => {
                    if b == b'[' {
                        self.state = State::Csi;
                        self.csi.clear();
                        continue;
                    }
                    self.state = match b {
                        b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                        // Intermediates (e.g. `ESC ( B`) precede a final byte.
                        0x20..=0x2f => State::EscapeIntermediate,
                        _ => State::Ground,
                    };
                    out.extend_from_slice(&[0x1b, b]);
                }
                State::EscapeIntermediate => {
                    if !(0x20..=0x2f).contains(&b) {
                        self.state = State::Ground;
                    }
                    out.push(b);
                }
                State::Csi => {
                    self.csi.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if let Some(count) = self.finish_csi(&mut out) {
                            chunks.push(Chunk::Bytes(std::mem::take(&mut out)));
                            chunks.push(Chunk::BackTab(count));
                        }
                        self.state = State::Ground;
                    } else if self.csi.len() >= MAX_CSI_LEN {
                        out.extend_from_slice(b"\x1b[");
                        out.extend_from_slice(&self.csi);
                        self.state = State::Ground;
                    }
                }
                State::String => {
                    if b == 0x07 {
                        self.state = State::Ground;
                    } else if b == 0x1b {
                        self.state = State::StringEscape;
                    }
                    out.push(b);
                }
                State::StringEscape => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::String
                    };
                    out.push(b);
                }
            }
        }
        // A trailing partial CSI stays buffered in `self.csi` for the next call.
        chunks.push(Chunk::Bytes(out));
        chunks.retain(|c| *c != Chunk::Bytes(Vec::new()));
        chunks
    }

    /// Write the rewritten sequence to `out`, or return a backward-tab count
    /// for the caller to resolve against the cursor.
    fn finish_csi(&mut self, out: &mut Vec<u8>) -> Option<u16> {
        let (params, final_byte) = self.csi.split_at(self.csi.len() - 1);
        let plain = params.iter().all(|b| b.is_ascii_digit());
        let count = || {
            std::str::from_utf8(params)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1)
                .min(MAX_REPEAT)
        };
        match final_byte[0] {
            b'`' if plain => {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(params);
                out.push(b'G');
            }
            b'b' if plain => {
                for _ in 0..count() {
                    out.extend_from_slice(&self.last_char);
                }
            }
            b'I' if plain => out.extend(std::iter::repeat_n(b'\t', count())),
            b'Z' if plain => return Some(count().min(u16::MAX as usize) as u16),
            _ => {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(&self.csi);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewritten bytes, with back-tabs rendered as `<CBTn>`.
    fn norm(chunks: &[&[u8]]) -> Vec<u8> {
        let mut c = VtCompat::new();
        let mut out = Vec::new();
        for d in chunks {
            for chunk in c.rewrite(d) {
                match chunk {
                    Chunk::Bytes(b) => out.extend(b),
                    Chunk::BackTab(n) => out.extend(format!("<CBT{n}>").bytes()),
                }
            }
        }
        out
    }

    fn screen_after(rows: u16, cols: u16, data: &[u8]) -> (String, (u16, u16)) {
        let mut parser = vt100::Parser::new(
            rows.try_into().expect("test rows must be nonzero"),
            cols.try_into().expect("test columns must be nonzero"),
            0,
        );
        VtCompat::new().feed(&mut parser, data);
        let screen = parser.screen();
        (screen.contents(), screen.cursor_position())
    }

    #[test]
    fn splits_cbt_and_expands_cht() {
        assert_eq!(norm(&[b"ab\x1b[5Zc\x1b[2Id"]), b"ab<CBT5>c\t\td");
        assert_eq!(norm(&[b"\x1b[Z"]), b"<CBT1>");
        assert_eq!(norm(&[b"\x1b[?2Z"]), b"\x1b[?2Z");
    }

    #[test]
    fn backtab_moves_to_earlier_tab_stops() {
        // From column 48: five stops back is column 8.
        let (_, cursor) = screen_after(3, 69, b"\x1b[49G\x1b[5Z");
        assert_eq!(cursor, (0, 8));
        // From between stops, the first back-tab lands on the stop at or before.
        let (_, cursor) = screen_after(3, 69, b"\x1b[13G\x1b[Z");
        assert_eq!(cursor, (0, 8));
        let (_, cursor) = screen_after(3, 69, b"\x1b[3G\x1b[9Z");
        assert_eq!(cursor, (0, 0));
    }

    #[test]
    fn agy_wake_echo_with_backtab_renders_in_place() {
        // agy 1.2.16 echoing an injected wake: it repaints the model label two
        // rows down, then returns with CUU + CBT instead of CUB.
        let data = b"> <hcom>\n\n\x1b[38C\x1b[1K G\x1b[2A\x1b[5Z[inform #1] a -> b</hcom>";
        let (contents, _) = screen_after(4, 69, data);
        assert_eq!(
            contents.lines().next(),
            Some("> <hcom>[inform #1] a -> b</hcom>")
        );
    }

    #[test]
    fn expands_rep_with_last_ascii_char() {
        assert_eq!(norm(&[b"a\x1b[3b|"]), b"aaaa|");
        assert_eq!(norm(&[b"x\x1b[b"]), b"xx");
    }

    #[test]
    fn expands_rep_with_last_multibyte_char_across_sgr() {
        assert_eq!(
            norm(&["─\x1b[38;5;1m\x1b[2b".as_bytes()]),
            "─\x1b[38;5;1m──".as_bytes()
        );
    }

    #[test]
    fn rewrites_hpa_to_cha() {
        assert_eq!(norm(&[b"\x1b[12`x"]), b"\x1b[12Gx");
        assert_eq!(norm(&[b"\x1b[`"]), b"\x1b[G");
    }

    #[test]
    fn handles_sequences_split_across_reads() {
        assert_eq!(norm(&[b"ab\x1b", b"[4", b"b"]), b"abbbbb");
        assert_eq!(
            norm(&[
                "\u{2500}".as_bytes()[..1].as_ref(),
                &"\u{2500}".as_bytes()[1..],
                b"\x1b[1b"
            ]),
            "──".as_bytes()
        );
    }

    #[test]
    fn passes_other_sequences_through() {
        let input: &[u8] = b"\x1b[?25l\x1b[2A\x1b[42D\x1b[>4;2m\x1b(B\x1b]0;t b\x07z";
        assert_eq!(norm(&[input]), input);
        // Charset designation's final byte is not a printed character.
        assert_eq!(norm(&[b"a\x1b(B\x1b[2b"]), b"a\x1b(Baa");
    }

    #[test]
    fn osc_payload_does_not_become_rep_source() {
        assert_eq!(
            norm(&[b"q\x1b]2;title\x1b\\\x1b[2b"]),
            b"q\x1b]2;title\x1b\\qq"
        );
    }

    #[test]
    fn private_rep_is_untouched_and_count_is_capped() {
        assert_eq!(norm(&[b"a\x1b[?3b"]), b"a\x1b[?3b");
        assert_eq!(norm(&[b"a\x1b[999999b"]).len(), 1 + MAX_REPEAT);
    }
}
