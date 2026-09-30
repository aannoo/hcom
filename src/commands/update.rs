//! `hcom update` command — check and apply updates.
//!
//! Uses the shared `fetch_update_info()` function from update.rs to get current,
//! latest, and availability in one call. Applies immediately when an update is
//! available; `--check` reports availability without applying.

use crate::db::HcomDb;
use crate::shared::CommandContext;

#[derive(clap::Parser, Debug)]
#[command(name = "update", about = "Check for and apply updates")]
pub struct UpdateArgs {
    /// Only check — print update status without applying
    #[arg(long)]
    pub check: bool,
}

fn print_dev_root_notice(db: &HcomDb) {
    if let Some((path, source)) = crate::router::resolve_effective_dev_root(db.path()) {
        println!("Using local build: {} [{}]", path.display(), source);
        println!("`hcom update` bypasses dev_root and updates the binary you invoked.");
        println!("The local checkout is not changed.");
        println!();
    }
}

pub fn cmd_update(_db: &HcomDb, args: &UpdateArgs, _ctx: Option<&CommandContext>) -> i32 {
    println!("Checking for updates...");
    print_dev_root_notice(_db);

    let info = match crate::update::fetch_update_info() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    if !info.available {
        println!("hcom v{} is up to date", info.current);
        // Clear stale "update available" cache if it existed
        let _ = crate::paths::atomic_write(&crate::update::flag_path(), "");
        return 0;
    }

    println!("Update available: v{} → v{}", info.current, info.latest);

    if args.check {
        println!("Run `hcom update` to apply.");
        return 0;
    }

    let status = if cfg!(windows) {
        if crate::update::is_powershell_installer_command(info.cmd) {
            let program = crate::update::windows_installer_program();
            println!(
                "Running: {program} -NoProfile -ExecutionPolicy Bypass -Command \"irm https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.ps1 | iex\""
            );
            std::process::Command::new(program)
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "irm https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.ps1 | iex",
                ])
                .status()
        } else if crate::update::is_shell_pipe_command(info.cmd) {
            Err(std::io::Error::other(
                "POSIX shell update command selected on Windows",
            ))
        } else {
            println!("Running: {}", info.cmd);
            match crate::update::split_program_args(info.cmd) {
                Some((program, args)) => std::process::Command::new(program).args(args).status(),
                None => Err(std::io::Error::other("empty update command")),
            }
        }
    } else {
        println!("Running: {}", info.cmd);
        std::process::Command::new("sh")
            .args(["-c", info.cmd])
            .status()
    };

    match status {
        Ok(s) if s.success() => {
            // Clear the cached "update available" notice
            let _ = crate::paths::atomic_write(&crate::update::flag_path(), "");
            println!("Done. Run 'hcom --version' to confirm.");
            0
        }
        Ok(s) => {
            eprintln!(
                "Error: Update command failed (exit {})",
                s.code().unwrap_or(-1)
            );
            1
        }
        Err(e) => {
            eprintln!("Error: Could not run update command: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn update_args_default() {
        let args = UpdateArgs::try_parse_from(["update"]).unwrap();
        assert!(!args.check);
    }

    #[test]
    fn update_args_check_flag() {
        let args = UpdateArgs::try_parse_from(["update", "--check"]).unwrap();
        assert!(args.check);
    }

    #[test]
    fn print_dev_root_notice_is_safe_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        print_dev_root_notice(&db);
    }
}
