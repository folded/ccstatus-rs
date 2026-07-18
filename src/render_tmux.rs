//! tmux-side rendering: ANSI→tmux-format translation.
//!
//! Element content is produced as raw ANSI (by the registrar's renderer or,
//! for `warmth`, [`crate::render::warmth_segment`]). The daemon joins the
//! elements routed to a surface (via [`crate::render::join_segments`]) and
//! translates the result here to tmux format strings, since tmux re-parses
//! `status-format` for `#[…]` directives and would otherwise print escape
//! soup. The backend-neutral composition helpers live in [`crate::render`].

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
        // tmux expands status strings through strftime, so a literal `%` is a
        // date specifier (eaten, or `%H`/`%m`/… inject the time). Double it.
        if b == b'%' {
            out.push_str("%%");
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i] != 0x1b && bytes[i] != b'#' && bytes[i] != b'%' {
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
    fn ansi_escapes_literal_percent() {
        // tmux strftime-expands status strings, so % must be doubled.
        assert_eq!(ansi_to_tmux("22% cache"), "22%% cache");
        assert_eq!(ansi_to_tmux("\x1b[2m100%\x1b[0m"), "#[dim]100%%#[default]");
    }

    #[test]
    fn ansi_drops_unknown_sgr() {
        assert_eq!(ansi_to_tmux("\x1b[33mx\x1b[0m"), "x#[default]");
    }
}
