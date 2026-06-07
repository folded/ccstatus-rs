mod api;
mod cache;
mod cli;
mod color;
mod format;
mod git;
mod heatmap;
mod hooks;
mod install;
mod oauth;
mod render_tmux;
mod state;
mod term;
mod tmux;
mod util;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::Value;

use cli::{Config, ParseOutcome};
use color::*;
use format::{format_tokens, push, push_fmt, shorten_model_name};
use state::PaneState;
use util::{now_unix, resolve_session_id};

const SELF_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let cfg = match cli::parse_args(env::args().skip(1)) {
        ParseOutcome::Run(c) => c,
        ParseOutcome::Hook(kind) => return hooks::run(kind),
        ParseOutcome::Render(flavor, pane_id) => return render_tmux::run(flavor, &pane_id),
        ParseOutcome::TmuxOnFocus(hint) => return tmux::on_focus(hint.as_deref()),
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

    let out = render(&input, &cfg);

    if let Some(pane_id) = active_tmux_pane() {
        let lines: Vec<String> = out.split('\n').map(str::to_string).collect();
        register_pane(&input, &pane_id, lines);
        // tmux owns the visible display via its own status rows; emit
        // nothing here so the Claude statusline row stays clear.
        return ExitCode::SUCCESS;
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(out.as_bytes());
    ExitCode::SUCCESS
}

/// Returns `Some(pane_id)` if Claude Code was launched inside tmux. Both
/// `$TMUX` and `$TMUX_PANE` must be set; either one missing means we render
/// for stdout as usual.
fn active_tmux_pane() -> Option<String> {
    env::var("TMUX").ok().filter(|s| !s.is_empty())?;
    env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())
}

/// Write per-pane state so the tmux-side renderer (running on
/// `status-interval`) can show a live indicator. Coalesces writes that
/// arrive within 500 ms of an identical prior write so a streaming
/// statusline call doesn't hammer the filesystem.
fn register_pane(input: &Value, pane_id: &str, lines: Vec<String>) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());
    let claude_pid = resolve_claude_pid();
    let pane_tty = query_pane_tty(pane_id);
    let transcript_path = input
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    if let Some(existing) = state::read_pane(&server_id, pane_id) {
        if existing.session_id == session_id
            && existing.claude_pid == claude_pid
            && existing.lines == lines
            && pane_recent(&server_id, pane_id, Duration::from_millis(500))
        {
            return;
        }
    }

    let pane_state = PaneState {
        session_id,
        claude_pid,
        pane_tty,
        transcript_path,
        registered_at: now_unix(),
        last_warmth: None,
        lines,
    };
    let _ = state::write_pane(&server_id, pane_id, &pane_state);
}

/// Walk up the process tree from our parent until `comm` matches `claude`,
/// then return that pid. Falls back to `$PPID` after a bounded number of
/// hops. Bound prevents pathological loops on weird systems.
fn resolve_claude_pid() -> u32 {
    let ppid = unsafe { libc::getppid() } as u32;
    let mut pid = ppid;
    for _ in 0..8 {
        if process_is_claude(pid) {
            return pid;
        }
        match parent_of(pid) {
            Some(p) if p > 1 => pid = p,
            _ => break,
        }
    }
    ppid
}

