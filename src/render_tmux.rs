//! tmux-side renderer (`ccstatus --render-tmux <flavor> <pane_id>`).
//!
//! Invoked by tmux every `status-interval`. Reads `/tmp/claude/pane/<id>.json`
//! and the joined session file, then emits a one-line summary suitable for
//! `status-format` (or, eventually, `pane-border-format` / a pane title).
//!
//! Output uses tmux format strings (`#[fg=…]`) rather than raw ANSI: tmux
//! re-parses `#(…)` shell output for format directives, and would strip raw
//! ANSI unless the user has configured passthrough.

use std::process::ExitCode;

use crate::color::{DIM, GREEN, RED, RESET, WHITE};
use crate::format::shorten_model_name;
use crate::state::{self, PaneState, SessionState};
use crate::tmux;
use crate::util::now_unix;

/// Threshold at which we flip the indicator from `warm` to `cold`. Sits a
/// little under Claude's documented ~5-minute prompt-cache TTL so the user
/// sees the transition before they're about to pay for a re-warm.
const WARM_THRESHOLD_SECS: i64 = 270;

#[derive(Debug, Clone, Copy)]
pub enum RenderFlavor {
    /// Compact warm/cold/idle line using tmux format strings.
    Row,
    /// Nth pre-rendered line from pane state (raw ANSI, as produced by
    /// the standard render). Line 0 additionally appends the current
    /// warmth indicator.
    Line(usize),
}

pub fn run(flavor: RenderFlavor, pane_id: &str) -> ExitCode {
    let line = build_line(flavor, pane_id);
    if !line.is_empty() {
        print!("{line}");
    }
    ExitCode::SUCCESS
}

fn build_line(flavor: RenderFlavor, pane_id: &str) -> String {
    let Some(server_id) = tmux::server_id() else {
        return String::new();
    };
    let Some(pane) = state::read_pane(&server_id, pane_id) else {
        return String::new();
    };
    let session = state::read_session(&pane.session_id).unwrap_or_default();
    match flavor {
        RenderFlavor::Row => format_row(&pane, &session),
        RenderFlavor::Line(n) => format_stashed_line(&pane, &session, n),
    }
}

/// Pull the Nth pre-rendered line. Line 0 (the rich line) gets warmth
/// appended; subsequent lines (heatmap rows) are emitted verbatim.
fn format_stashed_line(pane: &PaneState, session: &SessionState, n: usize) -> String {
    let Some(base) = pane.lines.get(n) else {
        return String::new();
    };
    if n == 0 {
        let mut out = base.clone();
        if let Some(suffix) = warmth_ansi_suffix(session) {
            out.push_str(&suffix);
        }
        out
    } else {
        base.clone()
    }
}

fn warmth_ansi_suffix(session: &SessionState) -> Option<String> {
    let ts = session.last_turn_ts?;
    let idle = (now_unix() - ts).max(0);
    let (label, color) = if idle < WARM_THRESHOLD_SECS {
        ("warm", GREEN)
    } else {
        ("cold", RED)
    };
    Some(format!(
        " {DIM}|{RESET} {WHITE}idle{RESET} {}  {DIM}|{RESET} {color}{label}{RESET}",
        format_duration(idle)
    ))
}

fn format_row(_pane: &PaneState, session: &SessionState) -> String {
    let mut out = String::with_capacity(96);
    out.push_str("#[fg=blue]claude#[default]");

    if let Some(model) = &session.model {
        let short = shorten_model_name(model);
        push_sep(&mut out);
        out.push_str("#[fg=cyan]");
        out.push_str(&escape(&short));
        out.push_str("#[default]");
    }

    push_sep(&mut out);
    out.push_str(&format!("t{}", session.turn_count));

    if let Some(ts) = session.last_turn_ts {
        let idle = (now_unix() - ts).max(0);
        let (warmth, color) = if idle < WARM_THRESHOLD_SECS {
            ("warm", "green")
        } else {
            ("cold", "red")
        };
        push_sep(&mut out);
        out.push_str(&format!("idle {}", format_duration(idle)));
        push_sep(&mut out);
        out.push_str(&format!("#[fg={color}]{warmth}#[default]"));
    }

    out
}

fn push_sep(out: &mut String) {
    out.push_str(" #[fg=brightblack]|#[default] ");
}

fn format_duration(secs: i64) -> String {
    let m = secs / 60;
    let s = secs % 60;
    format!("{m:02}:{s:02}")
}

/// Escape `#` so tmux doesn't reinterpret literal text as a format
/// directive. Anything else passes through unchanged.
fn escape(s: &str) -> String {
    s.replace('#', "##")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_mm_ss() {
        assert_eq!(format_duration(0), "00:00");
        assert_eq!(format_duration(59), "00:59");
        assert_eq!(format_duration(60), "01:00");
        assert_eq!(format_duration(125), "02:05");
        assert_eq!(format_duration(3725), "62:05");
    }

    #[test]
    fn escape_doubles_hash() {
        assert_eq!(escape("Claude #4"), "Claude ##4");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn row_when_no_tmux_env_is_empty() {
        // build_line short-circuits when $TMUX is unset; running tests
        // outside of tmux should produce no output regardless of pane id.
        let saved = std::env::var("TMUX").ok();
        // Safety: tests run single-threaded by default for env mutation here.
        unsafe { std::env::remove_var("TMUX") };
        let out = build_line(RenderFlavor::Row, "%nonexistent-pane-xyz");
        if let Some(v) = saved {
            unsafe { std::env::set_var("TMUX", v) };
        }
        assert_eq!(out, "");
    }
}
