use std::path::PathBuf;

use crate::log::{log_error, log_info};

const EXTENSION_ID: &str = "hcom.hcom-bridge";
const EXTENSION_VERSION: &str = "0.1.0";
const ANTIGRAVITY_BIN: &str = "antigravity";

pub const EXTENSION_JS: &str = include_str!("../antigravity_extension/extension.js");
pub const PACKAGE_JSON: &str = include_str!("../antigravity_extension/package.json");

fn get_antigravity_extensions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(&home).join(".antigravity").join("extensions")
}

fn get_extension_dir() -> PathBuf {
    get_antigravity_extensions_dir()
        .join(format!("{}-{}", EXTENSION_ID, EXTENSION_VERSION))
}

fn get_extensions_json_path() -> PathBuf {
    get_antigravity_extensions_dir().join("extensions.json")
}

pub fn is_antigravity_installed() -> bool {
    // Check in PATH
    let in_path = std::process::Command::new("which")
        .arg(ANTIGRAVITY_BIN)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // Check common install paths
    let at_path = std::path::Path::new("/opt/Antigravity/bin/antigravity").exists();
    in_path || at_path
}

pub fn verify_extension_installed() -> bool {
    let ext_dir = get_extension_dir();
    if !ext_dir.join("extension.js").exists() || !ext_dir.join("package.json").exists() {
        return false;
    }
    // Check extensions.json has our entry
    let ext_json_path = get_extensions_json_path();
    let content = match std::fs::read_to_string(&ext_json_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content.contains(EXTENSION_ID)
}

pub fn install_extension() -> std::io::Result<bool> {
    if verify_extension_installed() {
        return Ok(false); // already installed
    }

    if !is_antigravity_installed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Antigravity not found on this system",
        ));
    }

    let ext_dir = get_extension_dir();
    std::fs::create_dir_all(&ext_dir)?;

    // Write extension.js
    std::fs::write(ext_dir.join("extension.js"), EXTENSION_JS)?;

    // Write package.json
    std::fs::write(ext_dir.join("package.json"), PACKAGE_JSON)?;

    // Update extensions.json
    update_extensions_json()?;

    log_info(
        "antigravity",
        "extension.installed",
        &format!("installed hcom-bridge extension to {}", ext_dir.display()),
    );

    Ok(true)
}

fn update_extensions_json() -> std::io::Result<()> {
    let path = get_extensions_json_path();
    let mut entries: Vec<serde_json::Value> = if path.exists() {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    let ext_dir = get_extension_dir();
    let entry = serde_json::json!({
        "identifier": {
            "id": EXTENSION_ID
        },
        "version": EXTENSION_VERSION,
        "location": {
            "$mid": 1,
            "fsPath": ext_dir.to_string_lossy(),
            "path": ext_dir.to_string_lossy(),
            "scheme": "file"
        },
        "relativeLocation": format!("{}-{}", EXTENSION_ID, EXTENSION_VERSION),
        "metadata": {
            "installedTimestamp": chrono::Utc::now().timestamp_millis(),
            "source": "hcom",
            "id": EXTENSION_ID,
            "publisherDisplayName": "hcom",
            "targetPlatform": "universal",
            "updated": false,
            "isPreReleaseVersion": false,
            "hasPreReleaseVersion": false
        }
    });

    // Replace existing entry or append
    if let Some(pos) = entries.iter().position(|e| {
        e["identifier"]["id"].as_str() == Some(EXTENSION_ID)
    }) {
        entries[pos] = entry;
    } else {
        entries.push(entry);
    }

    let json = serde_json::to_string_pretty(&entries)?;
    std::fs::write(&path, json)?;

    Ok(())
}

pub fn remove_extension() -> std::io::Result<()> {
    let ext_dir = get_extension_dir();
    if ext_dir.exists() {
        std::fs::remove_dir_all(&ext_dir)?;
    }

    // Remove from extensions.json
    let path = get_extensions_json_path();
    if path.exists() {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<serde_json::Value> =
            serde_json::from_str(&content).unwrap_or_default();
        entries.retain(|e| e["identifier"]["id"].as_str() != Some(EXTENSION_ID));
        let json = serde_json::to_string_pretty(&entries)?;
        std::fs::write(&path, json)?;
    }

    log_info("antigravity", "extension.removed", "hcom-bridge extension removed");
    Ok(())
}

pub fn ensure_extension_installed() -> bool {
    if verify_extension_installed() {
        return true;
    }
    match install_extension() {
        Ok(_) => true,
        Err(e) => {
            log_error(
                "antigravity",
                "extension.install.failed",
                &format!("error: {e}"),
            );
            false
        }
    }
}

pub fn get_extension_status() -> (bool, String) {
    let ag_installed = is_antigravity_installed();
    if !ag_installed {
        return (false, "Antigravity not installed".into());
    }
    let ext_installed = verify_extension_installed();
    let ext_dir = get_extension_dir();
    let status = if ext_installed {
        format!("installed ({})", ext_dir.display())
    } else {
        "not installed".into()
    };
    (ext_installed, status)
}
