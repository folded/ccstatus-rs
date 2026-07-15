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
        if let Some(token) = oauth::get_oauth_token(config_dir)
            && let Some(body) = api::fetch_usage(&token)
        {
            let _ = cache::write_atomic(&cache_path, &body);
            usage_data = Some(body);
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
            if let Some(epoch) = builtin_5h_reset.and_then(value_as_epoch)
                && let Some(s) = format_local(epoch, "%H:%M")
            {
                push_fmt(&mut out, format_args!(" {DIM}@{s}{RESET}"));
            }
        }
        let pct_7d = builtin_7d_pct
            .and_then(|v| v.as_f64())
            .map(|n| n.round() as i64);
        let reset_7d = builtin_7d_reset.and_then(value_as_epoch);
        let scoped = usage_json.map(scoped_limits).unwrap_or_default();
        push_weekly(&mut out, &sep, pct_7d, reset_7d, scoped);
        if let Some(data) = usage_json {
            render_extra_usage(data, &sep, &mut out);
        }
    } else if let Some(data) = usage_json {
        if let Ok(v) = serde_json::from_str::<Value>(data)
            && v.get("five_hour").is_some()
        {
            let pct_5h = v
                .pointer("/five_hour/utilization")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0)
                .round() as i64;
            let iso_5h = v.pointer("/five_hour/resets_at").and_then(|x| x.as_str());
            let c = usage_color(pct_5h);
            out.push_str(&sep);
            push_fmt(
                &mut out,
                format_args!("{WHITE}5h{RESET} {c}{pct_5h}%{RESET}"),
            );
            if let Some(reset) = iso_5h
                .and_then(iso_to_epoch)
                .and_then(|e| format_local(e, "%H:%M"))
            {
                push_fmt(&mut out, format_args!(" {DIM}@{reset}{RESET}"));
            }

            let pct_7d = v
                .pointer("/seven_day/utilization")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0)
                .round() as i64;
            let reset_7d = v
                .pointer("/seven_day/resets_at")
                .and_then(|x| x.as_str())
                .and_then(iso_to_epoch);
            push_weekly(&mut out, &sep, Some(pct_7d), reset_7d, scoped_limits(data));

            render_extra_usage(data, &sep, &mut out);
            return out;
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
            v.and_then(|x| x.as_f64())
                .map(|n| n.round() as i64 != 0)
                .unwrap_or(false)
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

/// Display format for weekly reset times (`7d` and scoped quotas alike).
const WEEKLY_RESET_FMT: &str = "%a %b %-d, %H:%M";

/// Scoped resets within this of the 7d reset count as "the same window".
/// The live endpoint reports the same weekly boundary with seconds of
/// jitter across entries (`20:59:59` vs `21:00:00.47`); genuinely distinct
/// windows differ by hours or days.
const RESET_GROUP_TOLERANCE_SECS: i64 = 60;

/// A model-scoped quota from the API's `limits` array — a per-model window
/// with no key of its own (e.g. Fable's weekly limit, `kind:
/// "weekly_scoped"` with `scope.model.display_name: "Fable"`).
struct ScopedLimit {
    label: String,
    pct: i64,
    /// Reset epoch, kept numeric so grouping can tolerate reporting jitter.
    reset: Option<i64>,
}

/// Parse the model-scoped entries of a usage payload. Unscoped entries
/// (`session`, `weekly_all`) duplicate the 5h/7d segments and are skipped.
fn scoped_limits(data: &str) -> Vec<ScopedLimit> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    let Some(limits) = v.get("limits").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    limits
        .iter()
        .filter_map(|entry| {
            let label = entry
                .pointer("/scope/model/display_name")?
                .as_str()?
                .trim()
                .to_string();
            if label.is_empty() {
                return None;
            }
            let pct = entry.get("percent")?.as_f64()?.round() as i64;
            let reset = entry
                .get("resets_at")
                .and_then(|x| x.as_str())
                .and_then(iso_to_epoch);
            Some(ScopedLimit { label, pct, reset })
        })
        .collect()
}

