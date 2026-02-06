//! Configuration loaded from environment variables at startup.
//!
//! Centralizes all HCOM_* env var access into a single Config struct,
//! providing a single source of truth with fail-fast validation.

use std::path::PathBuf;
use std::sync::Mutex;

/// Global configuration instance, lazily initialized and resettable for tests.
static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Find python3 next to our binary (same bin/ dir in a venv or uv tool install).
/// Falls back to bare "python3" for PATH resolution.
fn find_sibling_python() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("python3");
            if sibling.exists() {
                return sibling.to_string_lossy().into_owned();
            }
        }
    }
    "python3".to_string()
}

/// Configuration loaded from HCOM_* environment variables.
///
/// All environment variable access should go through this struct
/// rather than calling env::var directly.
#[derive(Clone, Debug)]
pub struct Config {
    /// HCOM directory (HCOM_DIR or ~/.hcom)
    pub hcom_dir: PathBuf,
    /// Instance name (HCOM_INSTANCE_NAME)
    pub instance_name: Option<String>,
    /// Process ID for daemon binding (HCOM_PROCESS_ID)
    pub process_id: Option<String>,
    /// PTY mode flag (HCOM_PTY_MODE=1)
    pub pty_mode: bool,
    /// PTY debug flag (HCOM_PTY_DEBUG=1)
    pub pty_debug: bool,
    /// Python executable (HCOM_PYTHON or "python3")
    pub python: String,
}

impl Config {
    /// Initialize global config from environment variables (call once at startup).
    /// Can be called multiple times - subsequent calls are no-ops.
    pub fn init() {
        let mut config = CONFIG.lock().unwrap();
        if config.is_none() {
            *config = Some(Self::from_env());
        }
    }

    /// Get reference to global config (must call init() first).
    /// Panics if init() was not called.
    pub fn get() -> Config {
        CONFIG
            .lock()
            .unwrap()
            .clone()
            .expect("Config::init() must be called before Config::get()")
    }

    /// Reset global config (test-only).
    /// Allows tests to reinitialize config with different env vars.
    #[cfg(test)]
    pub fn reset() {
        *CONFIG.lock().unwrap() = None;
    }

    /// Load configuration from environment variables
    fn from_env() -> Self {
        use std::env;

        // HCOM_DIR: custom directory or ~/.hcom
        let hcom_dir = if let Ok(dir) = env::var("HCOM_DIR") {
            PathBuf::from(dir)
        } else if let Ok(home) = env::var("HOME") {
            PathBuf::from(home).join(".hcom")
        } else {
            PathBuf::from(".hcom")
        };

        // HCOM_INSTANCE_NAME: optional instance name
        let instance_name = env::var("HCOM_INSTANCE_NAME").ok().filter(|s| !s.is_empty());

        // HCOM_PROCESS_ID: optional process ID for daemon binding
        let process_id = env::var("HCOM_PROCESS_ID").ok().filter(|s| !s.is_empty());

        // HCOM_PTY_MODE: boolean flag (true if "1")
        let pty_mode = env::var("HCOM_PTY_MODE").map(|v| v == "1").unwrap_or(false);

        // HCOM_PTY_DEBUG: boolean flag (true if "1")
        let pty_debug = env::var("HCOM_PTY_DEBUG").map(|v| v == "1").unwrap_or(false);

        // HCOM_PYTHON: python executable
        // Default: sibling python3 next to our binary (works in venvs / uv tool installs),
        // falls back to system "python3"
        let python = env::var("HCOM_PYTHON").unwrap_or_else(|_| find_sibling_python());

        Self {
            hcom_dir,
            instance_name,
            process_id,
            pty_mode,
            pty_debug,
            python,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    /// Helper to set env var for test scope
    fn with_env<F>(key: &str, value: &str, f: F)
    where
        F: FnOnce(),
    {
        // SAFETY: Tests use serial_test to run single-threaded.
        // No data races possible when tests run serially.
        unsafe {
            env::set_var(key, value);
        }
        f();
        unsafe {
            env::remove_var(key);
        }
    }

    /// Helper to clear multiple env vars for test scope
    fn without_env<F>(keys: &[&str], f: F)
    where
        F: FnOnce(),
    {
        let saved: Vec<_> = keys
            .iter()
            .map(|k| (k, env::var(k).ok()))
            .collect();

        // SAFETY: Tests use serial_test to run single-threaded.
        // No data races possible when tests run serially.
        for key in keys {
            unsafe {
                env::remove_var(key);
            }
        }

        f();

        for (key, val) in saved {
            if let Some(v) = val {
                unsafe {
                    env::set_var(key, v);
                }
            }
        }
    }

    #[test]
    #[serial]
    fn test_default_config_uses_home_hcom() {
        Config::reset();
        without_env(&["HCOM_DIR"], || {
            Config::init();
            let config = Config::get();

            // Should use ~/.hcom when HCOM_DIR not set
            let expected = env::var("HOME")
                .map(|h| PathBuf::from(h).join(".hcom"))
                .unwrap();
            assert_eq!(config.hcom_dir, expected);
        });
    }

    #[test]
    #[serial]
    fn test_hcom_dir_overrides_home() {
        Config::reset();
        with_env("HCOM_DIR", "/custom/hcom", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.hcom_dir, PathBuf::from("/custom/hcom"));
        });
    }

