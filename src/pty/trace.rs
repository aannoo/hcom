//! Relay byte trace: `HCOM_PTY_TRACE=<dir>` logs every hop of the Windows
//! relay to `<dir>/hcom-pty-<pid>.log`, one timestamped line per event, so a
//! byte lost or reordered between layers shows up at the hop that dropped it.
//!
//! - `mode`    console modes read and set on our own console
//! - `rec`     input records read from our console, decoded
//! - `in`      bytes written to the ConPTY input (keys, mouse, replies)
//! - `inject`  bytes the inject server wrote to the ConPTY input
//! - `out`     bytes read from the ConPTY (the child's output)
//! - `stdout`  bytes written to our console
//!
//! All hops share one file and one clock, so their interleaving is exact.
//! Off unless the env var is set; every call is a cheap check when off.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub(super) enum Hop {
    Mode,
    Record,
    Input,
    Inject,
    Output,
    Stdout,
}

impl Hop {
    fn tag(self) -> &'static str {
        match self {
            Hop::Mode => "mode",
            Hop::Record => "rec",
            Hop::Input => "in",
            Hop::Inject => "inject",
            Hop::Output => "out",
            Hop::Stdout => "stdout",
        }
    }
}

struct Trace {
    start: Instant,
    file: Mutex<BufWriter<File>>,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

fn trace() -> Option<&'static Trace> {
    TRACE
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os("HCOM_PTY_TRACE")?);
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("hcom-pty-{}.log", std::process::id()));
            let file = File::create(&path).ok()?;
            crate::log::log_info("pty", "trace.on", &path.display().to_string());
            Some(Trace {
                start: Instant::now(),
                file: Mutex::new(BufWriter::new(file)),
            })
        })
        .as_ref()
}

pub(super) fn enabled() -> bool {
    trace().is_some()
}

/// Log `data` crossing `hop`, escaped (see [`escape`]).
pub(super) fn bytes(hop: Hop, data: &[u8]) {
    if let Some(t) = trace() {
        t.line(hop, &format!("{:>5} {}", data.len(), escape(data)));
    }
}

/// Log a line of text for `hop`. Build it only when [`enabled`].
pub(super) fn note(hop: Hop, text: &str) {
    if let Some(t) = trace() {
        t.line(hop, text);
    }
}

impl Trace {
    fn line(&self, hop: Hop, text: &str) {
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if let Ok(mut f) = self.file.lock() {
            // Flushed per line so a crash or kill keeps everything before it.
            let _ = writeln!(f, "{ms:>11.3} {:<6} {text}", hop.tag());
            let _ = f.flush();
        }
    }
}

/// Printable ASCII as is, ESC as `\e`, other control and non-ASCII bytes as
/// `\xNN`, backslash doubled, so each event stays on one greppable line.
fn escape(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len());
    for &b in data {
        match b {
            0x1b => s.push_str("\\e"),
            b'\\' => s.push_str("\\\\"),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_keeps_one_line_and_marks_control_bytes() {
        assert_eq!(
            escape(b"\x1b[?1006h a\\b\r\n\x07\xc3\xa9"),
            "\\e[?1006h a\\\\b\\r\\n\\x07\\xc3\\xa9"
        );
    }
}
