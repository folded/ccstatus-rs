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

use crate::color::{DIM, GREEN, RED, RESET};
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
/// appended; subsequent lines (heatmap rows) are emitted verbatim. The
/// pane-state lines are raw ANSI (produced by the standard render); we
/// translate to tmux format strings here so tmux interprets them as
/// colours instead of printing them as escape-sequence soup.
fn format_stashed_line(pane: &PaneState, session: &SessionState, n: usize) -> String {
    let Some(base) = pane.lines.get(n) else {
        return String::new();
    };
    let combined = if n == 0 {
        match warmth_ansi_suffix(session) {
            Some(suffix) => format!("{base}{suffix}"),
            None => base.clone(),
        }
    } else {
        base.clone()
    };
    ansi_to_tmux(&combined)
}

/// Translate the (small) subset of ANSI SGR sequences emitted by
/// `color.rs` and `format.rs` into tmux format directives. Unrecognised
/// sequences are dropped silently — tmux prints garbage if any leak
/// through, so we'd rather lose colour than corrupt the status row.
///
/// Also escapes literal `#` to `##` so tmux doesn't reinterpret content
/// text as a format-string introducer.
fn ansi_to_tmux(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 32);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            // Scan for terminating 'm'.
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            let body = &input[i + 2..j];
            out.push_str(&sgr_to_tmux(body));
            i = j + 1;
            continue;
        }
        if b == b'#' {
            out.push_str("##");
            i += 1;
            continue;
        }
        // UTF-8 multi-byte chars: copy until next ASCII or escape boundary.
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i] != 0x1b && bytes[i] != b'#' {
            i += 1;
        }
        out.push_str(&input[start..i]);
    }
    out
}

fn sgr_to_tmux(body: &str) -> String {
    let codes: Vec<&str> = body.split(';').collect();
    match codes.as_slice() {
        [""] | ["0"] => "#[default]".to_string(),
        ["1"] => "#[bold]".to_string(),
        ["2"] => "#[dim]".to_string(),
        ["3"] => "#[italics]".to_string(),
        ["4"] => "#[underscore]".to_string(),
        ["7"] => "#[reverse]".to_string(),
        ["38", "2", r, g, b] => rgb_directive("fg", r, g, b),
        ["48", "2", r, g, b] => rgb_directive("bg", r, g, b),
        _ => String::new(),
    }
}

fn rgb_directive(role: &str, r: &str, g: &str, b: &str) -> String {
    match (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>()) {
        (Ok(r), Ok(g), Ok(b)) => format!("#[{role}=#{r:02x}{g:02x}{b:02x}]"),
        _ => String::new(),
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
    Some(format!(" {DIM}|{RESET} {color}{label}{RESET}"))
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
        out.push_str(&format!("#[fg={color}]{warmth}#[default]"));
    }

    out
}

fn push_sep(out: &mut String) {
    out.push_str(" #[fg=brightblack]|#[default] ");
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
    fn escape_doubles_hash() {
        assert_eq!(escape("Claude #4"), "Claude ##4");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn ansi_basic_codes_convert() {
        assert_eq!(ansi_to_tmux("\x1b[0m"), "#[default]");
        assert_eq!(ansi_to_tmux("\x1b[2m"), "#[dim]");
        assert_eq!(
            ansi_to_tmux("\x1b[38;2;0;153;255mClaude\x1b[0m"),
            "#[fg=#0099ff]Claude#[default]"
        );
    }

    #[test]
    fn ansi_passes_utf8_through() {
        // Middle dot used in heatmap rows.
        assert_eq!(
            ansi_to_tmux("\x1b[2m·\x1b[0m"),
            "#[dim]·#[default]"
        );
    }

    #[test]
    fn ansi_escapes_literal_hash() {
        assert_eq!(ansi_to_tmux("turn #42"), "turn ##42");
    }

    #[test]
    fn ansi_drops_unknown_sgr() {
        // Code 33 (yellow basic palette) isn't in our table; drop, don't
        // leak garbage into the row.
        assert_eq!(ansi_to_tmux("\x1b[33mx\x1b[0m"), "x#[default]");
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