fn process_is_claude(pid: u32) -> bool {
    let Some(comm) = ps_field(pid, "comm=") else {
        return false;
    };
    let leaf = comm.rsplit('/').next().unwrap_or(&comm);
    leaf == "claude" || leaf.starts_with("claude-") || leaf.starts_with("claude ")
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

fn query_pane_tty(pane_id: &str) -> String {
    let out = Command::new("tmux")
        .args(["display", "-t", pane_id, "-p", "#{pane_tty}"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => String::new(),
    }
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

fn render(input: &Value, cfg: &Config) -> String {
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

    let mut out = String::with_capacity(512);

    push_fmt(&mut out, format_args!("{BLUE}{model_name}{RESET}"));

    if cfg.cwd && !cwd.is_empty() {
        let display_dir = cwd.rsplit('/').next().unwrap_or(cwd);
        push_fmt(&mut out, format_args!(" {DIM}|{RESET} {CYAN}{display_dir}{RESET}"));
        if cfg.git {
            if let Some(g) = git::collect(cwd) {
                push_fmt(&mut out, format_args!("{DIM}@{RESET}{GREEN}{branch}{RESET}", branch = g.branch));
                if g.added + g.deleted > 0 {
                    push_fmt(
                        &mut out,
                        format_args!(
                            " {DIM}({RESET}{GREEN}+{a}{RESET} {RED}-{d}{RESET}{DIM}){RESET}",
                            a = g.added,
                            d = g.deleted
                        ),
                    );
                }
            }
        }
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
            if denom > 0 { (cache_read * 100 / denom) as i64 } else { 0 }
        };
        let cache_color = match cache_pct {
            p if p >= 80 => GREEN,
            p if p >= 50 => YELLOW,
            _ => RED,
        };
        let sub_pct: Option<i64> = heatmap_result.as_ref().and_then(|r| {
            let total = r.today_main_raw + r.today_sub_raw;
            if total == 0 { None } else { Some((r.today_sub_raw * 100 / total) as i64) }
        });

        push_fmt(
            &mut out,
            format_args!(
                " {DIM}|{RESET} {ORANGE}{used_str}/{total_str}{RESET} {DIM}({RESET}{GREEN}{pct_used}%{RESET} {DIM}·{RESET} {WHITE}cache{RESET} {cache_color}{cache_pct}%{RESET}",
            ),
        );
        if let Some(p) = sub_pct {
            push_fmt(&mut out, format_args!(" {DIM}·{RESET} {WHITE}sub{RESET} {WHITE}{p}%{RESET}"));
        }
        push_fmt(&mut out, format_args!("{DIM}){RESET}"));
    }

    if cfg.effort {
        let effort = resolve_effort(input, &config_dir);
        push(&mut out, " ");
        push(&mut out, DIM);
        push(&mut out, "|");
        push(&mut out, RESET);
        push(&mut out, " effort: ");
        match effort.as_str() {
            "low" => push_fmt(&mut out, format_args!("{DIM}low{RESET}")),
            "medium" => push_fmt(&mut out, format_args!("{ORANGE}med{RESET}")),
            "high" => push_fmt(&mut out, format_args!("{GREEN}high{RESET}")),
            "xhigh" => push_fmt(&mut out, format_args!("{PURPLE}xhigh{RESET}")),
            "max" => push_fmt(&mut out, format_args!("{RED}max{RESET}")),
            other => push_fmt(&mut out, format_args!("{GREEN}{other}{RESET}")),
        }
    }

    if cfg.limits {
        render_rate_limits(input, &config_dir, &mut out);
    }

    if cfg.cli_version {
        if let Some(v) = cli_version_cached() {
            push_fmt(&mut out, format_args!(" {DIM}|{RESET} {ORANGE}v{v}{RESET}"));
        }
    }

    if cfg.updates {
        if let Some(update_line) = update_check_line() {
            out.push_str(&update_line);
        }
    }

    if let Some(rows) = heatmap_result {
        out.push('\n');
        out.push_str(&rows.main_row);
        out.push('\n');
        out.push_str(&rows.sub_row);
    }

    out
}

fn u64_at(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|v| v.get(key)).and_then(|v| v.as_u64()).unwrap_or(0)
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

fn render_rate_limits(input: &Value, config_dir: &str, out: &mut String) {
    let sep = format!(" {DIM}|{RESET} ");

    let builtin_5h_pct = input.pointer("/rate_limits/five_hour/used_percentage");
    let builtin_5h_reset = input.pointer("/rate_limits/five_hour/resets_at");
    let builtin_7d_pct = input.pointer("/rate_limits/seven_day/used_percentage");
    let builtin_7d_reset = input.pointer("/rate_limits/seven_day/resets_at");

    let use_builtin = builtin_5h_pct.is_some() || builtin_7d_pct.is_some();
    let effective_builtin = use_builtin && {
        let nonzero = |v: Option<&Value>| -> bool {
            v.and_then(|x| x.as_f64()).map(|n| n.round() as i64 != 0).unwrap_or(false)
        };
        let has_reset = |v: Option<&Value>| -> bool {
            match v {
                Some(Value::String(s)) => !s.is_empty() && s != "null",
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
                _ => false,
            }
        };
        nonzero(builtin_5h_pct)
            || nonzero(builtin_7d_pct)
            || has_reset(builtin_5h_reset)
            || has_reset(builtin_7d_reset)
    };

    let cfg_hash = oauth::config_dir_hash(config_dir);
    let cache_path = cache::cache_dir().join(format!("statusline-usage-cache-{cfg_hash}.json"));
    let cache_max_age = Duration::from_secs(60);

    let mut usage_data = cache::read_if_fresh(&cache_path, cache_max_age);
    let needs_refresh = usage_data.is_none();

    if needs_refresh {
        let _ = cache::touch(&cache_path);
        if let Some(token) = oauth::get_oauth_token(config_dir) {
            if let Some(body) = api::fetch_usage(&token) {
                let _ = cache::write_atomic(&cache_path, &body);
                usage_data = Some(body);
            }
        }
        cache::remove_if_empty(&cache_path);
        if usage_data.is_none() {
            usage_data = cache::read_stale(&cache_path);
        }
    } else if usage_data.is_none() {
        usage_data = cache::read_stale(&cache_path);
    }

    if effective_builtin {
        if let Some(pct) = builtin_5h_pct.and_then(|v| v.as_f64()) {
            let p = pct.round() as i64;
            let c = usage_color(p);
            out.push_str(&sep);
            push_fmt(out, format_args!("{WHITE}5h{RESET} {c}{p}%{RESET}"));
            if let Some(epoch) = builtin_5h_reset.and_then(value_as_epoch) {
                if let Some(s) = format_local(epoch, "%H:%M") {
                    push_fmt(out, format_args!(" {DIM}@{s}{RESET}"));
                }
            }
        }
        if let Some(pct) = builtin_7d_pct.and_then(|v| v.as_f64()) {
            let p = pct.round() as i64;
            let c = usage_color(p);
            out.push_str(&sep);
            push_fmt(out, format_args!("{WHITE}7d{RESET} {c}{p}%{RESET}"));
            if let Some(epoch) = builtin_7d_reset.and_then(value_as_epoch) {
                if let Some(s) = format_local(epoch, "%a %b %-d, %H:%M") {
                    push_fmt(out, format_args!(" {DIM}@{s}{RESET}"));
                }
            }
        }
        if let Some(ref data) = usage_data {
            render_extra_usage(data, &sep, out);
        }
        write_builtin_cache(input, &cache_path, usage_data.as_deref());
    } else if let Some(data) = usage_data.as_deref() {
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if v.get("five_hour").is_some() {
                let pct_5h = v
                    .pointer("/five_hour/utilization")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0)
                    .round() as i64;
                let iso_5h = v.pointer("/five_hour/resets_at").and_then(|x| x.as_str());
                let c = usage_color(pct_5h);
                out.push_str(&sep);
                push_fmt(out, format_args!("{WHITE}5h{RESET} {c}{pct_5h}%{RESET}"));
                if let Some(reset) = iso_5h.and_then(iso_to_epoch).and_then(|e| format_local(e, "%H:%M")) {
                    push_fmt(out, format_args!(" {DIM}@{reset}{RESET}"));
                }

                let pct_7d = v
                    .pointer("/seven_day/utilization")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0)
                    .round() as i64;
                let iso_7d = v.pointer("/seven_day/resets_at").and_then(|x| x.as_str());
                let c7 = usage_color(pct_7d);
                out.push_str(&sep);
                push_fmt(out, format_args!("{WHITE}7d{RESET} {c7}{pct_7d}%{RESET}"));
                if let Some(reset) =
                    iso_7d.and_then(iso_to_epoch).and_then(|e| format_local(e, "%a %b %-d, %H:%M"))
                {
                    push_fmt(out, format_args!(" {DIM}@{reset}{RESET}"));
                }

                render_extra_usage(data, &sep, out);
                return;
            }
        }
        out.push_str(&sep);
        push_fmt(out, format_args!("{WHITE}5h{RESET} {DIM}-{RESET}"));
        out.push_str(&sep);
        push_fmt(out, format_args!("{WHITE}7d{RESET} {DIM}-{RESET}"));
    } else {
        out.push_str(&sep);
        push_fmt(out, format_args!("{WHITE}5h{RESET} {DIM}-{RESET}"));
        out.push_str(&sep);
        push_fmt(out, format_args!("{WHITE}7d{RESET} {DIM}-{RESET}"));
    }
}

