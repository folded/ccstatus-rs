use std::fmt::Write;

pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        let rounded = (v * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            format!("{}m", rounded as i64)
        } else {
            format!("{:.1}m", rounded)
        }
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Strip "(1M context)" trailing context tag, replacing with " 1M".
pub fn shorten_model_name(name: &str) -> String {
    if let Some(start) = name.find(" (") {
        let rest = &name[start + 2..];
        if let Some(end) = rest.find(" context)") {
            let inside = &rest[..end];
            if !inside.is_empty()
                && inside
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || matches!(c, 'k' | 'K' | 'm' | 'M'))
            {
                let mut out = String::with_capacity(name.len());
                out.push_str(&name[..start]);
                out.push(' ');
                out.push_str(inside);
                return out;
            }
        }
    }
    name.to_string()
}

pub fn push_fmt(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}

/// `s` cut to at most `max` characters, the last being `…` when anything was
/// dropped. Counts chars, not columns, like [`crate::render::visible_width`].
/// `max` must be at least 1.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    // A cut that lands after a space would leave "word …".
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_leaves_short_strings_alone() {
        assert_eq!(truncate_chars("limits refactor", 32), "limits refactor");
        assert_eq!(truncate_chars("", 32), "");
        // exactly at the cap is not truncated.
        assert_eq!(truncate_chars("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_chars_caps_total_length_including_the_ellipsis() {
        let out = truncate_chars(&"a".repeat(40), 32);
        assert_eq!(out.chars().count(), 32);
        assert_eq!(out, format!("{}…", "a".repeat(31)));
        assert_eq!(truncate_chars("abcdef", 5), "abcd…");
    }

    #[test]
    fn truncate_chars_counts_chars_not_bytes() {
        // 6 two-byte chars: a byte count would cut mid-character.
        assert_eq!(truncate_chars("ééééééé", 6), "ééééé…");
    }

    #[test]
    fn truncate_chars_drops_a_trailing_space_before_the_ellipsis() {
        assert_eq!(truncate_chars("abc defgh", 5), "abc…");
    }
}