    #[test]
    #[serial]
    fn test_instance_name_some_when_set() {
        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "test-instance", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.instance_name, Some("test-instance".to_string()));
        });
    }

    #[test]
    #[serial]
    fn test_instance_name_none_when_unset() {
        Config::reset();
        without_env(&["HCOM_INSTANCE_NAME"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.instance_name, None);
        });
    }

    #[test]
    #[serial]
    fn test_process_id_some_when_set() {
        Config::reset();
        with_env("HCOM_PROCESS_ID", "pid-123", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.process_id, Some("pid-123".to_string()));
        });
    }

    #[test]
    #[serial]
    fn test_process_id_none_when_unset() {
        Config::reset();
        without_env(&["HCOM_PROCESS_ID"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.process_id, None);
        });
    }

    #[test]
    #[serial]
    fn test_pty_mode_true_when_1() {
        Config::reset();
        with_env("HCOM_PTY_MODE", "1", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.pty_mode, true);
        });
    }

    #[test]
    #[serial]
    fn test_pty_mode_false_when_unset() {
        Config::reset();
        without_env(&["HCOM_PTY_MODE"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.pty_mode, false);
        });
    }

    #[test]
    #[serial]
    fn test_pty_mode_false_when_not_1() {
        Config::reset();
        with_env("HCOM_PTY_MODE", "0", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.pty_mode, false);
        });
    }

    #[test]
    #[serial]
    fn test_pty_debug_true_when_1() {
        Config::reset();
        with_env("HCOM_PTY_DEBUG", "1", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.pty_debug, true);
        });
    }

    #[test]
    #[serial]
    fn test_pty_debug_false_when_unset() {
        Config::reset();
        without_env(&["HCOM_PTY_DEBUG"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.pty_debug, false);
        });
    }

    #[test]
    #[serial]
    fn test_python_default_finds_sibling_or_python3() {
        Config::reset();
        without_env(&["HCOM_PYTHON"], || {
            Config::init();
            let config = Config::get();
            // Either a sibling python3 path or bare "python3"
            assert!(
                config.python == "python3" || config.python.ends_with("/python3"),
                "expected python3 or path ending in /python3, got: {}",
                config.python
            );
        });
    }

    #[test]
    #[serial]
    fn test_python_respects_env_var() {
        Config::reset();
        with_env("HCOM_PYTHON", "/usr/local/bin/python3.11", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.python, "/usr/local/bin/python3.11");
        });
    }

    #[test]
    #[serial]
    fn test_reset_allows_reinit() {
        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "first", || {
            Config::init();
            assert_eq!(Config::get().instance_name, Some("first".to_string()));
        });

        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "second", || {
            Config::init();
            assert_eq!(Config::get().instance_name, Some("second".to_string()));
        });
    }
}