/// Render the weekly cluster: the account-wide `7d` window with any scoped
/// quotas resetting at (tolerably) the same moment folded into the same
/// segment — `7d 10%; Fable 7% @Thu Jul 16, 07:00`, one `@` for the group,
/// wearing the 7d reset — and the remaining scoped quotas standalone with
/// their own reset.
fn push_weekly(
    out: &mut String,
    sep: &str,
    pct_7d: Option<i64>,
    reset_7d: Option<i64>,
    scoped: Vec<ScopedLimit>,
) {
    let grouped = |s: &ScopedLimit| match (reset_7d, s.reset) {
        (Some(a), Some(b)) => (a - b).abs() <= RESET_GROUP_TOLERANCE_SECS,
        _ => false,
    };
    let mut rest = Vec::new();
    if let Some(p) = pct_7d {
        let c = usage_color(p);
        out.push_str(sep);
        push_fmt(out, format_args!("{WHITE}7d{RESET} {c}{p}%{RESET}"));
        for s in scoped {
            if grouped(&s) {
                let c = usage_color(s.pct);
                push_fmt(
                    out,
                    format_args!(
                        "{DIM};{RESET} {WHITE}{}{RESET} {c}{}%{RESET}",
                        s.label, s.pct
                    ),
                );
            } else {
                rest.push(s);
            }
        }
        if let Some(r) = reset_7d.and_then(|e| format_local(e, WEEKLY_RESET_FMT)) {
            push_fmt(out, format_args!(" {DIM}@{r}{RESET}"));
        }
    } else {
        rest = scoped;
    }
    for s in rest {
        let c = usage_color(s.pct);
        out.push_str(sep);
        push_fmt(
            out,
            format_args!("{WHITE}{}{RESET} {c}{}%{RESET}", s.label, s.pct),
        );
        if let Some(r) = s.reset.and_then(|e| format_local(e, WEEKLY_RESET_FMT)) {
            push_fmt(out, format_args!(" {DIM}@{r}{RESET}"));
        }
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
    if !extra
        .get("is_enabled")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
    {
        return;
    }
    let pct = extra
        .get("utilization")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0)
        .round() as i64;
    let used_cents = extra
        .get("used_credits")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let limit_cents = extra
        .get("monthly_limit")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let color = usage_color(pct);

    let has_used = extra
        .get("used_credits")
        .map(|v| v.is_number())
        .unwrap_or(false);
    let has_limit = extra
        .get("monthly_limit")
        .map(|v| v.is_number())
        .unwrap_or(false);
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
        push_fmt(
            out,
            format_args!("{WHITE}extra{RESET} {GREEN}enabled{RESET}"),
        );
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

    // Carry the API-only sections through the normalisation: the builtin
    // input has no equivalent of extra-usage or the scoped `limits` array,
    // and dropping them here would blank those segments until the next
    // real fetch.
    let prior_v = prior.and_then(|s| serde_json::from_str::<Value>(s).ok());
    let carry = |k: &str| {
        prior_v
            .as_ref()
            .and_then(|v| v.get(k).cloned())
            .unwrap_or(Value::Null)
    };

    let v = serde_json::json!({
        "five_hour": {
            "utilization": pct_5h,
            "resets_at": reset_5h.and_then(epoch_to_iso_utc),
        },
        "seven_day": {
            "utilization": pct_7d,
            "resets_at": reset_7d.and_then(epoch_to_iso_utc),
        },
        "extra_usage": carry("extra_usage"),
        "limits": carry("limits"),
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

/// The prompt-cache TTL (seconds) Anthropic granted this session, inferred from
/// the most recent turn that wrote cache in the transcript: `3600` for the
/// 1-hour ephemeral tier (subscription plans), `300` for the 5-minute tier (API
/// keys). `None` if no cache-writing turn is found. Reads only the transcript
/// tail — the latest turn is at the end. Subagent requests use the 5m tier, but
/// they live in their own transcripts, so the main session's tier is unaffected.
pub fn detect_cache_ttl_secs(transcript_path: &str) -> Option<i64> {
    let tail = read_tail(transcript_path, 256 * 1024)?;
    // Scan complete lines newest-first; a truncated leading line just fails to
    // parse and is skipped.
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(cc) = v.pointer("/message/usage/cache_creation")
            && let Some(ttl) = ttl_from_cache_creation(cc)
        {
            return Some(ttl);
        }
    }
    None
}

/// Pure: map a turn's `cache_creation` ephemeral breakdown to a TTL — `3600` if
/// it wrote to the 1-hour tier, `300` if only the 5-minute tier, `None` if it
/// wrote no cache this turn (so the scan continues to an earlier turn).
fn ttl_from_cache_creation(cc: &Value) -> Option<i64> {
    let field = |k| cc.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    if field("ephemeral_1h_input_tokens") > 0 {
        Some(3600)
    } else if field("ephemeral_5m_input_tokens") > 0 {
        Some(300)
    } else {
        None
    }
}

/// Read the last `max` bytes of a file as (lossy) UTF-8, or `None` if it can't
/// be opened. Lossy decoding tolerates a multibyte char split at the seek point.
fn read_tail(path: &str, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_ttl_from_tier() {
        // 1h tier present -> subscription, 3600.
        assert_eq!(
            ttl_from_cache_creation(
                &json!({"ephemeral_1h_input_tokens": 26014, "ephemeral_5m_input_tokens": 0})
            ),
            Some(3600)
        );
        // Only the 5m tier -> API-key, 300.
        assert_eq!(
            ttl_from_cache_creation(
                &json!({"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 26014})
            ),
            Some(300)
        );
        // 1h wins if both are non-zero.
        assert_eq!(
            ttl_from_cache_creation(
                &json!({"ephemeral_1h_input_tokens": 10, "ephemeral_5m_input_tokens": 10})
            ),
            Some(3600)
        );
        // No cache written this turn -> keep scanning.
        assert_eq!(
            ttl_from_cache_creation(&json!({"ephemeral_1h_input_tokens": 0})),
            None
        );
    }

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
    fn scoped_limits_render_model_percent() {
        // Shape from the live oauth/usage endpoint: unscoped entries mirror
        // 5h/7d; the scoped one is Fable's own weekly quota.
        let usage = json!({
            "five_hour": { "utilization": 31.0 },
            "seven_day": { "utilization": 9.0 },
            "limits": [
                { "kind": "session", "group": "session", "percent": 31, "scope": null },
                { "kind": "weekly_all", "group": "weekly", "percent": 9, "scope": null },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 6,
                  "resets_at": "2026-07-15T21:00:00.473515+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } }
            ]
        })
        .to_string();
        // API branch (no builtin input).
        let s = format_segment(&json!({}), Some(&usage));
        assert!(s.contains("Fable"));
        assert!(s.contains("6%"));
        // The scoped window's own reset time renders (the only resets_at in
        // this payload — fractional-seconds ISO form from the live endpoint).
        assert!(s.contains('@'));
        // Unscoped entries don't render twice: exactly one 9% (the 7d).
        assert_eq!(s.matches("9%").count(), 1);

        // Builtin branch renders it too.
        let input = json!({
            "rate_limits": {
                "five_hour": { "used_percentage": 31.0, "resets_at": 1_700_000_000 },
                "seven_day": { "used_percentage": 9.0, "resets_at": 1_700_500_000 }
            }
        });
        let s = format_segment(&input, Some(&usage));
        assert!(s.contains("Fable"));
        assert!(s.contains("6%"));
    }

    #[test]
    fn scoped_limit_groups_into_7d_when_displayed_reset_matches() {
        // API branch: the scoped reset differs from 7d's by a second of
        // reporting jitter (observed live: 20:59:59 vs 21:00:00.47) ->
        // same window -> folded into the 7d segment with a single shared
        // `@` (`7d 9%; Fable 6% @...`).
        let usage = json!({
            "five_hour": { "utilization": 31.0 },
            "seven_day": { "utilization": 9.0,
                           "resets_at": "2026-07-15T21:00:00.473026+00:00" },
            "limits": [
                { "kind": "weekly_scoped", "percent": 6,
                  "resets_at": "2026-07-15T20:59:59+00:00",
                  "scope": { "model": { "display_name": "Fable" } } }
            ]
        })
        .to_string();
        let s = format_segment(&json!({}), Some(&usage));
        assert!(s.contains("Fable"));
        assert!(s.contains(';')); // grouped, not a standalone segment
        assert_eq!(s.matches('@').count(), 1); // one shared weekly reset

        // Builtin branch: stdin epoch matches the scoped ISO reset (same
        // second) -> grouped there too. 5h contributes the second `@`.
        let input = json!({
            "rate_limits": {
                "five_hour": { "used_percentage": 31.0, "resets_at": 1_700_000_000 },
                "seven_day": { "used_percentage": 9.0, "resets_at": 1_784_149_200 }
            }
        });
        let usage = json!({
            "limits": [
                { "kind": "weekly_scoped", "percent": 6,
                  "resets_at": "2026-07-15T21:00:00.473515+00:00",
                  "scope": { "model": { "display_name": "Fable" } } }
            ]
        })
        .to_string();
        let s = format_segment(&input, Some(&usage));
        assert!(s.contains("Fable"));
        assert!(s.contains(';'));
        assert_eq!(s.matches('@').count(), 2); // 5h + the shared weekly
    }

    #[test]
    fn scoped_limits_absent_or_null_render_nothing() {
        let usage = json!({
            "five_hour": { "utilization": 1.0 },
            "limits": [
                { "kind": "weekly_scoped", "percent": 6, "scope": { "model": null } },
                { "kind": "weekly_scoped", "scope": { "model": { "display_name": "X" } } }
            ]
        })
        .to_string();
        // No display_name -> skipped; no percent -> skipped.
        let s = format_segment(&json!({}), Some(&usage));
        assert!(!s.contains('X'));
        assert!(!s.contains("6%"));
    }

    #[test]
    fn builtin_cache_write_carries_limits_and_extra() {
        let dir = std::env::temp_dir().join(format!("ccstatus-test-limits-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let prior = json!({
            "extra_usage": { "is_enabled": true },
            "limits": [ { "kind": "weekly_scoped", "percent": 6,
                          "scope": { "model": { "display_name": "Fable" } } } ]
        })
        .to_string();
        let input = json!({
            "rate_limits": {
                "five_hour": { "used_percentage": 31.0, "resets_at": 1_700_000_000 },
                "seven_day": { "used_percentage": 9.0, "resets_at": 1_700_500_000 }
            }
        });
        write_builtin_cache(&input, &path, Some(&prior));
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written.pointer("/limits/0/scope/model/display_name"),
            Some(&json!("Fable"))
        );
        assert_eq!(
            written.pointer("/extra_usage/is_enabled"),
            Some(&json!(true))
        );
        let _ = fs::remove_dir_all(&dir);
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
