//! Bundled ConPTY: make portable-pty use a current OpenConsole instead of the
//! inbox one.
//!
//! The inbox ConPTY (kernel32) parses the child's output into its own buffer
//! and re-renders it, so a TUI's frames reach the terminal rewritten — OpenCode
//! ends up with stale cells in the gutters and mangled words (#119). It also
//! answers DA1 itself, ahead of the real terminal's XTVERSION/kitty replies, so
//! a capability probe that uses DA1 as its "flush" sees no replies (#152).
//! Current OpenConsole passes output and queries through.
//!
//! portable-pty loads `conpty.dll` by bare name on its first `openpty`, falling
//! back to kernel32. A module already loaded under that name satisfies the
//! bare-name load, so preloading the bundled DLL by full path selects it.
//! `conpty.dll` starts the `OpenConsole.exe` that sits next to it.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

use crate::log::{log_info, log_warn};

const VERSION: &str = conpty_assets::VERSION;

/// Set once the bundled `conpty.dll` is loaded.
static BUNDLED: AtomicBool = AtomicBool::new(false);

/// Which ConPTY [`select`] picked, for diagnostics.
pub(super) fn selected() -> &'static str {
    if BUNDLED.load(Ordering::Relaxed) {
        VERSION
    } else {
        "inbox"
    }
}

#[cfg(target_arch = "x86_64")]
const FILES: &[(&str, &[u8])] = &[
    ("conpty.dll", conpty_assets::x64::CONPTY_DLL),
    ("OpenConsole.exe", conpty_assets::x64::OPENCONSOLE_EXE),
    ("LICENSE", conpty_assets::MICROSOFT_LICENSE.as_bytes()),
];
#[cfg(not(target_arch = "x86_64"))]
const FILES: &[(&str, &[u8])] = &[];

/// Select the ConPTY implementation for this process. Call once, before the
/// first `openpty`. Falls back to the inbox ConPTY on any failure, and when
/// `HCOM_CONPTY=system` asks for it.
///
/// The preload is also what keeps a `conpty.dll` some other app put on PATH
/// (WezTerm does) from being picked. Don't narrow the process-wide DLL search
/// to exclude PATH instead: `SetDefaultDllDirectories` broke Codex delivery.
pub(super) fn select() {
    if std::env::var("HCOM_CONPTY").is_ok_and(|v| v.eq_ignore_ascii_case("system")) {
        log_info(
            "pty",
            "conpty.system",
            "HCOM_CONPTY=system: using inbox ConPTY",
        );
        return;
    }
    if FILES.is_empty() {
        return;
    }
    let Some(dir) = dirs::data_local_dir().map(|d| d.join("hcom").join("conpty").join(VERSION))
    else {
        log_warn(
            "pty",
            "conpty.no_dir",
            "no local data dir; using inbox ConPTY",
        );
        return;
    };
    if let Err(e) = extract(&dir) {
        log_warn(
            "pty",
            "conpty.extract_failed",
            &format!("{}: {e}; using inbox ConPTY", dir.display()),
        );
        return;
    }
    let dll: Vec<u16> = dir
        .join("conpty.dll")
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `dll` is a NUL-terminated wide path. The module is never freed;
    // portable-pty keeps calling into it for the life of the process.
    let module = unsafe { LoadLibraryW(dll.as_ptr()) };
    if module.is_null() {
        log_warn(
            "pty",
            "conpty.load_failed",
            &format!(
                "{}: {}; using inbox ConPTY",
                dir.display(),
                std::io::Error::last_os_error()
            ),
        );
    } else {
        BUNDLED.store(true, Ordering::Relaxed);
    }
}

/// Write the bundled files into `dir` unless identical copies are already
/// there. Each file is written to a temp file and renamed into place, so a
/// crash never leaves a truncated binary behind. A file another hcom has
/// running can't be replaced, but then it already holds the same bytes.
fn extract(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, bytes) in FILES {
        let path = dir.join(name);
        if is_current(&path, bytes) {
            continue;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, bytes)?;
        if let Err(e) = tmp.persist(&path)
            && !is_current(&path, bytes)
        {
            return Err(e.error);
        }
    }
    Ok(())
}

fn is_current(path: &Path, bytes: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|on_disk| on_disk == bytes)
}
