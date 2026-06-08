mod api;
mod cache;
mod cli;
mod color;
mod config;
mod control;
mod daemon;
mod fleet;
mod format;
mod git;
mod heatmap;
mod hooks;
mod install;
mod ipc;
mod oauth;
mod render_tmux;
mod server_dir;
mod state;
mod term;
mod tmux;
mod top;
mod usage;
mod util;
mod window;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use cli::{Config, ParseOutcome};
use color::*;
use format::{format_tokens, shorten_model_name};
use state::PaneState;
use tmux::Tmux;
use util::{now_unix, resolve_session_id};

const SELF_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let cfg = match cli::parse_args(env::args().skip(1)) {
        ParseOutcome::Run(c) => c,
        ParseOutcome::Hook(kind) => return hooks::run(kind),
        ParseOutcome::Handler(session) => return daemon::run(session),
        ParseOutcome::TmuxReset => {
            tmux::reset(&tmux::CliTmux);
            println!("ccstatus: bar reset to defaults");
            return ExitCode::SUCCESS;
        }
        ParseOutcome::Top => return top::run(),
        ParseOutcome::Install => {
            return match install::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(msg) => {
                    eprintln!("ccstatus: {msg}");
                    ExitCode::FAILURE
                }
            };
        }
        ParseOutcome::Help => {
            print!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        ParseOutcome::Version => {
            println!("ccstatus {SELF_VERSION}");
            return ExitCode::SUCCESS;
        }
        ParseOutcome::Error(msg) => {
            eprintln!("ccstatus: {msg}\n\n{}", cli::HELP);
            return ExitCode::from(2);
        }
    };

    let mut buf = String::new();
    if io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        print!("Claude");
        return ExitCode::SUCCESS;
    }

    let input: Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => {
            print!("Claude");
            return ExitCode::SUCCESS;
        }
    };

    let _ = cache::ensure_cache_dir();

    // Presence: record this session's model/cwd/context/pid every render, so
    // the fleet sees it even outside tmux (where no pane state is written).
    write_session_presence(&input);

    let elements = render_elements(&input, &cfg);

    // Inside tmux, routing decides where each element goes; outside tmux
    // there is no tmux surface, so everything falls back to Claude's
    // statusline.
    let (routing, pane_id) = match active_tmux_pane() {
        Some(pane_id) => (config::Routing::load(), Some(pane_id)),
        None => (config::Routing::all_claude(), None),
    };

    // Store the rendered elements for the daemon and ping it, but only when
    // at least one element is routed to a tmux surface.
    if let Some(pane_id) = &pane_id
        && routing.any_tmux()
        && register_pane(&input, pane_id, &elements).is_some()
        && let Some(tmux_session) = tmux::CliTmux.session_of(pane_id)
    {
        let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());
        ipc::notify_register(&server_id, &tmux_session, pane_id);
    }

    // Print the elements routed to Claude's own statusline.
    let claude = compose_claude(&elements, &routing);
    if !claude.is_empty() {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(claude.as_bytes());
    }
    ExitCode::SUCCESS
}

/// Returns `Some(pane_id)` if Claude Code was launched inside tmux. Both
/// `$TMUX` and `$TMUX_PANE` must be set; either one missing means we render
/// for stdout as usual.
fn active_tmux_pane() -> Option<String> {
    env::var("TMUX").ok().filter(|s| !s.is_empty())?;
    env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())
}

