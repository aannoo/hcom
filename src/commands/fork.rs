//! Forks an active or stopped instance by launching with --fork-session
//! (Claude), fork subcommand (Codex), --fork (OpenCode), or respective options for Kilo/Pi/OMP.

use anyhow::Result;

use crate::router::GlobalFlags;

/// Run the fork command. `argv` is the full argv[1..].
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    let (name, extra_args) = super::resume::parse_resume_argv(argv, "f")?;
    super::resume::do_resume(&name, true, &extra_args, flags)
}
