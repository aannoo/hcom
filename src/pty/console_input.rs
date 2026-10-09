//! Console input records → ConPTY input bytes (Windows relay).
//!
//! The relay reads its console as `INPUT_RECORD`s in the same input mode the
//! child reads its own console ([`ConsoleInput`]), so keys reach the child as
//! they would if it ran directly. `std::io::stdin()` can't be used: on a
//! console it reports end-of-file for a read that ends in Ctrl+Z.
//!
//! - [`ConsoleInput::Vt`]: the console host has already turned keys into VT
//!   text; forward the characters of key-down records. Mouse records become
//!   SGR reports only while the child has SGR mouse tracking on, and never
//!   once the console has handed over a mouse report as text itself.
//! - [`ConsoleInput::Records`]: forward keys in win32-input-mode
//!   (`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`), which the ConPTY is created to
//!   accept, so the child sees the exact record — modifiers included, which
//!   is what makes Shift+Enter differ from Enter. Characters with no virtual
//!   key are terminal input the console host didn't map to a key (replies to
//!   the child's queries) and go through as text, as Windows Terminal sends
//!   them. Mouse records become SGR mouse reports.
//!
//! Platform-independent so it's tested on every host; `win.rs` maps the
//! Windows records onto these types.

#![cfg_attr(not(windows), allow(dead_code))]

use crate::integration_spec::ConsoleInput;

/// A `KEY_EVENT_RECORD`.
#[derive(Clone, Copy, Debug)]
pub(super) struct KeyRecord {
    pub down: bool,
    pub repeat: u16,
    pub vk: u16,
    pub scan: u16,
    pub ch: u16,
    pub ctrl: u32,
}

/// A `MOUSE_EVENT_RECORD`, position 0-based.
#[derive(Clone, Copy, Debug)]
pub(super) struct MouseRecord {
    pub x: i16,
    pub y: i16,
    pub buttons: u32,
    pub ctrl: u32,
    pub flags: u32,
}

// dwControlKeyState
const RIGHT_ALT_PRESSED: u32 = 0x1;
const LEFT_ALT_PRESSED: u32 = 0x2;
const RIGHT_CTRL_PRESSED: u32 = 0x4;
const LEFT_CTRL_PRESSED: u32 = 0x8;
const SHIFT_PRESSED: u32 = 0x10;
// dwEventFlags
const MOUSE_MOVED: u32 = 0x1;
const MOUSE_WHEELED: u32 = 0x4;
const MOUSE_HWHEELED: u32 = 0x8;
// dwButtonState low bits → SGR button number
const BUTTONS: [(u32, u32); 3] = [(0x1, 0), (0x4, 1), (0x2, 2)];

/// The mouse reports a VT child asked for (DECSET 1000/1002/1003), when it
/// asked for them in SGR encoding (1006). Other encodings count as `Off`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum MouseTracking {
    #[default]
    Off = 0,
    /// Presses only (X10).
    Press = 1,
    /// Presses and releases.
    PressRelease = 2,
    /// Plus moves with a button held.
    ButtonMotion = 3,
    /// Plus every move.
    AnyMotion = 4,
}

impl MouseTracking {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Press,
            2 => Self::PressRelease,
            3 => Self::ButtonMotion,
            4 => Self::AnyMotion,
            _ => Self::Off,
        }
    }
}

pub(super) struct InputEncoder {
    mode: ConsoleInput,
    /// Mouse buttons held as of the last record, to turn the console's
    /// button-state snapshots into SGR press/release edges.
    buttons: u32,
    /// High surrogate of a character split across two records.
    high_surrogate: Option<u16>,
    /// The last two characters forwarded in Vt mode, to spot `ESC [ <`.
    vt_tail: [u16; 2],
    /// Vt mode: the console has sent a mouse report as text, so it already
    /// reports the mouse that way and records must not be reported again.
    sgr_text_seen: bool,
}

impl InputEncoder {
    pub(super) fn new(mode: ConsoleInput) -> Self {
        Self {
            mode,
            buttons: 0,
            high_surrogate: None,
            vt_tail: [0; 2],
            sgr_text_seen: false,
        }
    }

    /// Whether the console has sent a mouse report as text (Vt mode).
    #[cfg(test)]
    fn sgr_text_seen(&self) -> bool {
        self.sgr_text_seen
    }