/// Write per-pane state so the daemon can compose the tmux surfaces from
/// the rendered elements. Coalesces writes that arrive within 500 ms of an
/// identical prior write so a streaming statusline call doesn't hammer the
/// filesystem. Returns the session id that was registered (so the caller
/// can ping the daemon with it), or None if the input lacked one.
fn register_pane(
    input: &Value,
    pane_id: &str,
    elements: &[(config::Element, String)],
) -> Option<String> {
    let session_id = resolve_session_id(input)?;
    let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());
    let claude_pid = resolve_claude_pid();
    let pane_tty = tmux::CliTmux.pane_tty(pane_id).unwrap_or_default();
    let transcript_path = input
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let element_map: HashMap<String, String> = elements
        .iter()
        .filter(|(_, c)| !c.is_empty())
        .map(|(e, c)| (e.key().to_string(), c.clone()))
        .collect();

    if let Some(existing) = state::read_pane(&server_id, pane_id) {
        if existing.session_id == session_id
            && existing.claude_pid == claude_pid
            && existing.elements == element_map
            && pane_recent(&server_id, pane_id, Duration::from_millis(500))
        {
            return Some(session_id);
        }
    }

    let pane_state = PaneState {
        session_id: session_id.clone(),
        claude_pid,
        pane_tty,
        transcript_path,
        registered_at: now_unix(),
        last_warmth: None,
        elements: element_map,
    };
    let _ = state::write_pane(&server_id, pane_id, &pane_state);
    Some(session_id)
}

/// Walk up the process tree from our parent until we hit an *interactive*
/// Claude session process (not the shared daemon or a `--bg-*` helper — see
/// [`util::is_interactive_claude`]), then return that pid. Binding to the
/// daemon would make the session immortal in the fleet. Falls back to `$PPID`
/// after a bounded number of hops; the bound prevents pathological loops.
fn resolve_claude_pid() -> u32 {
    let ppid = unsafe { libc::getppid() } as u32;
    let mut pid = ppid;
    for _ in 0..8 {
        if util::is_interactive_claude(pid) {
            return pid;
        }
        match parent_of(pid) {
            Some(p) if p > 1 => pid = p,
            _ => break,
        }
    }
    ppid
}

fn parent_of(pid: u32) -> Option<u32> {
    ps_field(pid, "ppid=")?.trim().parse().ok()
}

fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", field])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pane_recent(server_id: &str, pane_id: &str, window: Duration) -> bool {
    let meta = match fs::metadata(state::pane_path(server_id, pane_id)) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    SystemTime::now()
        .duration_since(mtime)
        .map(|age| age < window)
        .unwrap_or(false)
}

/// Record this session's presence (model, cwd, context, pid) so the fleet can
/// show it — crucially, this runs on *every* render, including outside tmux
/// where no pane state exists. Merges into the session record without
/// touching the hook-owned turn fields. Coalesced: skips when nothing changed,
/// and (to spare a streaming statusline) when an unchanged-but-for-context
/// write would land within 1s of the last one.
fn write_session_presence(input: &Value) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    let before = s.clone();

    s.model = input
        .pointer("/model/display_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(s.model);
    let cwd = input.pointer("/cwd").and_then(|v| v.as_str()).unwrap_or("");
    if !cwd.is_empty() {
        s.cwd = Some(cwd.to_string());
    }
    s.claude_pid = Some(resolve_claude_pid());
    // Terminal identity for a non-tmux window jump. Constant for the process,
    // so this only contributes to the first write and never churns after.
    s.term_program = env::var("TERM_PROGRAM")
        .ok()
        .filter(|v| !v.is_empty())
        .or(s.term_program);
    s.iterm_session_id = env::var("ITERM_SESSION_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or(s.iterm_session_id);

    let usage = input.pointer("/context_window/current_usage");
    let size = input
        .pointer("/context_window/context_window_size")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0)
        .unwrap_or(200_000);
    let input_tokens = u64_at(usage, "input_tokens");
    let cache_create = u64_at(usage, "cache_creation_input_tokens");
    let cache_read = u64_at(usage, "cache_read_input_tokens");
    let current = input_tokens + cache_create + cache_read;
    s.context_pct_used = Some((current * 100 / size).min(100) as u32);
    let denom = input_tokens + cache_create + cache_read;
    s.cache_read_pct = Some(if denom > 0 {
        (cache_read * 100 / denom) as u32
    } else {
        0
    });

    if s == before {
        return;
    }
    if session_recent(&session_id, Duration::from_secs(1)) {
        return;
    }
    let _ = state::write_session(&session_id, &s);
}

