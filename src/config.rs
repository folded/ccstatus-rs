//! Shared routing configuration: where each status **element** is sent.
//!
//! Read by both the registrar (which composes Claude's own statusline and
//! stores element content in pane state) and the daemon (which composes the
//! tmux surfaces), so the two must agree. A single JSON file keeps them in
//! sync:
//!
//! ```json
//! {
//!   "route": {
//!     "model": "row2", "cwd": "row2", "tokens": "row2",
//!     "warmth": "row2", "heatmap_main": "row1", "heatmap_sub": "row0"
//!   }
//! }
//! ```
//!
//! Destinations:
//!
//! - `"off"` — not shown;
//! - `"claude"` — Claude's own statusline via stdout (updates only when
//!   Claude re-renders — unsuitable for the live `warmth` element). A line and
//!   alignment may be appended: `"claude.right"` right-aligns on the first
//!   line; `"claude.1"` is the second printed line; `"claude.1.right"` is the
//!   second line, right-aligned. Bare `"claude"` means line 0, left.
//! - `"row0"`, `"row1"`, … — a dedicated tmux status row (`row0` nearest the
//!   panes), driven live by the daemon. Multiple elements on one row join
//!   inline.
//! - `"left"` / `"right"` — the user's base status row's `status-left` /
//!   `status-right` edge (zero added height).
//!
//! Missing keys fall back to per-element defaults that reproduce the Phase 1
//! look. Outside tmux the caller forces every element to `"claude"`.
//!
//! The daemon reads this once at startup (Phase 2a); live hot-reload is 2c.

use std::path::PathBuf;

use serde_json::Value;

/// A routable status element. The discriminant order is the canonical order
/// elements are composed in (left to right within a surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    Model,
    Cwd,
    Tokens,
    Effort,
    Limits,
    Version,
    Updates,
    /// Cache-warmth / expiry indicator. Computed live by the daemon, not the
    /// registrar — it only ticks on a daemon-driven surface.
    Warmth,
    HeatmapMain,
    HeatmapSub,
}

/// How an element occupies a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Inline; joins with ` | ` alongside other segments on the surface.
    Segment,
    /// A standalone full-width row.
    Row,
}

impl Element {
    pub const ALL: [Element; 10] = [
        Element::Model,
        Element::Cwd,
        Element::Tokens,
        Element::Effort,
        Element::Limits,
        Element::Version,
        Element::Updates,
        Element::Warmth,
        Element::HeatmapMain,
        Element::HeatmapSub,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Element::Model => "model",
            Element::Cwd => "cwd",
            Element::Tokens => "tokens",
            Element::Effort => "effort",
            Element::Limits => "limits",
            Element::Version => "version",
            Element::Updates => "updates",
            Element::Warmth => "warmth",
            Element::HeatmapMain => "heatmap_main",
            Element::HeatmapSub => "heatmap_sub",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Element::HeatmapMain | Element::HeatmapSub => Kind::Row,
            _ => Kind::Segment,
        }
    }

    /// Computed live by the daemon rather than rendered by the registrar.
    pub fn is_live(self) -> bool {
        matches!(self, Element::Warmth)
    }

    fn default_dest(self) -> Dest {
        // Reproduce the Phase 1 layout: rich segments + warmth on row2,
        // heatmaps on rows 1 and 0, the user's base row below.
        match self {
            Element::HeatmapMain => Dest::Row(1),
            Element::HeatmapSub => Dest::Row(0),
            Element::Updates => Dest::Off,
            _ => Dest::Row(2),
        }
    }
}

/// Horizontal alignment of a segment within a Claude statusline line. (Named
/// `Align` to avoid colliding with the daemon's edge-revert `Side`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Off,
    /// Claude's own statusline, on the given printed line (0 = first) with the
    /// given horizontal alignment.
    Claude {
        line: u8,
        align: Align,
    },
    Row(u8),
    /// The base status row's `status-left` edge (zero added height).
    Left,
    /// The base status row's `status-right` edge (zero added height).
    Right,
}

impl Dest {
    /// Bare `claude`: first line, left-aligned.
    pub const CLAUDE: Dest = Dest::Claude {
        line: 0,
        align: Align::Left,
    };

    fn parse(s: &str) -> Option<Dest> {
        match s {
            "off" => Some(Dest::Off),
            "left" => Some(Dest::Left),
            "right" => Some(Dest::Right),
            _ if s == "claude" || s.starts_with("claude.") => {
                Dest::parse_claude(s.strip_prefix("claude.").unwrap_or(""))
            }
            _ => s
                .strip_prefix("row")
                .and_then(|n| n.parse::<u8>().ok())
                .map(Dest::Row),
        }
    }

