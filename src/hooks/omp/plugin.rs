pub const PLUGIN_SOURCE: &str = include_str!("../../omp_plugin/hcom.ts");
const PLUGIN_FILENAME: &str = "hcom.ts";

fn current_home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}

fn omp_plugin_dir() -> std::path::PathBuf {
    let tool_root = crate::runtime_env::tool_config_root();
    let home = current_home_dir();
    if tool_root == home {
        if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR")
            && !dir.is_empty()
        {
            return std::path::PathBuf::from(dir).join("extensions");
        }
        home.join(".omp").join("agent").join("extensions")
    } else {
        tool_root.join(".omp").join("extensions")
    }
}

pub fn get_omp_plugin_path() -> std::path::PathBuf {
    omp_plugin_dir().join(PLUGIN_FILENAME)
}

pub fn extension_inject_args() -> Vec<String> {
    vec![
        "-e".to_string(),
        get_omp_plugin_path().to_string_lossy().to_string(),
    ]
}

/// Remove hcom's managed OMP extension injection (`-e <hcom.ts>` /
/// `--extension …`, incl. the `=` forms) from a stored or replayed launch-arg
/// vector, preserving every user-supplied extension and its ordering. An entry
/// is treated as managed when its path is the current plugin path, an existing
/// hcom-owned file, or — for a moved/missing managed file — a narrow lexical
/// match (basename `hcom.ts` directly under an `extensions` directory).
///
/// Idempotent. Callers strip stored args before snapshotting and reinjecting so
/// a stale plugin path from an older hcom/config layout is not replayed
/// alongside the freshly injected current path (which could fail startup or load
/// hcom twice). A genuine `-e other.ts` user extension always survives.
pub fn strip_managed_extension_args(args: &mut Vec<String>) {
    let current = get_omp_plugin_path();
    let is_managed = |value: &str| -> bool {
        let path = std::path::Path::new(value);
        if path == current.as_path() {
            return true;
        }
        if is_hcom_owned(path) {
            return true;
        }
        // Moved/missing managed file only: basename hcom.ts under an `extensions`
        // dir. Gated on !exists so an EXISTING user `-e …/extensions/hcom.ts`
        // with unrelated contents (which is_hcom_owned already rejected) is kept
        // — only exact-current and hcom-owned files are removed when present.
        !path.exists()
            && path.file_name().and_then(|n| n.to_str()) == Some(PLUGIN_FILENAME)
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("extensions")
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        // Two-token forms: `-e PATH` / `--extension PATH`.
        if (tok == "-e" || tok == "--extension") && i + 1 < args.len() {
            if is_managed(&args[i + 1]) {
                i += 2;
                continue;
            }
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            i += 2;
            continue;
        }
        // Equals forms: `--extension=PATH` / `-e=PATH`.
        if let Some(value) = tok
            .strip_prefix("--extension=")
            .or_else(|| tok.strip_prefix("-e="))
            && is_managed(value)
        {
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    *args = out;
}

fn plugin_matches_source(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content == PLUGIN_SOURCE,
        Err(_) => false,
    }
}

pub fn verify_omp_plugin_installed() -> bool {
    plugin_matches_source(&get_omp_plugin_path())
}

fn is_hcom_owned(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| content.contains("customType: \"hcom-bootstrap\""))
        .unwrap_or(false)
}

pub fn install_omp_plugin() -> std::io::Result<bool> {
    let target_dir = omp_plugin_dir();
    let target = target_dir.join(PLUGIN_FILENAME);
    std::fs::create_dir_all(&target_dir)?;
    if target.is_symlink() || target.exists() {
        if !plugin_matches_source(&target) && !is_hcom_owned(&target) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "A non-hcom hcom.ts file already exists and will not be overwritten",
            ));
        }
        std::fs::remove_file(&target)?;
    }
    std::fs::write(&target, PLUGIN_SOURCE)?;
    Ok(true)
}

pub fn ensure_omp_plugin_installed() -> bool {
    if verify_omp_plugin_installed() {
        return true;
    }
    install_omp_plugin().unwrap_or(false)
}

pub fn remove_omp_plugin() -> std::io::Result<()> {
    let path = get_omp_plugin_path();
    if (path.exists() || path.is_symlink())
        && (plugin_matches_source(&path) || is_hcom_owned(&path))
    {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