fn session_recent(session_id: &str, window: Duration) -> bool {
    let meta = match fs::metadata(state::session_path(session_id)) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    SystemTime::now()
        .duration_since(mtime)
        .map(|age| age < window)
        .unwrap_or(false)
}

/// Render the statusline as a list of named elements, each holding its own
/// raw-ANSI content with **no** leading separator. Surfaces join the
/// segments they're routed (` | ` between them); rows stand alone. The live
/// `warmth` element is not produced here — the daemon computes it.
fn render_elements(input: &Value, cfg: &Config) -> Vec<(config::Element, String)> {
    use config::Element as E;
    let config_dir = oauth::config_dir();

    let model_name_raw = input
        .pointer("/model/display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Claude");
    let model_name = shorten_model_name(model_name_raw);

    let size = input
        .pointer("/context_window/context_window_size")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0)
        .unwrap_or(200_000);

    let usage = input.pointer("/context_window/current_usage");
    let input_tokens = u64_at(usage, "input_tokens");
    let cache_create = u64_at(usage, "cache_creation_input_tokens");
    let cache_read = u64_at(usage, "cache_read_input_tokens");
    let current = input_tokens + cache_create + cache_read;

    let used_str = format_tokens(current);
    let total_str = format_tokens(size);
    let pct_used = (current * 100 / size).min(100);

    let cwd = input.pointer("/cwd").and_then(|v| v.as_str()).unwrap_or("");

    let mut out: Vec<(E, String)> = Vec::new();

    out.push((E::Model, format!("{BLUE}{model_name}{RESET}")));

    if cfg.cwd && !cwd.is_empty() {
        let display_dir = cwd.rsplit('/').next().unwrap_or(cwd);
        let mut s = format!("{CYAN}{display_dir}{RESET}");
        if cfg.git {
            if let Some(g) = git::collect(cwd) {
                s.push_str(&format!("{DIM}@{RESET}{GREEN}{}{RESET}", g.branch));
                if g.added + g.deleted > 0 {
                    s.push_str(&format!(
                        " {DIM}({RESET}{GREEN}+{}{RESET} {RED}-{}{RESET}{DIM}){RESET}",
                        g.added, g.deleted
                    ));
                }
            }
        }
        out.push((E::Cwd, s));
    }

    let cols = term::columns(120);
    let transcript_path = input.get("transcript_path").and_then(|v| v.as_str());
    let heatmap_result = if cfg.heatmap {
        heatmap::render(cwd, transcript_path, cols)
    } else {
        None
    };

    if cfg.tokens {
        let cache_pct = {
            let denom = input_tokens + cache_create + cache_read;
            if denom > 0 {
                (cache_read * 100 / denom) as i64
            } else {
                0
            }
        };
        let cache_color = match cache_pct {
            p if p >= 80 => GREEN,
            p if p >= 50 => YELLOW,
            _ => RED,
        };
        let sub_pct: Option<i64> = heatmap_result.as_ref().and_then(|r| {
            let total = r.today_main_raw + r.today_sub_raw;
            if total == 0 {
                None
            } else {
                Some((r.today_sub_raw * 100 / total) as i64)
            }
        });
        let mut s = format!(
            "{ORANGE}{used_str}/{total_str}{RESET} {DIM}({RESET}{GREEN}{pct_used}%{RESET} {DIM}·{RESET} {WHITE}cache{RESET} {cache_color}{cache_pct}%{RESET}"
        );
        if let Some(p) = sub_pct {
            s.push_str(&format!(
                " {DIM}·{RESET} {WHITE}sub{RESET} {WHITE}{p}%{RESET}"
            ));
        }
        s.push_str(&format!("{DIM}){RESET}"));
        out.push((E::Tokens, s));
    }

    if cfg.effort {
        let effort = resolve_effort(input, &config_dir);
        let level = match effort.as_str() {
            "low" => format!("{DIM}low{RESET}"),
            "medium" => format!("{ORANGE}med{RESET}"),
            "high" => format!("{GREEN}high{RESET}"),
            "xhigh" => format!("{PURPLE}xhigh{RESET}"),
            "max" => format!("{RED}max{RESET}"),
            other => format!("{GREEN}{other}{RESET}"),
        };
        out.push((E::Effort, format!("effort: {level}")));
    }

    if cfg.limits {
        if let Some(s) = usage::render(input, &config_dir) {
            out.push((E::Limits, s));
        }
    }

    if cfg.cli_version {
        if let Some(v) = cli_version_cached() {
            out.push((E::Version, format!("{ORANGE}v{v}{RESET}")));
        }
    }

    if cfg.updates {
        if let Some(line) = update_check_line() {
            let line = line.strip_prefix('\n').unwrap_or(&line).to_string();
            out.push((E::Updates, line));
        }
    }

    if let Some(rows) = heatmap_result {
        out.push((E::HeatmapMain, rows.main_row));
        out.push((E::HeatmapSub, rows.sub_row));
    }

    out
}

