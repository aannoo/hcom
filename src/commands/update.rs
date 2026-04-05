//! `hcom update` command — check and apply updates.
//!
//! Uses the shared `fetch_update_info()` function from update.rs to get current,
//! latest, and availability in one call. Handles interactive prompts and applies updates.

use crate::db::HcomDb;
use crate::shared::{CommandContext, is_inside_ai_tool};

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
        println!("`hcom update` updates the installed hcom, not this local checkout.");
        println!("Unset with `hcom config dev_root --unset` to update the installed binary.");
        println!();
    }
}

pub fn cmd_update(_db: &HcomDb, args: &UpdateArgs, ctx: Option<&CommandContext>) -> i32 {
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

    let go = ctx.map(|c| c.go).unwrap_or(false);
    let inside_ai = is_inside_ai_tool();

    // Inside AI tool without --go: suggest hcom update --go
    if inside_ai && !go {
        println!("Run `hcom update --go` to apply automatically.");
        return 0;
    }

    // Interactive prompt when running in a terminal
    if !go && !inside_ai {
        print!("Apply update? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
        {
            println!("Cancelled.");
            return 0;
        }
    }

    println!("Running: {}", info.cmd);
    let status = std::process::Command::new("sh")
        .args(["-c", info.cmd])
        .status();

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
