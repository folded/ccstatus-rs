//! The `limits` element: the rate-limit / quota segment.
//!
//! Owns the OAuth-token fetch, the 60s stampede-guarded usage cache, the
//! builtin-input-vs-API branching, the extra-usage credits, and the two
//! reset-time formats. A deep module matching its siblings `git::collect`
//! and `heatmap::render`.
//!
//! The decision/effect split mirrors `daemon`: `load_usage` is the only part
//! that touches network and filesystem; `format_segment` is pure and is the
//! test surface.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use crate::color::*;
use crate::format::push_fmt;
use crate::render_tmux;
use crate::{api, cache, oauth};

/// The account-global usage snapshot, read from the freshest on-disk usage
/// cache (no network). Account usage is identical across every session, so
/// aggregate surfaces show it once rather than per-session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    pub five_hour_pct: Option<i64>,
    pub seven_day_pct: Option<i64>,
    /// Extra-usage credits in dollars, when enabled with a known balance.
    pub extra_used: Option<f64>,
    pub extra_limit: Option<f64>,
    pub extra_enabled: bool,
}

/// Read the freshest usage cache and parse it into a [`UsageSummary`], or
/// `None` when no usable cache exists. Pure parse ([`parse_summary`]) behind a
/// thin freshest-file read.
pub fn summary() -> Option<UsageSummary> {
    let path = freshest_usage_cache()?;
    let text = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    parse_summary(&v)
}

/// PURE: a usage-cache JSON value -> summary. Requires the `five_hour` key
/// (otherwise it's an error/empty body, not real usage).
fn parse_summary(v: &Value) -> Option<UsageSummary> {
    v.get("five_hour")?;
    let pct = |k: &str| {
        v.pointer(&format!("/{k}/utilization"))
            .and_then(|x| x.as_f64())
            .map(|n| n.round() as i64)
    };
    let extra = v.get("extra_usage");
    let extra_enabled = extra
        .and_then(|e| e.get("is_enabled"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let dollars = |k: &str| {
        extra
            .and_then(|e| e.get(k))
            .and_then(|x| x.as_f64())
            .map(|c| c / 100.0)
    };
    Some(UsageSummary {
        five_hour_pct: pct("five_hour"),
        seven_day_pct: pct("seven_day"),
        extra_used: dollars("used_credits"),
        extra_limit: dollars("monthly_limit"),
        extra_enabled,
    })
}

/// The most recently written `statusline-usage-cache-*.json` (there is one per
/// config dir; the freshest is the most relevant).
fn freshest_usage_cache() -> Option<PathBuf> {
    let dir = cache::cache_dir();
    fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("statusline-usage-cache-") && n.ends_with(".json")
        })
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(path, _)| path)
}

/// The rate-limit / quota segment as raw ANSI, or `None` when empty. Owns the
/// fetch, the cache, the branching, and the formatting.
pub fn render(input: &Value, config_dir: &str) -> Option<String> {
    let usage = load_usage(input, config_dir);
    let s = strip_leading_sep(format_segment(input, usage.as_deref()));
    if s.is_empty() { None } else { Some(s) }
}

/// EFFECT: oauth token -> cache freshness -> fetch -> stale fallback ->
/// `write_builtin_cache`. The only part that touches network/fs.
fn load_usage(input: &Value, config_dir: &str) -> Option<String> {
    let cfg_hash = oauth::config_dir_hash(config_dir);
    let cache_path = cache::cache_dir().join(format!("statusline-usage-cache-{cfg_hash}.json"));
    let cache_max_age = Duration::from_secs(60);

    let mut usage_data = cache::read_if_fresh(&cache_path, cache_max_age);
    if usage_data.is_none() {
        // Stampede guard: touch first so concurrent renders see a fresh
        // mtime and skip the fetch, then fetch and write the body.
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
    }

    // When the input carries usable builtin rate-limits, normalise them (plus
    // any prior extra-usage) into the cache so a later render without builtin
    // data can fall back to it.
    if effective_builtin(input) {
        write_builtin_cache(input, &cache_path, usage_data.as_deref());
    }
    usage_data
}