    /// Parse the part after `claude.` — dot-separated tokens, each either a
    /// line number or an alignment keyword, in any order. Empty means defaults.
    fn parse_claude(rest: &str) -> Option<Dest> {
        let mut line = 0u8;
        let mut align = Align::Left;
        if !rest.is_empty() {
            for tok in rest.split('.') {
                match tok {
                    "left" => align = Align::Left,
                    "right" => align = Align::Right,
                    n => line = n.parse().ok()?,
                }
            }
        }
        Some(Dest::Claude { line, align })
    }

    /// A daemon-driven tmux surface (a dedicated row or a base-row edge).
    pub fn is_tmux(self) -> bool {
        matches!(self, Dest::Row(_) | Dest::Left | Dest::Right)
    }
}

pub struct Routing {
    dests: [Dest; 10],
}

impl Default for Routing {
    fn default() -> Self {
        let mut dests = [Dest::Off; 10];
        for e in Element::ALL {
            dests[e as usize] = e.default_dest();
        }
        Self { dests }
    }
}

impl Routing {
    /// Every element to Claude's statusline. Used outside tmux, where there
    /// is no tmux surface to route to. Segments land on the first line; the
    /// two heatmap rows take the lines below it (reproducing the stdout
    /// layout). The live `warmth` element is dropped (it can't tick on
    /// Claude's statusline).
    pub fn all_claude() -> Self {
        let mut dests = [Dest::CLAUDE; 10];
        dests[Element::Warmth as usize] = Dest::Off;
        dests[Element::HeatmapMain as usize] = Dest::Claude {
            line: 1,
            align: Align::Left,
        };
        dests[Element::HeatmapSub as usize] = Dest::Claude {
            line: 2,
            align: Align::Left,
        };
        Self { dests }
    }

    pub fn dest(&self, e: Element) -> Dest {
        self.dests[e as usize]
    }

    /// Any element routed to a daemon-driven tmux surface (a row or a
    /// base-row edge). Determines whether the registrar registers/spawns
    /// the daemon at all.
    pub fn any_tmux(&self) -> bool {
        Element::ALL.iter().any(|&e| self.dest(e).is_tmux())
    }