    /// Append the bytes for `key` to `out`.
    pub(super) fn key(&mut self, key: KeyRecord, out: &mut Vec<u8>) {
        if self.mode == ConsoleInput::Vt || key.vk == 0 {
            if key.down && key.ch != 0 {
                // A held key can arrive as one record with a repeat count. A
                // surrogate half only makes sense once; pairing handles it.
                let repeat = if (0xD800..=0xDFFF).contains(&key.ch) {
                    1
                } else {
                    key.repeat.max(1)
                };
                for _ in 0..repeat {
                    self.push_text(key.ch, out);
                }
                if self.mode == ConsoleInput::Vt {
                    if self.vt_tail == [0x1b, u16::from(b'[')] && key.ch == u16::from(b'<') {
                        self.sgr_text_seen = true;
                    }
                    self.vt_tail = [self.vt_tail[1], key.ch];
                }
            }
            return;
        }
        self.high_surrogate = None;
        out.extend_from_slice(
            format!(
                "\x1b[{};{};{};{};{};{}_",
                key.vk,
                key.scan,
                key.ch,
                u8::from(key.down),
                key.ctrl,
                key.repeat.max(1)
            )
            .as_bytes(),
        );
    }

    fn push_text(&mut self, unit: u16, out: &mut Vec<u8>) {
        // An unpaired surrogate decodes to U+FFFD.
        let mut units: Vec<u16> = self.high_surrogate.take().into_iter().collect();
        if (0xD800..=0xDBFF).contains(&unit) {
            self.high_surrogate = Some(unit);
        } else {
            units.push(unit);
        }
        for c in char::decode_utf16(units) {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
    }

    /// Append the SGR mouse reports (`ESC [ < b ; x ; y M|m`) for `mouse`.
    /// `tracking` is what the child asked for; it only matters in Vt mode,
    /// where the reports are text the child reads as keys.
    pub(super) fn mouse(&mut self, mouse: MouseRecord, tracking: MouseTracking, out: &mut Vec<u8>) {
        let held = mouse.buttons & 0x7;
        let tracking = match self.mode {
            ConsoleInput::Records => MouseTracking::AnyMotion,
            // Some console hosts hand a VT client the terminal's mouse report
            // as text; others parse it into a record the client never sees
            // as text. Report records only for a child that asked, so moves
            // can't type into a prompt, and only if text never came.
            ConsoleInput::Vt if self.sgr_text_seen => MouseTracking::Off,
            ConsoleInput::Vt => tracking,
        };
        if tracking == MouseTracking::Off {
            self.buttons = held;
            return;
        }
        let x = i32::from(mouse.x).max(0) + 1;
        let y = i32::from(mouse.y).max(0) + 1;
        let mods = modifier_bits(mouse.ctrl);
        let mut report = |code: u32, press: bool| {
            let fin = if press { 'M' } else { 'm' };
            out.extend_from_slice(format!("\x1b[<{};{x};{y}{fin}", code | mods).as_bytes());
        };
        // The high word of dwButtonState is the signed wheel delta.
        let wheel_up = (mouse.buttons as i32) > 0;
        if mouse.flags & MOUSE_WHEELED != 0 {
            report(if wheel_up { 64 } else { 65 }, true);
            return;
        }
        if mouse.flags & MOUSE_HWHEELED != 0 {
            report(if wheel_up { 67 } else { 66 }, true);
            return;
        }
        if mouse.flags & MOUSE_MOVED != 0 {
            let wanted = if held != 0 {
                MouseTracking::ButtonMotion
            } else {
                MouseTracking::AnyMotion
            };
            if tracking >= wanted {
                let code = BUTTONS
                    .iter()
                    .find(|(bit, _)| held & bit != 0)
                    .map_or(3, |(_, b)| *b);
                report(code + 32, true);
            }
        } else {
            for (bit, code) in BUTTONS {
                if held & bit != 0 && self.buttons & bit == 0 {
                    report(code, true);
                } else if held & bit == 0
                    && self.buttons & bit != 0
                    && tracking >= MouseTracking::PressRelease
                {
                    report(code, false);
                }
            }
        }
        self.buttons = held;
    }
}

fn modifier_bits(ctrl: u32) -> u32 {
    let mut bits = 0;
    if ctrl & SHIFT_PRESSED != 0 {
        bits |= 4;
    }
    if ctrl & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        bits |= 8;
    }
    if ctrl & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
        bits |= 16;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(down: bool, vk: u16, scan: u16, ch: u16, ctrl: u32) -> KeyRecord {
        KeyRecord {
            down,
            repeat: 1,
            vk,
            scan,
            ch,
            ctrl,
        }
    }

    fn keys_in(mode: ConsoleInput, records: &[KeyRecord]) -> String {
        let mut enc = InputEncoder::new(mode);
        let mut out = Vec::new();
        for r in records {
            enc.key(*r, &mut out);
        }
        String::from_utf8(out).unwrap()
    }

    fn keys(records: &[KeyRecord]) -> String {
        keys_in(ConsoleInput::Records, records)
    }

    fn mouse(enc: &mut InputEncoder, x: i16, y: i16, buttons: u32, flags: u32) -> String {
        tracked(enc, MouseTracking::Off, x, y, buttons, flags)
    }

