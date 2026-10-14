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

/// Make a payload string safe to embed in a rendered row: every control
/// character (newline, tab, and ESC, so a payload can't inject its own escape
/// sequences) becomes a space, then the ends are trimmed.
pub fn plain_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Shorten `s` to at most `max_chars` characters. When it has to cut, the
/// result ends in `…`, which counts against the budget, and a space left
/// dangling before the ellipsis is dropped. Counts chars, not display columns.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let kept: String = s.chars().take(max_chars - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_replaces_controls_and_trims() {
        assert_eq!(plain_text("a\nb\tc"), "a b c");
        assert_eq!(plain_text("\x1b[31mred\x1b[0m"), "[31mred [0m");
        assert_eq!(plain_text("  \n "), "");
        assert_eq!(plain_text("  keep inner  spaces "), "keep inner  spaces");
        assert_eq!(plain_text("héllo ✓"), "héllo ✓");
    }

    #[test]
    fn truncate_chars_fits_unchanged() {
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello", 9), "hello");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn truncate_chars_counts_the_ellipsis_in_the_budget() {
        assert_eq!(truncate_chars("hello world", 8), "hello w…");
        assert_eq!(truncate_chars("hello world", 8).chars().count(), 8);
        assert_eq!(truncate_chars("hello", 1), "…");
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn truncate_chars_trims_a_space_before_the_ellipsis() {
        assert_eq!(truncate_chars("hello world", 7), "hello…");
        // multibyte chars count one each and are never split mid-char.
        assert_eq!(truncate_chars("ééééé", 3), "éé…");
    }
}