/// Compose the lines printed to Claude's own statusline: the claude-routed
/// segment elements joined on one line, followed by any claude-routed row
/// element (heatmaps) on their own lines.
fn compose_claude(elements: &[(config::Element, String)], routing: &config::Routing) -> String {
    use config::{Dest, Element, Kind};
    let find = |e: Element| {
        elements
            .iter()
            .find(|(el, _)| *el == e)
            .map(|(_, c)| c.as_str())
            .filter(|s| !s.is_empty())
    };
    let segs = Element::ALL
        .iter()
        .copied()
        .filter(|&e| e.kind() == Kind::Segment && routing.dest(e) == Dest::Claude)
        .filter_map(find);
    let mut lines: Vec<String> = Vec::new();
    let seg_line = render_tmux::join_segments(segs);
    if !seg_line.is_empty() {
        lines.push(seg_line);
    }
    for e in [Element::HeatmapMain, Element::HeatmapSub] {
        if routing.dest(e) == Dest::Claude {
            if let Some(c) = find(e) {
                lines.push(c.to_string());
            }
        }
    }
    lines.join("\n")
}

fn u64_at(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|v| v.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

fn resolve_effort(input: &Value, config_dir: &str) -> String {
    if let Some(s) = input.pointer("/effort/level").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Ok(v) = env::var("CLAUDE_CODE_EFFORT_LEVEL") {
        if !v.is_empty() {
            return v;
        }
    }
    let settings_path = format!("{config_dir}/settings.json");
    if let Ok(text) = fs::read_to_string(&settings_path) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(level) = v.get("effortLevel").and_then(|v| v.as_str()) {
                if !level.is_empty() {
                    return level.to_string();
                }
            }
        }
    }
    "medium".to_string()
}

fn cli_version_cached() -> Option<String> {
    let path = cache::cache_dir().join("statusline-cli-version");
    if let Some(s) = cache::read_if_fresh(&path, Duration::from_secs(3600)) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let out = Command::new("claude").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let version = text.split_whitespace().next()?.to_string();
    let _ = cache::write_atomic(&path, &version);
    Some(version)
}

fn update_check_line() -> Option<String> {
    let cache_path = cache::cache_dir().join("statusline-version-cache.json");
    let mut data = cache::read_if_fresh(&cache_path, Duration::from_secs(86400));
    if data.is_none() {
        let _ = cache::touch(&cache_path);
        if let Some(body) = api::fetch_latest_release() {
            let _ = cache::write_atomic(&cache_path, &body);
            data = Some(body);
        } else {
            cache::remove_if_empty(&cache_path);
        }
    }
    let data = data?;
    let v: Value = serde_json::from_str(&data).ok()?;
    let tag = v.get("tag_name").and_then(|t| t.as_str())?;
    if version_gt(tag.trim_start_matches('v'), SELF_VERSION) {
        Some(format!(
            "\n{DIM}Update available: {tag} → Tell Claude: \"Find my installed status bar and update it\"{RESET}"
        ))
    } else {
        None
    }
}

fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let mut it = s.split('.');
        (
            it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        )
    };
    parse(a) > parse(b)
}
