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

/// Append to `out` with no allocation churn. Convenience for building the status line.
pub fn push(out: &mut String, s: &str) {
    out.push_str(s);
}

pub fn push_fmt(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}
