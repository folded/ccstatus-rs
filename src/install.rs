use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::oauth;

pub fn run() -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| format!("resolving current executable: {e}"))?;
    let exe_str = exe
        .to_str()
        .ok_or_else(|| "current executable path is not valid UTF-8".to_string())?;

    let dir = oauth::config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("creating {dir}: {e}"))?;
    let settings_path = PathBuf::from(format!("{dir}/settings.json"));

    let mut settings: Value = if settings_path.exists() {
        let text = fs::read_to_string(&settings_path)
            .map_err(|e| format!("reading {}: {e}", settings_path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text)
                .map_err(|e| format!("parsing {}: {e}", settings_path.display()))?
        }
    } else {
        json!({})
    };

    if !settings.is_object() {
        return Err(format!(
            "{} is not a JSON object",
            settings_path.display()
        ));
    }

    if let Some(existing) = settings
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    {
        if !existing.contains("ccstatus") {
            return Err(format!(
                "{} already configures statusLine.command = {existing:?}; remove or edit it by hand before re-running --install",
                settings_path.display()
            ));
        }
    }

    settings["statusLine"] = json!({
        "type": "command",
        "command": exe_str,
    });

    let pretty = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("serializing settings: {e}"))?;
    fs::write(&settings_path, format!("{pretty}\n"))
        .map_err(|e| format!("writing {}: {e}", settings_path.display()))?;

    println!(
        "ccstatus: wired statusLine.command -> {exe_str} in {}",
        settings_path.display()
    );
    Ok(())
}