/// PURE: given the input payload and the (optional) cached usage JSON, build
/// the segment. Covers effective-builtin detection, the builtin branch, the
/// API branch, the `5h - / 7d -` fallback, and extra-usage.
fn format_segment(input: &Value, usage_json: Option<&str>) -> String {
    let sep = render_tmux::sep();
    let mut out = String::new();

    let builtin_5h_pct = input.pointer("/rate_limits/five_hour/used_percentage");
    let builtin_5h_reset = input.pointer("/rate_limits/five_hour/resets_at");
    let builtin_7d_pct = input.pointer("/rate_limits/seven_day/used_percentage");
    let builtin_7d_reset = input.pointer("/rate_limits/seven_day/resets_at");

    if effective_builtin(input) {
        if let Some(pct) = builtin_5h_pct.and_then(|v| v.as_f64()) {
            let p = pct.round() as i64;
            let c = usage_color(p);
            out.push_str(&sep);
            push_fmt(&mut out, format_args!("{WHITE}5h{RESET} {c}{p}%{RESET}"));
            if let Some(epoch) = builtin_5h_reset.and_then(value_as_epoch) {
                if let Some(s) = format_local(epoch, "%H:%M") {
                    push_fmt(&mut out, format_args!(" {DIM}@{s}{RESET}"));
                }
            }
        }
        if let Some(pct) = builtin_7d_pct.and_then(|v| v.as_f64()) {
            let p = pct.round() as i64;
            let c = usage_color(p);
            out.push_str(&sep);
            push_fmt(&mut out, format_args!("{WHITE}7d{RESET} {c}{p}%{RESET}"));
            if let Some(epoch) = builtin_7d_reset.and_then(value_as_epoch) {
                if let Some(s) = format_local(epoch, "%a %b %-d, %H:%M") {
                    push_fmt(&mut out, format_args!(" {DIM}@{s}{RESET}"));
                }
            }
        }
        if let Some(data) = usage_json {
            render_extra_usage(data, &sep, &mut out);
        }
    } else if let Some(data) = usage_json {
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
                push_fmt(&mut out, format_args!("{WHITE}5h{RESET} {c}{pct_5h}%{RESET}"));
                if let Some(reset) = iso_5h.and_then(iso_to_epoch).and_then(|e| format_local(e, "%H:%M")) {
                    push_fmt(&mut out, format_args!(" {DIM}@{reset}{RESET}"));
                }

                let pct_7d = v
                    .pointer("/seven_day/utilization")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0)
                    .round() as i64;
                let iso_7d = v.pointer("/seven_day/resets_at").and_then(|x| x.as_str());
                let c7 = usage_color(pct_7d);
                out.push_str(&sep);
                push_fmt(&mut out, format_args!("{WHITE}7d{RESET} {c7}{pct_7d}%{RESET}"));
                if let Some(reset) =
                    iso_7d.and_then(iso_to_epoch).and_then(|e| format_local(e, "%a %b %-d, %H:%M"))
                {
                    push_fmt(&mut out, format_args!(" {DIM}@{reset}{RESET}"));
                }

                render_extra_usage(data, &sep, &mut out);
                return out;
            }
        }
        push_unknown(&mut out, &sep);
    } else {
        push_unknown(&mut out, &sep);
    }
    out
}

/// The `5h - / 7d -` placeholder shown when no usage data is available.
fn push_unknown(out: &mut String, sep: &str) {
    out.push_str(sep);
    push_fmt(out, format_args!("{WHITE}5h{RESET} {DIM}-{RESET}"));
    out.push_str(sep);
    push_fmt(out, format_args!("{WHITE}7d{RESET} {DIM}-{RESET}"));
}

