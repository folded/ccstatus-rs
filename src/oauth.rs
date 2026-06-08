use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

pub fn config_dir() -> String {
    env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_default();
        format!("{home}/.claude")
    })
}

pub fn config_dir_hash(dir: &str) -> String {
    let mut h = Sha256::new();
    h.update(dir.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s.truncate(8);
    s
}

/// Resolves the OAuth access token in priority order:
/// 1. `CLAUDE_CODE_OAUTH_TOKEN` env var
/// 2. macOS Keychain (`security find-generic-password`)
/// 3. `<config_dir>/.credentials.json`
/// 4. GNOME Keyring (`secret-tool`)
pub fn get_oauth_token(config_dir: &str) -> Option<String> {
    if let Ok(token) = env::var("CLAUDE_CODE_OAUTH_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }

    if let Some(token) = read_macos_keychain() {
        return Some(token);
    }

    let creds_path = format!("{config_dir}/.credentials.json");
    if let Some(token) = read_credentials_file(Path::new(&creds_path)) {
        return Some(token);
    }

    if let Some(token) = read_gnome_keyring() {
        return Some(token);
    }

    None
}

#[cfg(target_os = "macos")]
fn read_macos_keychain() -> Option<String> {
    let svc = match env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => format!("Claude Code-credentials-{}", config_dir_hash(&dir)),
        _ => "Claude Code-credentials".to_string(),
    };
    let out = Command::new("security")
        .args(["find-generic-password", "-s", &svc, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let blob = String::from_utf8(out.stdout).ok()?;
    extract_oauth_token(&blob)
}

#[cfg(not(target_os = "macos"))]
fn read_macos_keychain() -> Option<String> {
    None
}

fn read_credentials_file(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    extract_oauth_token(&contents)
}

#[cfg(target_os = "linux")]
fn read_gnome_keyring() -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", "Claude Code-credentials"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let blob = String::from_utf8(out.stdout).ok()?;
    extract_oauth_token(&blob)
}

#[cfg(not(target_os = "linux"))]
fn read_gnome_keyring() -> Option<String> {
    None
}

fn extract_oauth_token(json_blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_blob.trim()).ok()?;
    let token = v.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
    if token.is_empty() || token == "null" {
        None
    } else {
        Some(token.to_string())
    }
}
