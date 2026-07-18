//! Backend-neutral rendering helpers shared by every surface (Claude's own
//! statusline on stdout, the tmux bar, and future backends).
//!
//! Element content is produced as raw ANSI (by the registrar's renderer or,
//! for `warmth`, here). These helpers compose that content — joining segments,
//! measuring visible width, computing the live cache-warmth segment — without
//! committing to how a particular surface emits it. The tmux-specific
//! translation to `status-format` directives lives in [`crate::render_tmux`].

use crate::color::{DIM, GREEN, RED, RESET};
use crate::state::SessionState;
use crate::util::now_unix;

/// Default prompt-cache TTL when a session's tier is still unknown: the
/// conservative 5-minute cache (also correct for API keys).
pub const CACHE_TTL_DEFAULT_SECS: i64 = 300;

/// Seconds of idle after which the warmth indicator flips warm->cold, for a
/// session whose granted cache TTL is `ttl` (see [`SessionState::cache_ttl_secs`]
/// — 300 for the 5-minute cache, 3600 for the 1-hour cache on subscriptions).
/// 90% of the TTL: a margin for clock skew and for the idle clock starting at
/// turn-end rather than request-send. 5m→270s (the historical value); 1h→3240s.
/// The single owner; `daemon` calls it too.
pub fn warm_threshold_secs(ttl: Option<i64>) -> i64 {
    ttl.unwrap_or(CACHE_TTL_DEFAULT_SECS) * 9 / 10
}

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
    let (label, color) = if idle < warm_threshold_secs(session.cache_ttl_secs) {
        ("warm", GREEN)
    } else {
        ("cold", RED)
    };
    Some(format!("{color}{label}{RESET}"))
}

/// The visible (printed) width of an ANSI string: every `\x1b[…m` SGR run is
/// skipped, and the remaining glyphs are counted one column each. Shares the
/// escape-skipping skeleton with [`crate::render_tmux::ansi_to_tmux`]. Correct
/// for the content this crate emits, which is ASCII plus width-1 symbols (`·`,
/// `█`, `─`, …); it does not account for wide (CJK) or zero-width characters, so
/// a directory or branch name containing them would mis-measure by a column or
/// two.
pub fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut w = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i += 1; // skip the terminating 'm' (or land past the end)
        } else {
            let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
            i += ch_len;
            w += 1;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_ignores_sgr_and_counts_glyphs() {
        assert_eq!(visible_width("Claude"), 6);
        assert_eq!(visible_width("\x1b[38;2;0;153;255mClaude\x1b[0m"), 6);
        // dim middle dot: one visible column, escapes ignored.
        assert_eq!(visible_width("\x1b[2m·\x1b[0m"), 1);
        assert_eq!(visible_width(""), 0);
        // an unterminated escape contributes nothing.
        assert_eq!(visible_width("ab\x1b[2"), 2);
    }

    #[test]
    fn join_skips_empties_and_separates() {
        assert_eq!(join_segments(["a", "", "b"]), format!("a{}b", sep()));
        assert_eq!(join_segments(["", ""]), "");
        assert_eq!(join_segments(["solo"]), "solo");
    }
}
