//! Shutdown-signal registration: set an `AtomicBool` when the process is asked
//! to terminate, so long-running loops can exit cleanly.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Register `flag` to be set on a termination request (Unix `SIGTERM`; Windows
/// console close/break/shutdown events).
pub fn register_term(flag: &Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(flag));
    }
    #[cfg(windows)]
    {
        win::register(win::Kind::Term, Arc::clone(flag));
    }
}

/// Register `flag` to be set on an interrupt request (Unix `SIGINT`; Windows
/// Ctrl-C).
pub fn register_int(flag: &Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(flag));
    }
    #[cfg(windows)]
    {
        win::register(win::Kind::Int, Arc::clone(flag));
    }
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };

    // Win32 BOOL is a plain i32; TRUE = 1, FALSE = 0.
    const TRUE: i32 = 1;
    const FALSE: i32 = 0;

    #[derive(Clone, Copy)]
    pub enum Kind {
        Int,
        Term,
    }

    static INT_FLAGS: OnceLock<Mutex<Vec<Arc<AtomicBool>>>> = OnceLock::new();
    static TERM_FLAGS: OnceLock<Mutex<Vec<Arc<AtomicBool>>>> = OnceLock::new();
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    fn int_flags() -> &'static Mutex<Vec<Arc<AtomicBool>>> {
        INT_FLAGS.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn term_flags() -> &'static Mutex<Vec<Arc<AtomicBool>>> {
        TERM_FLAGS.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn set_all(flags: &Mutex<Vec<Arc<AtomicBool>>>) -> bool {
        if let Ok(list) = flags.lock() {
            if list.is_empty() {
                return false;
            }
            for f in list.iter() {
                f.store(true, Ordering::SeqCst);
            }
            return true;
        }
        false
    }

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        let handled = match ctrl_type {
            CTRL_C_EVENT => set_all(int_flags()),
            CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                set_all(term_flags())
            }
            _ => false,
        };
        if handled { TRUE } else { FALSE }
    }

    pub fn register(kind: Kind, flag: Arc<AtomicBool>) {
        if !INSTALLED.swap(true, Ordering::SeqCst) {
            // SAFETY: installs a process-wide console control handler once.
            unsafe {
                SetConsoleCtrlHandler(Some(handler), TRUE);
            }
        }
        let flags = match kind {
            Kind::Int => int_flags(),
            Kind::Term => term_flags(),
        };
        if let Ok(mut list) = flags.lock() {
            list.push(flag);
        }
    }
}