/// Whether the input's builtin rate-limits are present *and* carry usable
/// signal (a nonzero utilization or a real reset time), as opposed to a
/// zeroed placeholder.
fn effective_builtin(input: &Value) -> bool {
    let p5 = input.pointer("/rate_limits/five_hour/used_percentage");
    let p7 = input.pointer("/rate_limits/seven_day/used_percentage");
    let r5 = input.pointer("/rate_limits/five_hour/resets_at");
    let r7 = input.pointer("/rate_limits/seven_day/resets_at");
    let use_builtin = p5.is_some() || p7.is_some();
    use_builtin && {
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
        nonzero(p5) || nonzero(p7) || has_reset(r5) || has_reset(r7)
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

fn write_builtin_cache(input: &Value, path: &Path, prior: Option<&str>) {
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

/// Strip a single leading inline separator (` | `). The segment is built with
/// a separator before each item; as a standalone element it must not start
/// with one (surface composition adds separators).
fn strip_leading_sep(s: String) -> String {
    let sp = render_tmux::sep();
    s.strip_prefix(sp.as_str()).map(str::to_string).unwrap_or(s)
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
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contains_pct(s: &str) -> bool {
        s.contains('%')
    }

    #[test]
    fn builtin_payload_renders_5h_pct_and_reset() {
        let input = json!({
            "rate_limits": {
                "five_hour": { "used_percentage": 42.0, "resets_at": 1_700_000_000 },
                "seven_day": { "used_percentage": 10.0, "resets_at": 1_700_500_000 }
            }
        });
        let s = format_segment(&input, None);
        assert!(s.contains("5h"));
        assert!(contains_pct(&s));
        assert!(s.contains('@')); // a local reset time was formatted
        assert!(s.contains("7d"));
    }

    #[test]
    fn api_shaped_usage_without_builtin_takes_api_branch() {
        let input = json!({});
        let usage = json!({
            "five_hour": { "utilization": 55.0, "resets_at": "2024-01-01T00:00:00Z" },
            "seven_day": { "utilization": 5.0, "resets_at": "2024-01-07T00:00:00Z" }
        })
        .to_string();
        let s = format_segment(&input, Some(&usage));
        assert!(s.contains("5h"));
        assert!(s.contains("55%"));
        assert!(s.contains("7d"));
    }

    #[test]
    fn both_absent_falls_back_to_dashes() {
        let s = format_segment(&json!({}), None);
        assert!(s.contains("5h"));
        assert!(s.contains("7d"));
        assert!(s.contains('-'));
        assert!(!s.contains('%'));
    }

    #[test]
    fn extra_usage_with_used_and_limit_shows_dollars() {
        let input = json!({});
        let usage = json!({
            "five_hour": { "utilization": 1.0 },
            "extra_usage": {
                "is_enabled": true,
                "utilization": 20.0,
                "used_credits": 150.0,
                "monthly_limit": 1000.0
            }
        })
        .to_string();
        let s = format_segment(&input, Some(&usage));
        assert!(s.contains("extra"));
        assert!(s.contains("$1.50/$10.00"));
    }

    #[test]
    fn extra_usage_enabled_without_credits_shows_enabled() {
        let input = json!({});
        let usage = json!({
            "five_hour": { "utilization": 1.0 },
            "extra_usage": { "is_enabled": true, "utilization": 0.0 }
        })
        .to_string();
        let s = format_segment(&input, Some(&usage));
        assert!(s.contains("extra"));
        assert!(s.contains("enabled"));
    }

    #[test]
    fn parse_summary_reads_pcts_and_extra() {
        let v = json!({
            "five_hour": { "utilization": 62.4 },
            "seven_day": { "utilization": 8.0 },
            "extra_usage": { "is_enabled": true, "used_credits": 150.0, "monthly_limit": 1000.0 }
        });
        let s = parse_summary(&v).unwrap();
        assert_eq!(s.five_hour_pct, Some(62));
        assert_eq!(s.seven_day_pct, Some(8));
        assert!(s.extra_enabled);
        assert_eq!(s.extra_used, Some(1.50));
        assert_eq!(s.extra_limit, Some(10.0));
    }

    #[test]
    fn parse_summary_requires_five_hour() {
        assert!(parse_summary(&json!({ "error": "rate limited" })).is_none());
    }

    #[test]
    fn extra_usage_zeroed_is_omitted() {
        let input = json!({});
        let usage = json!({
            "five_hour": { "utilization": 1.0 },
            "extra_usage": {
                "is_enabled": true,
                "utilization": 0.0,
                "used_credits": 0.0,
                "monthly_limit": 0.0
            }
        })
        .to_string();
        let s = format_segment(&input, Some(&usage));
        assert!(!s.contains("extra"));
    }
}
