/// Bundled scripts embedded at compile time.
pub const SCRIPTS: &[(&str, &str)] = &[
    ("confess", include_str!("scripts/bundled/confess.sh")),
    ("debate", include_str!("scripts/bundled/debate.sh")),
    ("fatcow", include_str!("scripts/bundled/fatcow.sh")),
    ("onidle", include_str!("scripts/bundled/onidle.sh")),
];
