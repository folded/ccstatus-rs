//! Shared routing configuration: where each rendered status line is sent.
//!
//! Read by both the registrar (which decides what to print to Claude's own
//! statusline) and the daemon (which decides which tmux rows to drive), so
//! the two processes must agree. A single JSON file keeps them in sync:
//!
//! ```json
//! { "route": { "rich": "tmux", "heatmap_main": "tmux", "heatmap_sub": "off" } }
//! ```
//!
//! Destinations:
//!
//! - `"tmux"` — a dedicated tmux status row (driven by the daemon; the only
//!   surface where the warmth/cache-expiry indicator ticks live);
//! - `"claude"` — Claude's own statusline via stdout (updates only when
//!   Claude re-renders);
//! - `"off"` — not shown.
//!
//! Missing keys default to `"tmux"`. Outside tmux the caller forces every
//! line to Claude's statusline regardless of this file (there is no tmux
//! surface to route to).
//!
//! The daemon reads this once at startup (Phase 1); live hot-reload is
//! Phase 2.

use std::path::PathBuf;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Tmux,
    Claude,
    Off,
}

impl Dest {
    fn parse(s: &str) -> Option<Dest> {
        match s {
            "tmux" => Some(Dest::Tmux),
            "claude" => Some(Dest::Claude),
            "off" => Some(Dest::Off),
            _ => None,
        }
    }
}

/// The routable lines (Phase 1 granularity). The discriminant is the line's
/// index in the registrar's rendered output and in pane-state `lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Rich = 0,
    HeatmapMain = 1,
    HeatmapSub = 2,
}

impl Line {
    /// All lines in their natural top-to-bottom rendered order.
    pub const ALL: [Line; 3] = [Line::Rich, Line::HeatmapMain, Line::HeatmapSub];

    /// Lines in the top-to-bottom order they occupy the tmux status area,
    /// above the powerline row (sub-heatmap closest to the panes).
    pub const TMUX_ORDER: [Line; 3] = [Line::HeatmapSub, Line::HeatmapMain, Line::Rich];

    pub fn index(self) -> usize {
        self as usize
    }

    fn key(self) -> &'static str {
        match self {
            Line::Rich => "rich",
            Line::HeatmapMain => "heatmap_main",
            Line::HeatmapSub => "heatmap_sub",
        }
    }
}

pub struct Routing {
    rich: Dest,
    main: Dest,
    sub: Dest,
}

impl Default for Routing {
    /// All lines to tmux rows — the original in-tmux behaviour.
    fn default() -> Self {
        Self {
            rich: Dest::Tmux,
            main: Dest::Tmux,
            sub: Dest::Tmux,
        }
    }
}

impl Routing {
    /// Every line to Claude's statusline. Used outside tmux, where there is
    /// no tmux surface to route to.
    pub fn all_claude() -> Self {
        Self {
            rich: Dest::Claude,
            main: Dest::Claude,
            sub: Dest::Claude,
        }
    }

    pub fn dest(&self, line: Line) -> Dest {
        match line {
            Line::Rich => self.rich,
            Line::HeatmapMain => self.main,
            Line::HeatmapSub => self.sub,
        }
    }

    pub fn any_tmux(&self) -> bool {
        Line::ALL.iter().any(|&l| self.dest(l) == Dest::Tmux)
    }

    /// Load from the config file, falling back to per-key defaults for
    /// anything missing or unparseable. A missing/corrupt file yields the
    /// default routing (all tmux).
    pub fn load() -> Self {
        Self::from_value(read_config())
    }

    fn from_value(v: Option<Value>) -> Self {
        let mut r = Routing::default();
        let Some(route) = v.as_ref().and_then(|v| v.get("route")) else {
            return r;
        };
        let apply = |line: Line, slot: &mut Dest| {
            if let Some(d) = route.get(line.key()).and_then(|x| x.as_str()).and_then(Dest::parse) {
                *slot = d;
            }
        };
        apply(Line::Rich, &mut r.rich);
        apply(Line::HeatmapMain, &mut r.main);
        apply(Line::HeatmapSub, &mut r.sub);
        r
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("ccstatus").join("config.json")
}

fn read_config() -> Option<Value> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_tmux() {
        let r = Routing::default();
        assert_eq!(r.dest(Line::Rich), Dest::Tmux);
        assert_eq!(r.dest(Line::HeatmapSub), Dest::Tmux);
        assert!(r.any_tmux());
    }

    #[test]
    fn parses_route_table_with_defaults_for_missing() {
        let v: Value = serde_json::from_str(
            r#"{ "route": { "rich": "claude", "heatmap_sub": "off" } }"#,
        )
        .unwrap();
        let r = Routing::from_value(Some(v));
        assert_eq!(r.dest(Line::Rich), Dest::Claude);
        assert_eq!(r.dest(Line::HeatmapMain), Dest::Tmux); // missing -> default
        assert_eq!(r.dest(Line::HeatmapSub), Dest::Off);
    }

    #[test]
    fn line_index_matches_rendered_order() {
        assert_eq!(Line::Rich.index(), 0);
        assert_eq!(Line::HeatmapMain.index(), 1);
        assert_eq!(Line::HeatmapSub.index(), 2);
    }

    #[test]
    fn any_tmux_false_when_all_claude() {
        assert!(!Routing::all_claude().any_tmux());
    }
}