    fn tracked(
        enc: &mut InputEncoder,
        tracking: MouseTracking,
        x: i16,
        y: i16,
        buttons: u32,
        flags: u32,
    ) -> String {
        let mut out = Vec::new();
        enc.mouse(
            MouseRecord {
                x,
                y,
                buttons,
                ctrl: 0,
                flags,
            },
            tracking,
            &mut out,
        );
        String::from_utf8(out).unwrap()
    }

    fn vt_text(enc: &mut InputEncoder, text: &str) -> String {
        let mut out = Vec::new();
        for c in text.encode_utf16() {
            enc.key(key(true, 0, 0, c, 0), &mut out);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn keys_keep_modifiers_in_win32_input_mode() {
        // Shift+Enter: the modifier is what VT text can't carry.
        assert_eq!(
            keys(&[key(true, 0x0D, 0x1C, 13, SHIFT_PRESSED)]),
            "\x1b[13;28;13;1;16;1_"
        );
        assert_eq!(
            keys(&[key(false, 0x0D, 0x1C, 13, 0)]),
            "\x1b[13;28;13;0;0;1_"
        );
    }

    #[test]
    fn ctrl_z_is_an_ordinary_key() {
        assert_eq!(
            keys(&[key(true, 0x5A, 0x2C, 0x1A, LEFT_CTRL_PRESSED)]),
            "\x1b[90;44;26;1;8;1_"
        );
    }

    #[test]
    fn repeat_count_is_at_least_one() {
        let mut r = key(true, 0x41, 0x1E, 97, 0);
        r.repeat = 0;
        assert_eq!(keys(&[r]), "\x1b[65;30;97;1;0;1_");
        r.repeat = 3;
        assert_eq!(keys(&[r]), "\x1b[65;30;97;1;0;3_");
    }

    #[test]
    fn keyless_characters_pass_through_as_text() {
        // A terminal's DA1 reply, as the console host hands it over.
        let reply: Vec<KeyRecord> = "\x1b[?62;22c"
            .encode_utf16()
            .flat_map(|c| [key(true, 0, 0, c, 0), key(false, 0, 0, c, 0)])
            .collect();
        assert_eq!(keys(&reply), "\x1b[?62;22c");
    }

    #[test]
    fn text_honours_repeat_count() {
        let mut r = key(true, 0, 0, b'a' as u16, 0);
        r.repeat = 5;
        assert_eq!(keys(&[r]), "aaaaa");
        let mut r = key(true, 0x41, 0x1E, b'b' as u16, 0);
        r.repeat = 3;
        assert_eq!(keys_in(ConsoleInput::Vt, &[r]), "bbb");
    }

    #[test]
    fn keyless_surrogate_pairs_join_across_records() {
        let units: Vec<u16> = "é🙂".encode_utf16().collect();
        let records: Vec<KeyRecord> = units.iter().map(|&c| key(true, 0, 0, c, 0)).collect();
        assert_eq!(keys(&records), "é🙂");
    }

    #[test]
    fn lone_surrogate_becomes_replacement_char() {
        assert_eq!(
            keys(&[key(true, 0, 0, 0xD83D, 0), key(true, 0, 0, b'a' as u16, 0)]),
            "\u{FFFD}a"
        );
    }

    #[test]
    fn vt_mode_forwards_key_down_characters_only() {
        // How the console host hands over Ctrl+Z, then an arrow key as VT text.
        let mut records = vec![
            key(true, 0, 0, 0x1A, 0),
            key(false, 0x5A, 0x2C, 0x1A, LEFT_CTRL_PRESSED),
        ];
        records.extend("\x1b[A".encode_utf16().map(|c| key(true, 0, 0, c, 0)));
        // A key-down that still carries its virtual key is forwarded as text too.
        records.push(key(true, 0x41, 0x1E, b'a' as u16, 0));
        assert_eq!(keys_in(ConsoleInput::Vt, &records), "\x1a\x1b[Aa");
    }

    #[test]
    fn mouse_button_edges_become_press_and_release() {
        let mut enc = InputEncoder::new(ConsoleInput::Records);
        assert_eq!(mouse(&mut enc, 9, 4, 0x1, 0), "\x1b[<0;10;5M");
        assert_eq!(mouse(&mut enc, 11, 4, 0x1, MOUSE_MOVED), "\x1b[<32;12;5M");
        assert_eq!(mouse(&mut enc, 11, 4, 0x0, 0), "\x1b[<0;12;5m");
        assert_eq!(mouse(&mut enc, 2, 3, 0x0, MOUSE_MOVED), "\x1b[<35;3;4M");
        // Right button: SGR button 2.
        assert_eq!(mouse(&mut enc, 0, 0, 0x2, 0), "\x1b[<2;1;1M");
        assert_eq!(mouse(&mut enc, 0, 0, 0x0, 0), "\x1b[<2;1;1m");
    }

    #[test]
    fn vt_mode_drops_mouse_records_until_the_child_tracks() {
        let mut enc = InputEncoder::new(ConsoleInput::Vt);
        assert_eq!(mouse(&mut enc, 2, 3, 0x0, MOUSE_MOVED), "");
        assert_eq!(mouse(&mut enc, 2, 3, 0x1, 0), "");
        assert_eq!(mouse(&mut enc, 2, 3, 0x0, 0), "");
        // A click once tracking is on is a fresh press, not a stale release.
        let t = MouseTracking::PressRelease;
        assert_eq!(tracked(&mut enc, t, 2, 3, 0x1, 0), "\x1b[<0;3;4M");
        assert_eq!(tracked(&mut enc, t, 2, 3, 0x0, 0), "\x1b[<0;3;4m");
    }

    #[test]
    fn vt_mode_reports_only_what_the_child_asked_for() {
        let mut enc = InputEncoder::new(ConsoleInput::Vt);
        let t = MouseTracking::Press;
        assert_eq!(tracked(&mut enc, t, 0, 0, 0x1, 0), "\x1b[<0;1;1M");
        assert_eq!(tracked(&mut enc, t, 1, 0, 0x1, MOUSE_MOVED), "");
        assert_eq!(tracked(&mut enc, t, 1, 0, 0x0, 0), "");
        assert_eq!(
            tracked(&mut enc, t, 0, 0, 120 << 16, MOUSE_WHEELED),
            "\x1b[<64;1;1M"
        );

        let t = MouseTracking::ButtonMotion;
        assert_eq!(tracked(&mut enc, t, 4, 0, 0x0, MOUSE_MOVED), "");
        assert_eq!(tracked(&mut enc, t, 4, 0, 0x1, 0), "\x1b[<0;5;1M");
        assert_eq!(
            tracked(&mut enc, t, 5, 0, 0x1, MOUSE_MOVED),
            "\x1b[<32;6;1M"
        );
        assert_eq!(tracked(&mut enc, t, 5, 0, 0x0, 0), "\x1b[<0;6;1m");

        let t = MouseTracking::AnyMotion;
        assert_eq!(
            tracked(&mut enc, t, 6, 0, 0x0, MOUSE_MOVED),
            "\x1b[<35;7;1M"
        );
    }

    #[test]
    fn vt_mode_stops_reporting_records_once_reports_arrive_as_text() {
        let mut enc = InputEncoder::new(ConsoleInput::Vt);
        let t = MouseTracking::PressRelease;
        assert_eq!(tracked(&mut enc, t, 0, 0, 0x1, 0), "\x1b[<0;1;1M");
        assert_eq!(vt_text(&mut enc, "\x1b[?62c\x1b[A"), "\x1b[?62c\x1b[A");
        assert!(!enc.sgr_text_seen());
        assert_eq!(vt_text(&mut enc, "\x1b[<0;1;1m"), "\x1b[<0;1;1m");
        assert!(enc.sgr_text_seen());
        assert_eq!(tracked(&mut enc, t, 0, 0, 0x0, 0), "");
        assert_eq!(tracked(&mut enc, t, 0, 0, 0x1, 0), "");
    }

    #[test]
    fn records_mode_ignores_child_tracking() {
        let mut enc = InputEncoder::new(ConsoleInput::Records);
        assert_eq!(vt_text(&mut enc, "\x1b[<0;1;1M"), "\x1b[<0;1;1M");
        assert!(!enc.sgr_text_seen());
        assert_eq!(mouse(&mut enc, 2, 3, 0x0, MOUSE_MOVED), "\x1b[<35;3;4M");
    }

    #[test]
    fn mouse_wheel_direction_comes_from_the_high_word() {
        let mut enc = InputEncoder::new(ConsoleInput::Records);
        assert_eq!(
            mouse(&mut enc, 0, 0, 120 << 16, MOUSE_WHEELED),
            "\x1b[<64;1;1M"
        );
        let down = (-120i32 as u32) & 0xFFFF_0000;
        assert_eq!(mouse(&mut enc, 0, 0, down, MOUSE_WHEELED), "\x1b[<65;1;1M");
    }

    #[test]
    fn mouse_modifiers_set_sgr_bits() {
        let mut enc = InputEncoder::new(ConsoleInput::Records);
        let mut out = Vec::new();
        enc.mouse(
            MouseRecord {
                x: 0,
                y: 0,
                buttons: 0x1,
                ctrl: SHIFT_PRESSED | LEFT_CTRL_PRESSED,
                flags: 0,
            },
            MouseTracking::Off,
            &mut out,
        );
        assert_eq!(out, b"\x1b[<20;1;1M");
    }
}