fn render_extra_usage(data: &str, sep: &str, out: &mut String) {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    let extra = match v.get("extra_usage") {
        Some(x) if x.is_object() => x,
        _ => return,
    };
    if !extra.get("is_enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
        return;
    }
    let pct = extra
        .get("utilization")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0)
        .round() as i64;
    let used_cents = extra.get("used_credits").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let limit_cents = extra.get("monthly_limit").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let color = usage_color(pct);

    let has_used = extra.get("used_credits").map(|v| v.is_number()).unwrap_or(false);
    let has_limit = extra.get("monthly_limit").map(|v| v.is_number()).unwrap_or(false);
    if has_used && has_limit {
        let used = used_cents / 100.0;
        let limit = limit_cents / 100.0;
        if used == 0.0 && limit == 0.0 {
            return;
        }
        out.push_str(sep);
        push_fmt(
            out,
            format_args!("{WHITE}extra{RESET} {color}${used:.2}/${limit:.2}{RESET}"),
        );
    } else {
        out.push_str(sep);
        push_fmt(out, format_args!("{WHITE}extra{RESET} {GREEN}enabled{RESET}"));
    }
}

fn write_builtin_cache(input: &Value, path: &PathBuf, prior: Option<&str>) {
    let pct_5h = input
        .pointer("/rate_limits/five_hour/used_percentage")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let pct_7d = input
        .pointer("/rate_limits/seven_day/used_percentage")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let reset_5h = input
        .pointer("/rate_limits/five_hour/resets_at")
        .and_then(value_as_epoch);
    let reset_7d = input
        .pointer("/rate_limits/seven_day/resets_at")
        .and_then(value_as_epoch);

    let extra = prior
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("extra_usage").cloned())
        .unwrap_or(Value::Null);

    let v = serde_json::json!({
        "five_hour": {
            "utilization": pct_5h,
            "resets_at": reset_5h.and_then(epoch_to_iso_utc),
        },
        "seven_day": {
            "utilization": pct_7d,
            "resets_at": reset_7d.and_then(epoch_to_iso_utc),
        },
        "extra_usage": extra,
    });
    let _ = cache::write_atomic(path, &v.to_string());
}

fn value_as_epoch(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                Some(n)
            } else {
                iso_to_epoch(s)
            }
        }
        _ => None,
    }
}

fn iso_to_epoch(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    // Fallback: try without timezone, assume UTC.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(Utc.from_utc_datetime(&naive).timestamp());
    }
    None
}

fn epoch_to_iso_utc(epoch: i64) -> Option<String> {
    let dt = Utc.timestamp_opt(epoch, 0).single()?;
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn format_local(epoch: i64, fmt: &str) -> Option<String> {
    let dt = Local.timestamp_opt(epoch, 0).single()?;
    Some(dt.format(fmt).to_string())
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
