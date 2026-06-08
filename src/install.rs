use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

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
        return Err(format!("{} is not a JSON object", settings_path.display()));
    }

    if let Some(existing) = settings
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        && !existing.contains("ccstatus")
    {
        return Err(format!(
            "{} already configures statusLine.command = {existing:?}; remove or edit it by hand before re-running --install",
            settings_path.display()
        ));
    }

    settings["statusLine"] = json!({
        "type": "command",
        "command": exe_str,
    });

    // Wire the hooks that feed per-session state: Stop (turn finished) and
    // UserPromptSubmit (turn started → "working" in `ccstatus top`).
    ensure_hook(&mut settings, "Stop", "stop", exe_str);
    ensure_hook(
        &mut settings,
        "UserPromptSubmit",
        "user-prompt-submit",
        exe_str,
    );

    let pretty = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("serializing settings: {e}"))?;
    fs::write(&settings_path, format!("{pretty}\n"))
        .map_err(|e| format!("writing {}: {e}", settings_path.display()))?;

    println!(
        "ccstatus: wired statusLine.command + Stop/UserPromptSubmit hooks -> {exe_str} in {}",
        settings_path.display()
    );

    // Discoverability, not configuration: the routing config is optional (no
    // file = sensible default layout), so we point at it rather than write it.
    println!(
        "\nOptional: customize status-element routing in {} (see examples/config.json).",
        crate::config::config_path().display()
    );
    // On Linux, a heads-up if the default non-tmux "jump" actuator can't work
    // here yet (see `window.rs` / examples/jump-linux.sh).
    if let Some(note) = jump_note() {
        println!("{note}");
    }
    Ok(())
}

/// A one-line heads-up when ccstatus's bundled non-tmux "jump" actuator won't
/// work in this environment (Linux only; `None` otherwise). Pure decision in
/// [`jump_hint`]; this resolves the environment.
#[cfg(target_os = "linux")]
fn jump_note() -> Option<&'static str> {
    let set = |k: &str| env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    jump_hint(
        set("WAYLAND_DISPLAY"),
        set("DISPLAY"),
        crate::config::jump_command().is_some(),
        on_path("wmctrl") || on_path("xdotool"),
    )
}

#[cfg(not(target_os = "linux"))]
fn jump_note() -> Option<&'static str> {
    None
}

/// Pure: which (if any) jump heads-up to print, given the graphical display in
/// play, whether the user set `jump.linux`, and whether an X11 tool is present.
/// `None` when jumping is already covered or irrelevant (headless / configured
/// / X11 with a tool installed).
#[cfg(target_os = "linux")]
fn jump_hint(wayland: bool, x11: bool, configured: bool, x11_tool: bool) -> Option<&'static str> {
    if configured || (!wayland && !x11) {
        return None;
    }
    if wayland {
        // The bundled default is X11-only; Wayland has no portable actuator.
        return Some(
            "note: non-tmux \"jump\" under Wayland has no built-in default \u{2014} set `jump.linux` (see examples/README.md).",
        );
    }
    (!x11_tool).then_some(
        "note: non-tmux \"jump\" on X11 needs `wmctrl` or `xdotool` (neither found) \u{2014} install one, or set `jump.linux`.",
    )
}

/// Whether `bin` is an existing file on `$PATH`.
#[cfg(target_os = "linux")]
fn on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Ensure `settings.hooks.<event>` runs `<exe> --hook <kind>`, idempotently:
/// update an existing ccstatus entry for this kind in place (so the path
/// tracks the current binary), else append a new hook group — leaving any
/// non-ccstatus hooks the user configured untouched.
fn ensure_hook(settings: &mut Value, event: &str, kind: &str, exe_str: &str) {
    let cmd = format!("{exe_str} --hook {kind}");
    let needle = format!("--hook {kind}");

    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let groups = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    if !groups.is_array() {
        *groups = json!([]);
    }
    let groups = groups.as_array_mut().unwrap();

    for group in groups.iter_mut() {
        if let Some(list) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            for h in list.iter_mut() {
                if let Some(c) = h.get("command").and_then(|x| x.as_str())
                    && c.contains("ccstatus")
                    && c.contains(&needle)
                {
                    h["command"] = json!(cmd);
                    return;
                }
            }
        }
    }
    groups.push(json!({ "hooks": [ { "type": "command", "command": cmd } ] }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_commands(settings: &Value) -> Vec<String> {
        settings["hooks"]["Stop"]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
                    .flatten()
                    .filter_map(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn adds_hook_to_empty_settings() {
        let mut s = json!({});
        ensure_hook(&mut s, "Stop", "stop", "/bin/ccstatus");
        assert_eq!(stop_commands(&s), vec!["/bin/ccstatus --hook stop"]);
    }

    #[test]
    fn is_idempotent_and_updates_path() {
        let mut s = json!({});
        ensure_hook(&mut s, "Stop", "stop", "/old/ccstatus");
        ensure_hook(&mut s, "Stop", "stop", "/new/ccstatus"); // re-run, new path
        // Updated in place, not duplicated.
        assert_eq!(stop_commands(&s), vec!["/new/ccstatus --hook stop"]);
    }

    #[test]
    fn preserves_foreign_hooks() {
        let mut s = json!({
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "/other/tool --do-thing" } ] } ] }
        });
        ensure_hook(&mut s, "Stop", "stop", "/bin/ccstatus");
        let cmds = stop_commands(&s);
        assert!(cmds.contains(&"/other/tool --do-thing".to_string()));
        assert!(cmds.contains(&"/bin/ccstatus --hook stop".to_string()));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn jump_hint_covers_the_environments() {
        // Headless (no display): jump is irrelevant -> no note.
        assert_eq!(jump_hint(false, false, false, false), None);
        // User set jump.linux: they've handled it -> no note, any display.
        assert_eq!(jump_hint(true, false, true, false), None);
        assert_eq!(jump_hint(false, true, true, false), None);
        // X11 with no tool: nudge to install one.
        assert!(
            jump_hint(false, true, false, false)
                .unwrap()
                .contains("X11")
        );
        // X11 with a tool present: good to go -> no note.
        assert_eq!(jump_hint(false, true, false, true), None);
        // Wayland: no built-in default, even if an X11 tool happens to exist.
        assert!(
            jump_hint(true, false, false, true)
                .unwrap()
                .contains("Wayland")
        );
    }

    #[test]
    fn different_kinds_coexist() {
        let mut s = json!({});
        ensure_hook(&mut s, "Stop", "stop", "/bin/ccstatus");
        ensure_hook(
            &mut s,
            "UserPromptSubmit",
            "user-prompt-submit",
            "/bin/ccstatus",
        );
        assert_eq!(stop_commands(&s), vec!["/bin/ccstatus --hook stop"]);
        assert_eq!(
            s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            json!("/bin/ccstatus --hook user-prompt-submit")
        );
    }
}