    /// Distinct row numbers used, ascending (row0 nearest the panes).
    pub fn rows_used(&self) -> Vec<u8> {
        let mut rows: Vec<u8> = Element::ALL
            .iter()
            .filter_map(|&e| match self.dest(e) {
                Dest::Row(n) => Some(n),
                _ => None,
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Elements routed to a given destination, in canonical order.
    pub fn elements_for(&self, dest: Dest) -> Vec<Element> {
        Element::ALL
            .iter()
            .copied()
            .filter(|&e| self.dest(e) == dest)
            .collect()
    }

    /// Claude-routed elements with their line and alignment, in canonical
    /// order. Used to compose Claude's own (multi-line) statusline.
    pub fn claude_elements(&self) -> Vec<(Element, u8, Align)> {
        Element::ALL
            .iter()
            .copied()
            .filter_map(|e| match self.dest(e) {
                Dest::Claude { line, align } => Some((e, line, align)),
                _ => None,
            })
            .collect()
    }

    pub fn load() -> Self {
        Self::from_value(read_config())
    }

    /// Test-only: a routing starting from the defaults with the given
    /// element→dest overrides applied.
    #[cfg(test)]
    pub fn from_pairs(pairs: &[(Element, Dest)]) -> Self {
        let mut r = Routing::default();
        for &(e, d) in pairs {
            r.dests[e as usize] = d;
        }
        r
    }

    fn from_value(v: Option<Value>) -> Self {
        let mut r = Routing::default();
        let Some(route) = v.as_ref().and_then(|v| v.get("route")) else {
            return r;
        };
        for e in Element::ALL {
            if let Some(d) = route
                .get(e.key())
                .and_then(|x| x.as_str())
                .and_then(Dest::parse)
            {
                r.dests[e as usize] = d;
            }
        }
        r
    }
}

/// The user-configured Linux window-jump command (`jump.linux` in the config
/// file), run by [`crate::window::focus`] to raise a non-tmux Claude's
/// terminal window. It receives the Claude pid as `$CCSTATUS_CLAUDE_PID` and
/// as its first argument. `None` falls back to ccstatus's bundled best-effort
/// X11 default — set this to support a Wayland compositor or another emulator.
/// Only consumed by the Linux jump path (`window::focus`); dead elsewhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn jump_command() -> Option<String> {
    jump_command_from(read_config().as_ref())
}

/// Pure core of [`jump_command`]: pull `jump.linux` from a parsed config.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn jump_command_from(v: Option<&Value>) -> Option<String> {
    v?.get("jump")?
        .get("linux")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An explicit status background colour (`"background": "#rrggbb"` in the
/// config file), applied to the surfaces this tool owns — the Claude
/// statusline lines outside tmux, and the daemon-driven rows inside it — so
/// the bar reads consistently regardless of the terminal or tmux theme.
/// `None` leaves the inherited background untouched.
pub fn background() -> Option<(u8, u8, u8)> {
    background_from(read_config().as_ref())
}

fn background_from(v: Option<&Value>) -> Option<(u8, u8, u8)> {
    parse_hex_rgb(v?.get("background")?.as_str()?)
}

/// Parse `#rrggbb` (case-insensitive) into an RGB triple.
fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("ccstatus").join("config.json")
}

fn read_config() -> Option<Value> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Last-modified time of the config file, for change detection (hot
/// reload). `None` if the file is absent.
pub fn mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_reproduces_phase1_layout() {
        let r = Routing::default();
        assert_eq!(r.dest(Element::Model), Dest::Row(2));
        assert_eq!(r.dest(Element::Warmth), Dest::Row(2));
        assert_eq!(r.dest(Element::HeatmapMain), Dest::Row(1));
        assert_eq!(r.dest(Element::HeatmapSub), Dest::Row(0));
        assert_eq!(r.dest(Element::Updates), Dest::Off);
        assert_eq!(r.rows_used(), vec![0, 1, 2]);
        assert!(r.any_tmux());
    }

    #[test]
    fn parses_dests_including_rows() {
        assert_eq!(Dest::parse("off"), Some(Dest::Off));
        assert_eq!(Dest::parse("claude"), Some(Dest::CLAUDE));
        assert_eq!(Dest::parse("row0"), Some(Dest::Row(0)));
        assert_eq!(Dest::parse("row12"), Some(Dest::Row(12)));
        assert_eq!(Dest::parse("nonsense"), None);
    }

    #[test]
    fn parses_claude_line_and_alignment() {
        assert_eq!(
            Dest::parse("claude.right"),
            Some(Dest::Claude {
                line: 0,
                align: Align::Right
            })
        );
        assert_eq!(
            Dest::parse("claude.1"),
            Some(Dest::Claude {
                line: 1,
                align: Align::Left
            })
        );
        assert_eq!(
            Dest::parse("claude.1.right"),
            Some(Dest::Claude {
                line: 1,
                align: Align::Right
            })
        );
        // tokens may appear in either order.
        assert_eq!(Dest::parse("claude.right.2"), Dest::parse("claude.2.right"));
        // a non-numeric, non-alignment token is rejected.
        assert_eq!(Dest::parse("claude.bogus"), None);
    }

    #[test]
    fn from_value_overrides_only_named_keys() {
        let v: Value =
            serde_json::from_str(r#"{ "route": { "tokens": "claude", "heatmap_sub": "off" } }"#)
                .unwrap();
        let r = Routing::from_value(Some(v));
        assert_eq!(r.dest(Element::Tokens), Dest::CLAUDE);
        assert_eq!(r.dest(Element::HeatmapSub), Dest::Off);
        assert_eq!(r.dest(Element::Model), Dest::Row(2)); // untouched default
    }

    #[test]
    fn all_claude_drops_warmth_and_uses_no_rows() {
        let r = Routing::all_claude();
        assert_eq!(r.dest(Element::Model), Dest::CLAUDE);
        assert_eq!(r.dest(Element::Warmth), Dest::Off);
        assert_eq!(
            r.dest(Element::HeatmapMain),
            Dest::Claude {
                line: 1,
                align: Align::Left
            }
        );
        assert!(!r.any_tmux());
    }

    #[test]
    fn jump_command_reads_linux_key() {
        let v: Value = serde_json::from_str(r#"{ "jump": { "linux": "myjump $1" } }"#).unwrap();
        assert_eq!(jump_command_from(Some(&v)).as_deref(), Some("myjump $1"));
        // Absent section, empty string, and no config all fall back.
        let empty: Value = serde_json::from_str(r#"{ "route": {} }"#).unwrap();
        assert_eq!(jump_command_from(Some(&empty)), None);
        let blank: Value = serde_json::from_str(r#"{ "jump": { "linux": "" } }"#).unwrap();
        assert_eq!(jump_command_from(Some(&blank)), None);
        assert_eq!(jump_command_from(None), None);
    }

    #[test]
    fn background_parses_hex_and_rejects_junk() {
        let v: Value = serde_json::from_str(r##"{ "background": "#1a1b26" }"##).unwrap();
        assert_eq!(background_from(Some(&v)), Some((0x1a, 0x1b, 0x26)));
        // missing key, bad length, non-hex, and no config all yield None.
        let none: Value = serde_json::from_str(r#"{ "route": {} }"#).unwrap();
        assert_eq!(background_from(Some(&none)), None);
        assert_eq!(parse_hex_rgb("#1a1b2"), None);
        assert_eq!(parse_hex_rgb("1a1b26"), None);
        assert_eq!(parse_hex_rgb("#xxyyzz"), None);
        assert_eq!(background_from(None), None);
    }

    #[test]
    fn elements_for_row_is_ordered() {
        let r = Routing::default();
        assert_eq!(
            r.elements_for(Dest::Row(2)),
            vec![
                Element::Model,
                Element::Cwd,
                Element::Tokens,
                Element::Effort,
                Element::Limits,
                Element::Version,
                Element::Warmth,
            ]
        );
    }
}
