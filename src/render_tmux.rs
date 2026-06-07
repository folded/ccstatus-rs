//! tmux-side rendering helpers: ANSI→tmux-format translation, the inline
//! segment separator, and the live `warmth` segment computed by the daemon.
//!
//! Element content is produced as raw ANSI (by the registrar's renderer or,
//! for `warmth`, here). The daemon joins the elements routed to a surface
//! and translates the result to tmux format strings, since tmux re-parses
//! `status-format` for `#[…]` directives and would otherwise print escape
//! soup.

use crate::color::{DIM, GREEN, RED, RESET};
use crate::state::SessionState;
use crate::util::now_unix;

/// Threshold at which the warmth indicator flips warm->cold. Sits a little
/// under Claude's documented ~5-minute prompt-cache TTL. The single owner;
/// `daemon` reads it from here too.
pub const WARM_THRESHOLD_SECS: i64 = 270;

/// The inline separator between segments on one surface: ` | ` dimmed.
pub fn sep() -> String {
    format!(" {DIM}|{RESET} ")
}

/// Join non-empty segment contents with the inline separator.
pub fn join_segments<'a, I>(parts: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let sep = sep();
    let mut out = String::new();
    for p in parts {
        if p.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(&sep);
        }
        out.push_str(p);
    }
    out
}

/// The live cache-warmth segment as raw ANSI, or `None` when the session has
/// no recorded turn yet. Computed by the daemon each tick so it ticks
/// warm->cold on its own as the prompt cache ages.
pub fn warmth_segment(session: &SessionState) -> Option<String> {
    let ts = session.last_turn_ts?;
    let idle = (now_unix() - ts).max(0);
    let (label, color) = if idle < WARM_THRESHOLD_SECS {
        ("warm", GREEN)
    } else {
        ("cold", RED)
    };
    Some(format!("{color}{label}{RESET}"))
}

/// Translate the (small) subset of ANSI SGR sequences emitted by `color.rs`
/// and `format.rs` into tmux format directives. Unrecognised sequences are
/// dropped silently — tmux prints garbage if any leak through, so we'd
/// rather lose colour than corrupt the status row.
///
/// Also escapes literal `#` to `##` so tmux doesn't reinterpret content text
/// as a format-string introducer.
pub fn ansi_to_tmux(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 32);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && bytes.get(i + 1) == Some(&b'[') {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(ansi_to_tmux("\x1b[2m·\x1b[0m"), "#[dim]·#[default]");
    }

    #[test]
    fn ansi_escapes_literal_hash() {
        assert_eq!(ansi_to_tmux("turn #42"), "turn ##42");
    }

    #[test]
    fn ansi_drops_unknown_sgr() {
        assert_eq!(ansi_to_tmux("\x1b[33mx\x1b[0m"), "x#[default]");
    }

    #[test]
    fn join_skips_empties_and_separates() {
        assert_eq!(join_segments(["a", "", "b"]), format!("a{}b", sep()));
        assert_eq!(join_segments(["", ""]), "");
        assert_eq!(join_segments(["solo"]), "solo");
    }
}
