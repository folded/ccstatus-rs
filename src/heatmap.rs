use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, Timelike};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cache;
use crate::color::{DIM, RESET};
use crate::format::format_tokens;

const SLOTS: usize = 96;
/// Schema version for the heatmap cache. Bump to invalidate caches when the on-disk shape changes.
const SCHEMA: u32 = 2;

pub struct HeatmapResult {
    pub main_row: String,
    pub sub_row: String,
    pub today_main_raw: u64,
    pub today_sub_raw: u64,
}

struct Aggregates {
    today_main_weighted: [u64; SLOTS],
    today_sub_weighted: [u64; SLOTS],
    today_main_raw: [u64; SLOTS],
    today_sub_raw: [u64; SLOTS],
    /// Weighted (date, slot) -> tokens, partitioned by side.
    history_main: HashMap<(NaiveDate, u32), u64>,
    history_sub: HashMap<(NaiveDate, u32), u64>,
}

impl Default for Aggregates {
    fn default() -> Self {
        Aggregates {
            today_main_weighted: [0; SLOTS],
            today_sub_weighted: [0; SLOTS],
            today_main_raw: [0; SLOTS],
            today_sub_raw: [0; SLOTS],
            history_main: HashMap::new(),
            history_sub: HashMap::new(),
        }
    }
}

pub fn render(cwd: &str, transcript_path: Option<&str>, term_cols: u16) -> Option<HeatmapResult> {
    if cwd.is_empty() {
        return None;
    }

    let cache_path = cache_path(cwd);
    let now = Local::now();
    let today = now.date_naive();
    let now_slot = now.hour() as usize * 4 + now.minute() as usize / 15;

    let cached = cache::read_if_fresh(&cache_path, Duration::from_secs(60))
        .and_then(|s| CachedHeatmap::parse(&s, today));

    let data = match cached {
        Some(c) => c,
        None => {
            let dir = resolve_project_dir(cwd, transcript_path)?;
            let aggs = scan_project(&dir, today)?;
            let computed = CachedHeatmap::from_aggregates(aggs);
            let _ = cache::write_atomic(&cache_path, &computed.serialize());
            computed
        }
    };

    let n_cells = pick_cells(term_cols);

    Some(HeatmapResult {
        main_row: render_row(
            "main",
            &data.today_main_weighted,
            &data.today_main_raw,
            &data.main_samples,
            now_slot,
            n_cells,
        ),
        sub_row: render_row(
            "sub ",
            &data.today_sub_weighted,
            &data.today_sub_raw,
            &data.sub_samples,
            now_slot,
            n_cells,
        ),
        today_main_raw: data.today_main_raw.iter().sum(),
        today_sub_raw: data.today_sub_raw.iter().sum(),
    })
}

/// Visible chars per row outside the cells: label (4) + space (1) + 2 spaces + total (5) + 1 slack.
const ROW_CHROME: u16 = 13;

fn pick_cells(term_cols: u16) -> usize {
    let budget = term_cols.saturating_sub(ROW_CHROME) as usize;
    for &n in &[96, 48, 32, 24, 16, 12, 8, 6, 4, 3, 2, 1] {
        if n <= budget {
            return n;
        }
    }
    1
}

fn cache_path(cwd: &str) -> PathBuf {
    let mut h = Sha256::new();
    h.update(cwd.as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(4) {
        hex.push_str(&format!("{b:02x}"));
    }
    cache::cache_dir().join(format!("statusline-heatmap-{hex}.json"))
}

fn project_subdir(cwd: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_default();
    let encoded = cwd.replace('/', "-");
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encoded)
}

/// Pick the project directory whose transcripts we'll scan. `transcript_path`
/// from the per-render payload is authoritative — it's exactly where Claude
/// Code is writing this session's records, regardless of `/add-dir` or any
/// later cwd changes. If that's missing or malformed, fall back to the
/// cwd-encoded path (Claude's own naming convention) so isolated invocations
/// still work.
fn resolve_project_dir(cwd: &str, transcript_path: Option<&str>) -> Option<PathBuf> {
    if let Some(t) = transcript_path
        && let Some(parent) = PathBuf::from(t).parent()
        && parent.exists()
    {
        return Some(parent.to_path_buf());
    }
    let sub = project_subdir(cwd);
    if sub.exists() { Some(sub) } else { None }
}

fn scan_project(dir: &Path, today: NaiveDate) -> Option<Aggregates> {
    let mut aggs = Aggregates::default();
    walk(dir, today, &mut aggs);
    Some(aggs)
}

