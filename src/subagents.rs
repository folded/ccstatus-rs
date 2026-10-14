//! `ccstatus --subagents`: the command behind Claude Code's
//! `subagentStatusLine` setting, which renders the body of each row in the
//! subagent panel below the prompt.
//!
//! Claude runs it once per refresh tick with one JSON object on stdin (the
//! common hook fields, `columns`, and a `tasks` array) and reads one
//! `{"id", "content"}` JSON line per row to override from stdout. A task left
//! out keeps Claude's default row; an empty `content` hides it. The process
//! must exit 0, so every failure here is silent: print nothing and Claude
//! falls back to its defaults.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::color::{BLUE, DIM, GREEN, ORANGE, RED, RESET, usage_color};
use crate::format::{format_tokens, plain_text, truncate_chars};
use crate::render::visible_width;

/// Glyphs for the token sparkline, lowest to highest.
const SPARK_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Below this many columns a truncated label is mostly an ellipsis, so the
/// label is dropped instead.
const MIN_LABEL_CHARS: usize = 8;

pub fn run() -> ExitCode {
    let mut buf = String::new();
    if io::stdin().read_to_string(&mut buf).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(input) = serde_json::from_str::<Value>(&buf) else {
        return ExitCode::SUCCESS;
    };

    let mut out = String::new();
    for (id, content) in render_rows(&input, now_ms()) {
        out.push_str(&json!({ "id": id, "content": content }).to_string());
        out.push('\n');
    }
    // A closed pipe is Claude giving up on this tick, not an error to report.
    let _ = io::stdout().write_all(out.as_bytes());
    ExitCode::SUCCESS
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One `(task id, row body)` per task to override. A task with no string `id`
/// cannot be addressed, and one whose row renders empty would be hidden by
/// Claude (empty `content` means hide), so both are left out and keep the
/// default row. Without a usable `columns` nothing can be fitted, so no rows
/// at all.
pub fn render_rows(input: &Value, now_ms: i64) -> Vec<(String, String)> {
    let Some(columns) = input.get("columns").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(tasks) = input.get("tasks").and_then(Value::as_array) else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter_map(|task| {
            let id = task.get("id")?.as_str()?;
            let row = render_row(task, columns as usize, now_ms);
            (!row.is_empty()).then(|| (id.to_string(), row))
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Failed,
    Killed,
}

impl Status {
    /// Statuses beyond the four documented ones render as a running task, so a
    /// new Claude Code status still shows the full row.
    fn of(task: &Value) -> Self {
        match task.get("status").and_then(Value::as_str) {
            Some("completed") => Self::Completed,
            Some("failed") => Self::Failed,
            Some("killed") => Self::Killed,
            _ => Self::Running,
        }
    }
}

fn sep() -> String {
    format!(" {DIM}·{RESET} ")
}

fn join(parts: &[String]) -> String {
    parts.join(&sep())
}

/// A payload string with control characters flattened, or `None` when absent
/// or blank.
fn text_field(task: &Value, key: &str) -> Option<String> {
    let s = plain_text(task.get(key)?.as_str()?);
    (!s.is_empty()).then_some(s)
}

/// A non-negative whole number field; JSON integers and integral floats both
/// count, since Claude's serializer is not pinned down.
fn count_field(task: &Value, key: &str) -> Option<u64> {
    let v = task.get(key)?;
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

/// `("48k/200k", Some(24))` with a context window, `("48k", None)` without.
/// The percentage is clamped to 100 since a subagent's count can pass the
/// window just before compaction.
fn token_figures(task: &Value) -> Option<(String, Option<i64>)> {
    let tokens = count_field(task, "tokenCount")?;
    match count_field(task, "contextWindowSize").filter(|size| *size > 0) {
        Some(size) => {
            let pct = (tokens.saturating_mul(100) / size).min(100) as i64;
            Some((
                format!("{}/{}", format_tokens(tokens), format_tokens(size)),
                Some(pct),
            ))
        }
        None => Some((format_tokens(tokens), None)),
    }
}

/// One glyph per tick of growth: the difference between consecutive samples
/// on a log scale against the largest difference. `tokenSamples` is a
/// cumulative count, so plotting the samples themselves would just show one
/// step from the initial zero to a flat block; the deltas show whether the
/// subagent is busy right now. The first tick loads the whole prompt (tens of
/// thousands of tokens) while later ticks add hundreds, so a linear scale
/// would flatten those too; the log keeps them visible. A drop (a compaction)
/// counts as no growth. `None` under two numeric samples, where there is no
/// delta to draw.
fn sparkline(task: &Value) -> Option<String> {
    let samples: Vec<f64> = task
        .get("tokenSamples")?
        .as_array()?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    if samples.len() < 2 {
        return None;
    }
    let deltas: Vec<f64> = samples.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    let max = deltas.iter().copied().fold(0.0_f64, f64::max).ln_1p();
    Some(
        deltas
            .iter()
            .map(|d| {
                let level = if max > 0.0 {
                    (d.ln_1p() / max * (SPARK_BARS.len() - 1) as f64).round() as usize
                } else {
                    0
                };
                SPARK_BARS[level]
            })
            .collect(),
    )
}

/// missing `startTime`, or one ahead of the clock, reads `0s`.
fn elapsed(task: &Value, now_ms: i64) -> String {
    let start = task.get("startTime").and_then(Value::as_f64);
    let Some(start) = start.filter(|s| *s <= now_ms as f64) else {
        return "0s".to_string();
    };
    let secs = ((now_ms as f64 - start) / 1000.0) as u64;
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The row body for one task: `[✓|✗ ]lead · label · tokens · sparkline ·
/// elapsed`. Only the label is free text of unbounded length, so it alone is
/// cut to fit `columns`; everything else is fixed-width or short.
fn render_row(task: &Value, columns: usize, now_ms: i64) -> String {
    let status = Status::of(task);
    // A killed task dims the whole row instead of keeping its colours.
    let paint = |color: &str, text: &str| {
        let color = if status == Status::Killed { DIM } else { color };
        format!("{color}{text}{RESET}")
    };

    let mut parts = Vec::new();
    if let Some(lead) = text_field(task, "name")
        .or_else(|| text_field(task, "agentType"))
        .or_else(|| text_field(task, "type"))
    {
        parts.push(paint(BLUE, &lead));
    }
    let label_at = parts.len();
    if let Some((figures, pct)) = token_figures(task) {
        let mut seg = paint(ORANGE, &figures);
        if let Some(pct) = pct {
            seg.push(' ');
            seg.push_str(&paint(usage_color(pct), &format!("{pct}%")));
        }
        parts.push(seg);
    }
    // No name and no token count means the task is too malformed to describe;
    // the elapsed time alone would be a worse row than Claude's default.
    if parts.is_empty() {
        return String::new();
    }
    if status == Status::Running
        && let Some(spark) = sparkline(task)
    {
        parts.push(paint(ORANGE, &spark));
    }
    parts.push(paint(DIM, &elapsed(task, now_ms)));

    let prefix = match status {
        Status::Completed => format!("{GREEN}✓{RESET} "),
        Status::Failed => format!("{RED}✗{RESET} "),
        Status::Running | Status::Killed => String::new(),
    };

    if let Some(label) = text_field(task, "label").or_else(|| text_field(task, "description")) {
        let used = visible_width(&prefix) + visible_width(&join(&parts)) + visible_width(&sep());
        let budget = columns.saturating_sub(used);
        // A label that fits whole is kept at any length; only a cut one needs
        // room to stay readable.
        if label.chars().count() <= budget || budget >= MIN_LABEL_CHARS {
            parts.insert(label_at, paint(DIM, &truncate_chars(&label, budget)));
        }
    }

    format!("{prefix}{}", join(&parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    /// Drop SGR escapes so assertions read as the text a user sees.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn running_task() -> Value {
        json!({
            "id": "t1",
            "name": "surveyor",
            "type": "local_agent",
            "agentType": "Explore",
            "status": "running",
            "description": "Survey fleet state",
            "label": "Reading fleet.rs",
            "startTime": NOW - 72_000,
            "model": "claude-sonnet-5-5",
            "contextWindowSize": 200_000,
            "tokenCount": 48_000,
            "tokenSamples": [1000, 4000, 9000, 20000, 33000, 48000],
            "cwd": "/tmp",
        })
    }

    fn row(task: &Value) -> String {
        render_row(task, 200, NOW)
    }

    #[test]
    fn full_running_task() {
        let r = row(&running_task());
        assert_eq!(
            strip_sgr(&r),
            "surveyor · Reading fleet.rs · 48k/200k 24% · ▇▇███ · 1m12s"
        );
        assert!(r.contains(&format!("{BLUE}surveyor{RESET}")));
        assert!(r.contains(&format!("{DIM}Reading fleet.rs{RESET}")));
        assert!(r.contains(&format!("{ORANGE}48k/200k{RESET} {GREEN}24%{RESET}")));
        assert!(r.contains(&format!("{ORANGE}▇▇███{RESET}")));
        assert!(r.contains(&format!("{DIM}1m12s{RESET}")));
        assert!(r.contains(&format!(" {DIM}·{RESET} ")));
    }

    #[test]
    fn completed_and_failed_get_a_prefix_and_no_sparkline() {
        let mut t = running_task();
        t["status"] = json!("completed");
        let r = row(&t);
        assert!(r.starts_with(&format!("{GREEN}✓{RESET} ")));
        assert!(!strip_sgr(&r).contains('▁'));
        assert!(strip_sgr(&r).starts_with("✓ surveyor"));

        t["status"] = json!("failed");
        let r = row(&t);
        assert!(r.starts_with(&format!("{RED}✗{RESET} ")));
        assert!(!strip_sgr(&r).contains('▁'));
        assert!(strip_sgr(&r).starts_with("✗ surveyor"));
    }

    #[test]
    fn killed_dims_the_whole_row_without_sparkline() {
        let mut t = running_task();
        t["status"] = json!("killed");
        let r = row(&t);
        for colour in [BLUE, ORANGE, GREEN, RED] {
            assert!(!r.contains(colour), "colour leaked into a killed row");
        }
        assert!(!strip_sgr(&r).contains('▁'));
        assert_eq!(
            strip_sgr(&r),
            "surveyor · Reading fleet.rs · 48k/200k 24% · 1m12s"
        );
    }

    #[test]
    fn unknown_status_renders_as_running_without_prefix() {
        let mut t = running_task();
        t["status"] = json!("paused");
        assert_eq!(row(&t), row(&running_task()));
        t.as_object_mut().unwrap().remove("status");
        assert_eq!(row(&t), row(&running_task()));
    }

    #[test]
    fn lead_falls_back_from_name_to_agent_type_to_type() {
        let mut t = running_task();
        assert!(strip_sgr(&row(&t)).starts_with("surveyor · "));

        t["name"] = json!("   ");
        assert!(strip_sgr(&row(&t)).starts_with("Explore · "));

        t.as_object_mut().unwrap().remove("name");
        assert!(strip_sgr(&row(&t)).starts_with("Explore · "));

        t.as_object_mut().unwrap().remove("agentType");
        assert!(strip_sgr(&row(&t)).starts_with("local_agent · "));
    }

    #[test]
    fn missing_context_window_shows_the_bare_count() {
        let mut t = running_task();
        t.as_object_mut().unwrap().remove("contextWindowSize");
        let r = row(&t);
        assert!(strip_sgr(&r).contains(" · 48k · "));
        assert!(r.contains(&format!("{ORANGE}48k{RESET}")));
        assert!(!r.contains('%'));

        // a zero window is as good as none: no division, no percentage.
        t["contextWindowSize"] = json!(0);
        assert!(!row(&t).contains('%'));
    }

    #[test]
    fn percentage_is_clamped_and_coloured_by_usage() {
        let mut t = running_task();
        t["tokenCount"] = json!(250_000);
        let r = row(&t);
        assert!(strip_sgr(&r).contains("250k/200k 100%"));
        assert!(r.contains(&format!("{RED}100%{RESET}")));

        t["tokenCount"] = json!(150_000);
        assert!(row(&t).contains(&format!("{ORANGE}75%{RESET}")));
    }

    #[test]
    fn label_is_truncated_to_fit_the_columns() {
        let mut t = running_task();
        t["label"] = json!("Reading every single file under the fleet module in turn");
        let wide = render_row(&t, 200, NOW);
        assert!(
            strip_sgr(&wide).contains("Reading every single file under the fleet module in turn")
        );

        // Everything but the label is 39 columns and its separator 3 more, so
        // 60 leaves an 18-column label.
        let r = render_row(&t, 60, NOW);
        assert_eq!(visible_width(&r), 60);
        assert!(strip_sgr(&r).contains(" · Reading every sin… · "));
        assert!(strip_sgr(&r).ends_with("1m12s"));

        for columns in 40..=120 {
            let r = render_row(&t, columns, NOW);
            assert!(
                visible_width(&r) <= columns,
                "{columns} columns, row is {}",
                visible_width(&r)
            );
        }
    }

    #[test]
    fn label_is_dropped_when_too_little_room_remains() {
        let mut t = running_task();
        t["label"] = json!("Reading every single file under the fleet module in turn");
        // 39 columns: everything but the label is already 39 wide.
        let r = render_row(&t, 39, NOW);
        assert_eq!(strip_sgr(&r), "surveyor · 48k/200k 24% · ▇▇███ · 1m12s");
        assert!(!strip_sgr(&r).contains('…'));
        // and at an absurdly narrow width the rest is left alone, not cut.
        let r = render_row(&t, 5, NOW);
        assert_eq!(strip_sgr(&r), "surveyor · 48k/200k 24% · ▇▇███ · 1m12s");
    }

    #[test]
    fn a_short_label_survives_a_tight_budget() {
        let mut t = running_task();
        t["label"] = json!("scan");
        // 42 columns go to everything else plus the label's separator. Three
        // spare is under the 8-char minimum and too few for "scan"; four fits
        // it whole.
        let r = render_row(&t, 42 + 3, NOW);
        assert!(!strip_sgr(&r).contains("scan"));
        let r = render_row(&t, 42 + 4, NOW);
        assert!(strip_sgr(&r).contains("scan"));
        assert_eq!(visible_width(&r), 46);
    }

    #[test]
    fn label_falls_back_to_description() {
        let mut t = running_task();
        t.as_object_mut().unwrap().remove("label");
        assert!(strip_sgr(&row(&t)).contains("Survey fleet state"));
        t["label"] = json!("  ");
        assert!(strip_sgr(&row(&t)).contains("Survey fleet state"));
    }

    #[test]
    fn sparkline_plots_growth_per_tick() {
        let spark = |samples: Value| {
            let t = json!({ "tokenSamples": samples });
            sparkline(&t)
        };
        // flat count: no growth, lowest bar per tick.
        assert_eq!(spark(json!([5, 5, 5])).as_deref(), Some("▁▁"));
        // steady growth: every tick is the maximum.
        assert_eq!(spark(json!([0, 1, 2, 3])).as_deref(), Some("███"));
        // accelerating: bars rise with the per-tick delta.
        assert_eq!(spark(json!([0, 1, 3, 6, 10])).as_deref(), Some("▄▆▇█"));
        // the initial prompt load no longer flattens the later ticks: on the
        // log scale a 300-token tick after an 18k one still shows mid-height.
        assert_eq!(
            spark(json!([0, 18000, 18300, 18600])).as_deref(),
            Some("█▅▅")
        );
        // scale is relative to the largest delta, not the magnitude.
        assert_eq!(
            spark(json!([1000, 8000])).as_deref(),
            spark(json!([0, 1])).as_deref()
        );
        // a drop (compaction) counts as no growth.
        assert_eq!(spark(json!([9, 0, 9])).as_deref(), Some("▁█"));
        // a single sample, an empty list, a missing key, or non-numbers: none.
        assert_eq!(spark(json!([120_000])), None);
        assert_eq!(spark(json!([])), None);
        assert_eq!(spark(json!(["a", null, 3])), None);
        assert_eq!(sparkline(&json!({})), None);
    }

    #[test]
    fn running_row_omits_a_one_sample_sparkline() {
        let mut t = running_task();
        t["tokenSamples"] = json!([48_000]);
        assert!(!strip_sgr(&row(&t)).contains('▁'));
        t.as_object_mut().unwrap().remove("tokenSamples");
        assert!(!strip_sgr(&row(&t)).contains('▁'));
    }

    #[test]
    fn elapsed_formats_and_boundaries() {
        let at = |ago_ms: i64| elapsed(&json!({ "startTime": NOW - ago_ms }), NOW);
        assert_eq!(at(0), "0s");
        assert_eq!(at(999), "0s");
        assert_eq!(at(5_000), "5s");
        assert_eq!(at(59_999), "59s");
        assert_eq!(at(60_000), "1m00s");
        assert_eq!(at(72_000), "1m12s");
        assert_eq!(at(3_599_999), "59m59s");
        assert_eq!(at(3_600_000), "1h00m");
        assert_eq!(at(3_600_000 + 5 * 60_000 + 59_000), "1h05m");
        assert_eq!(at(26 * 3_600_000), "26h00m");
        // missing or in the future.
        assert_eq!(elapsed(&json!({}), NOW), "0s");
        assert_eq!(elapsed(&json!({ "startTime": NOW + 1 }), NOW), "0s");
        assert_eq!(elapsed(&json!({ "startTime": "soon" }), NOW), "0s");
    }

    #[test]
    fn tasks_without_a_string_id_are_skipped() {
        let mut good = running_task();
        good["id"] = json!("keep");
        let mut numeric = running_task();
        numeric["id"] = json!(7);
        let mut missing = running_task();
        missing.as_object_mut().unwrap().remove("id");
        let input = json!({ "columns": 100, "tasks": [numeric, missing, good] });
        let rows = render_rows(&input, NOW);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "keep");
    }

    #[test]
    fn control_characters_in_payload_strings_become_spaces() {
        let mut t = running_task();
        t["name"] = json!("sur\x1b[31mveyor\n");
        t["label"] = json!("line one\nline\ttwo\u{7}");
        let r = row(&t);
        assert!(strip_sgr(&r).starts_with("sur [31mveyor"));
        assert!(!r.contains('\n') && !r.contains('\t') && !r.contains('\u{7}'));
        // every ESC left is one this module wrote, so stripping its colour
        // codes leaves none behind.
        assert!(!strip_sgr(&r).contains('\x1b'));
        assert!(strip_sgr(&r).contains("line one line two"));
    }

    #[test]
    fn empty_and_malformed_input_yields_no_rows() {
        assert!(render_rows(&json!({}), NOW).is_empty());
        assert!(render_rows(&json!({ "columns": 80 }), NOW).is_empty());
        assert!(render_rows(&json!({ "columns": 80, "tasks": [] }), NOW).is_empty());
        assert!(render_rows(&json!({ "columns": 80, "tasks": "x" }), NOW).is_empty());
        // no usable width: nothing can be fitted, so keep Claude's rows.
        assert!(render_rows(&json!({ "tasks": [running_task()] }), NOW).is_empty());
        assert!(render_rows(&json!([]), NOW).is_empty());
    }

    #[test]
    fn a_task_with_no_name_or_tokens_keeps_the_default_row() {
        // An empty `content` would hide the row, so a task too bare to render
        // is left out of the output instead.
        let input = json!({ "columns": 80, "tasks": [{ "id": "bare", "label": "x" }] });
        assert!(render_rows(&input, NOW).is_empty());
    }
}