fn walk(dir: &Path, today: NaiveDate, aggs: &mut Aggregates) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(&path, today, aggs);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let file = match fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                ingest_line(&line, today, aggs);
            }
        }
    }
}

fn ingest_line(line: &str, today: NaiveDate, aggs: &mut Aggregates) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ts_str = match v.get("timestamp").and_then(|x| x.as_str()) {
        Some(s) => s,
        None => return,
    };
    let dt = match DateTime::parse_from_rfc3339(ts_str) {
        Ok(d) => d,
        Err(_) => return,
    };
    let local = dt.with_timezone(&Local);
    let date = local.date_naive();
    let slot = local.hour() * 4 + local.minute() / 15;

    let usage = match v.pointer("/message/usage") {
        Some(u) if u.is_object() => u,
        _ => return,
    };
    let weighted = weighted_tokens(usage);
    let raw = raw_tokens(usage);
    if weighted == 0 && raw == 0 {
        return;
    }
    let is_sub = v
        .get("isSidechain")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    if date == today {
        let s = slot as usize;
        if is_sub {
            aggs.today_sub_weighted[s] += weighted;
            aggs.today_sub_raw[s] += raw;
        } else {
            aggs.today_main_weighted[s] += weighted;
            aggs.today_main_raw[s] += raw;
        }
    }
    let history = if is_sub {
        &mut aggs.history_sub
    } else {
        &mut aggs.history_main
    };
    *history.entry((date, slot)).or_insert(0) += weighted;
}

/// Cost weights × 100 to stay in integer math:
/// input 100, cache_5m 125, cache_1h 200, cache_read 10, output 500.
fn weighted_tokens(usage: &Value) -> u64 {
    let input = u64_field(usage, "input_tokens");
    let cache_read = u64_field(usage, "cache_read_input_tokens");
    let output = u64_field(usage, "output_tokens");

    let (e5m, e1h) = match usage.get("cache_creation") {
        Some(cc) if cc.is_object() => (
            u64_field(cc, "ephemeral_5m_input_tokens"),
            u64_field(cc, "ephemeral_1h_input_tokens"),
        ),
        _ => (u64_field(usage, "cache_creation_input_tokens"), 0),
    };

    (input * 100 + e5m * 125 + e1h * 200 + cache_read * 10 + output * 500) / 100
}

fn raw_tokens(usage: &Value) -> u64 {
    u64_field(usage, "input_tokens")
        + u64_field(usage, "cache_creation_input_tokens")
        + u64_field(usage, "cache_read_input_tokens")
        + u64_field(usage, "output_tokens")
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

struct CachedHeatmap {
    today_main_weighted: [u64; SLOTS],
    today_sub_weighted: [u64; SLOTS],
    today_main_raw: [u64; SLOTS],
    today_sub_raw: [u64; SLOTS],
    main_samples: Vec<u64>,
    sub_samples: Vec<u64>,
}

impl CachedHeatmap {
    fn from_aggregates(aggs: Aggregates) -> Self {
        let mut main_samples: Vec<u64> = aggs
            .history_main
            .values()
            .copied()
            .filter(|n| *n > 0)
            .collect();
        let mut sub_samples: Vec<u64> = aggs
            .history_sub
            .values()
            .copied()
            .filter(|n| *n > 0)
            .collect();
        main_samples.sort_unstable();
        sub_samples.sort_unstable();
        CachedHeatmap {
            today_main_weighted: aggs.today_main_weighted,
            today_sub_weighted: aggs.today_sub_weighted,
            today_main_raw: aggs.today_main_raw,
            today_sub_raw: aggs.today_sub_raw,
            main_samples,
            sub_samples,
        }
    }

    fn serialize(&self) -> String {
        let v = serde_json::json!({
            "schema": SCHEMA,
            "today": Local::now().date_naive().to_string(),
            "today_main_weighted": self.today_main_weighted.to_vec(),
            "today_sub_weighted": self.today_sub_weighted.to_vec(),
            "today_main_raw": self.today_main_raw.to_vec(),
            "today_sub_raw": self.today_sub_raw.to_vec(),
            "main_samples": self.main_samples,
            "sub_samples": self.sub_samples,
        });
        v.to_string()
    }

    fn parse(s: &str, today: NaiveDate) -> Option<Self> {
        let v: Value = serde_json::from_str(s).ok()?;
        if v.get("schema").and_then(|x| x.as_u64()) != Some(SCHEMA as u64) {
            return None;
        }
        if v.get("today").and_then(|x| x.as_str()).unwrap_or("") != today.to_string() {
            return None;
        }
        let mw = parse_slot_array(v.get("today_main_weighted"))?;
        let sw = parse_slot_array(v.get("today_sub_weighted"))?;
        let mr = parse_slot_array(v.get("today_main_raw"))?;
        let sr = parse_slot_array(v.get("today_sub_raw"))?;
        let main_samples = parse_samples(v.get("main_samples"));
        let sub_samples = parse_samples(v.get("sub_samples"));
        Some(CachedHeatmap {
            today_main_weighted: mw,
            today_sub_weighted: sw,
            today_main_raw: mr,
            today_sub_raw: sr,
            main_samples,
            sub_samples,
        })
    }
}

fn parse_slot_array(v: Option<&Value>) -> Option<[u64; SLOTS]> {
    let arr = v?.as_array()?;
    if arr.len() != SLOTS {
        return None;
    }
    let mut out = [0u64; SLOTS];
    for i in 0..SLOTS {
        out[i] = arr[i].as_u64().unwrap_or(0);
    }
    Some(out)
}

fn parse_samples(v: Option<&Value>) -> Vec<u64> {
    v.and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_u64()).collect())
        .unwrap_or_default()
}

fn render_row(
    label: &str,
    weighted: &[u64; SLOTS],
    raw: &[u64; SLOTS],
    samples: &[u64],
    now_slot: usize,
    n_cells: usize,
) -> String {
    let mut out = String::with_capacity(384);
    out.push_str(DIM);
    out.push_str(label);
    out.push_str(RESET);
    out.push(' ');

    let slots_per_cell = SLOTS / n_cells;

    for cell in 0..n_cells {
        let start = cell * slots_per_cell;
        let end = start + slots_per_cell;
        let cell_weighted: u64 = weighted[start..end].iter().sum();
        let cell_passed = start <= now_slot;
        if cell_weighted == 0 {
            out.push_str(DIM);
            if cell_passed {
                out.push('·');
            } else {
                out.push(' ');
            }
            out.push_str(RESET);
        } else {
            let (r, g, b) = jet(percentile(cell_weighted, samples));
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m█{RESET}"));
        }
    }

    let raw_total: u64 = raw.iter().sum();
    out.push_str(&format!("  {DIM}{:>5}{RESET}", format_tokens(raw_total)));
    out
}

fn percentile(val: u64, sorted_samples: &[u64]) -> f64 {
    if sorted_samples.is_empty() {
        return 0.5;
    }
    let idx = sorted_samples.partition_point(|&x| x < val);
    idx as f64 / sorted_samples.len() as f64
}

fn jet(t: f64) -> (u8, u8, u8) {
    let clamp = |x: f64| x.clamp(0.0, 1.0);
    let r = clamp((4.0 * t - 1.5).min(-4.0 * t + 4.5));
    let g = clamp((4.0 * t - 0.5).min(-4.0 * t + 3.5));
    let b = clamp((4.0 * t + 0.5).min(-4.0 * t + 2.5));
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percentile_edges() {
        let s = vec![1u64, 2, 3, 4, 5];
        assert_eq!(percentile(0, &s), 0.0);
        assert_eq!(percentile(6, &s), 1.0);
        assert_eq!(percentile(3, &s), 0.4);
    }

    #[test]
    fn jet_endpoints() {
        let (r, _, b) = jet(0.0);
        assert!(r < b);
        let (r, _, b) = jet(1.0);
        assert!(r > b);
    }

    #[test]
    fn weighted_uses_ephemeral_breakdown() {
        let u = json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 1000,
            "output_tokens": 10,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 200,
                "ephemeral_1h_input_tokens": 50,
            }
        });
        // 100*1 + 200*1.25 + 50*2 + 1000*0.1 + 10*5 = 100 + 250 + 100 + 100 + 50 = 600
        assert_eq!(weighted_tokens(&u), 600);
    }

    #[test]
    fn weighted_falls_back_to_flat_cache_creation() {
        let u = json!({
            "input_tokens": 0,
            "cache_creation_input_tokens": 400,
            "cache_read_input_tokens": 0,
            "output_tokens": 0,
        });
        // 400 * 1.25 = 500
        assert_eq!(weighted_tokens(&u), 500);
    }
}
