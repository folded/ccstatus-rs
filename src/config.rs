//! Routing configuration: which **surface** and position each status
//! **element** occupies, per **layout**.
//!
//! The layout is chosen at runtime: inside tmux the `"tmux"` layout is used,
//! otherwise `"default"`. Each layout holds one or more surfaces; each surface
//! maps a region (`<side>[.<line>]`) to an ordered, comma-separated element
//! list, plus an optional `background`.
//!
//! ```json
//! {
//!   "tmux": {
//!     "claude": { "left": "cwd, effort", "right": "version" },
//!     "tmux":   { "left": "model", "right": "warmth",
//!                 "left.1": "heatmap_main", "left.2": "tokens, limits" }
//!   },
//!   "default": {
//!     "claude": { "left": "model, cwd, effort", "right": "version",
//!                 "left.1": "tokens, limits", "left.2": "heatmap_main" }
//!   }
//! }
//! ```
//!
//! Surfaces:
//! - `claude` — Claude's own statusline (stdout), available in every layout. A
//!   region places its elements on printed line `<line>` (0 = first), aligned
//!   left or right.
//! - `tmux` — the daemon-driven tmux status bar (only in the `tmux` layout).
//!   Line 0 is the base status row, where `left`/`right` are its
//!   `status-left`/`status-right` edges; lines >= 1 are dedicated rows, with
//!   `left`/`right` aligned via tmux `#[align]`.
//!
//! Both surfaces share the `<side>[.<line>]` grammar (`left`, `right`,
//! `left.1`, `right.2`, …; bare side = line 0). A region's value is a
//! comma-separated element list, and list order is render order. An element
//! listed on no surface is hidden. If an element appears on both surfaces of a
//! layout, the `claude` surface wins. `background` is a reserved per-surface
//! key (`#rrggbb`).

use std::path::PathBuf;

use serde_json::Value;

/// A routable status element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    Model,
    Cwd,
    Tokens,
    Effort,
    Limits,
    Version,
    Updates,
    /// Cache-warmth / expiry indicator. Computed live (by the daemon on a tmux
    /// surface, or by the registrar on Claude's statusline) rather than by the
    /// element renderer.
    Warmth,
    HeatmapMain,
    HeatmapSub,
}

/// How an element occupies a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Inline; joins with ` | ` alongside other segments in its region.
    Segment,
    /// A standalone full-width row (alignment ignored; takes the whole line).
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

    pub fn from_key(k: &str) -> Option<Element> {
        Element::ALL.into_iter().find(|e| e.key() == k)
    }

    pub fn kind(self) -> Kind {
        match self {
            Element::HeatmapMain | Element::HeatmapSub => Kind::Row,
            _ => Kind::Segment,
        }
    }

    /// Computed live rather than produced by the element renderer.
    pub fn is_live(self) -> bool {
        matches!(self, Element::Warmth)
    }
}

/// Horizontal alignment of an element within its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Where an element is rendered. Both surface variants carry the same
/// `{ line, align }` shape; the surface (Claude statusline vs tmux bar) decides
/// what "line" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Off,
    /// Claude's statusline: `line` is the printed line (0 = first).
    Claude {
        line: u8,
        align: Align,
    },
    /// The tmux bar: `line` is the status-format line. Line 0 is the base row
    /// (align → `status-left`/`status-right` edge); lines >= 1 are dedicated
    /// rows (align → `#[align]` within the row).
    Tmux {
        line: u8,
        align: Align,
    },
}

impl Dest {
    /// A daemon-driven tmux surface.
    pub fn is_tmux(self) -> bool {
        matches!(self, Dest::Tmux { .. })
    }
}

/// Parse a region key (`left`, `right`, `left.1`, `right.2`, …) into an
/// alignment and a line number (bare side = line 0).
fn parse_region(key: &str) -> Option<(Align, u8)> {
    let mut it = key.split('.');
    let align = match it.next()? {
        "left" => Align::Left,
        "right" => Align::Right,
        _ => return None,
    };
    let line = match it.next() {
        Some(n) => n.parse().ok()?,
        None => 0,
    };
    if it.next().is_some() {
        return None; // more than one dot
    }
    Some((align, line))
}

/// The resolved routing for one runtime context (in tmux, or not). Maps each
/// element to a destination and a render order, and carries the per-surface
/// background colours.
pub struct Routing {
    dests: [Dest; 10],
    /// Render order within a region (lower = earlier), from the config list
    /// order. Elements not placed sort last but are filtered out anyway.
    order: [u32; 10],
    claude_bg: Option<(u8, u8, u8)>,
    tmux_bg: Option<(u8, u8, u8)>,
}

impl Default for Routing {
    fn default() -> Self {
        Routing::builtin(true)
    }
}

impl Routing {
    fn empty() -> Self {
        Routing {
            dests: [Dest::Off; 10],
            order: [u32::MAX; 10],
            claude_bg: None,
            tmux_bg: None,
        }
    }

    /// The built-in layout used when the config file has no entry for the
    /// active layout. In tmux: a dedicated row of segments above two heatmap
    /// rows, the base row below. Outside tmux: segments on Claude's first line,
    /// the heatmaps on the lines beneath, `warmth` off (it has no live surface
    /// unless explicitly placed on Claude with `refreshInterval`).
    fn builtin(in_tmux: bool) -> Self {
        let mut r = Routing::empty();
        let place = |r: &mut Routing, e: Element, d: Dest| {
            r.dests[e as usize] = d;
            r.order[e as usize] = e as u32;
        };
        if in_tmux {
            for e in Element::ALL {
                let d = match e {
                    Element::Updates => Dest::Off,
                    Element::HeatmapSub => Dest::Tmux {
                        line: 1,
                        align: Align::Left,
                    },
                    Element::HeatmapMain => Dest::Tmux {
                        line: 2,
                        align: Align::Left,
                    },
                    _ => Dest::Tmux {
                        line: 3,
                        align: Align::Left,
                    },
                };
                place(&mut r, e, d);
            }
        } else {
            for e in Element::ALL {
                let d = match e {
                    Element::Warmth => Dest::Off,
                    Element::HeatmapMain => Dest::Claude {
                        line: 1,
                        align: Align::Left,
                    },
                    Element::HeatmapSub => Dest::Claude {
                        line: 2,
                        align: Align::Left,
                    },
                    _ => Dest::Claude {
                        line: 0,
                        align: Align::Left,
                    },
                };
                place(&mut r, e, d);
            }
        }
        r
    }

    /// Resolve the routing for the active context from the config file.
    pub fn for_context(in_tmux: bool) -> Self {
        Self::from_value(read_config(), in_tmux)
    }

    fn from_value(v: Option<Value>, in_tmux: bool) -> Self {
        let layout_name = if in_tmux { "tmux" } else { "default" };
        let Some(layout) = v.as_ref().and_then(|v| v.get(layout_name)) else {
            return Routing::builtin(in_tmux);
        };
        Self::build(layout, in_tmux)
    }

    /// Build the routing from a single layout object. The `tmux` surface is
    /// applied first and the `claude` surface second, so on a conflicting
    /// element the Claude placement wins.
    fn build(layout: &Value, in_tmux: bool) -> Self {
        let mut r = Routing::empty();
        let mut order: u32 = 0;

        if in_tmux && let Some(surf) = layout.get("tmux") {
            r.tmux_bg = surface_background(surf);
            apply_surface(surf, &mut r, &mut order, |line, align| Dest::Tmux {
                line,
                align,
            });
        }
        if let Some(surf) = layout.get("claude") {
            r.claude_bg = surface_background(surf);
            apply_surface(surf, &mut r, &mut order, |line, align| Dest::Claude {
                line,
                align,
            });
        }
        r
    }

    pub fn dest(&self, e: Element) -> Dest {
        self.dests[e as usize]
    }

    pub fn claude_background(&self) -> Option<(u8, u8, u8)> {
        self.claude_bg
    }

    pub fn tmux_background(&self) -> Option<(u8, u8, u8)> {
        self.tmux_bg
    }

    /// Any element on a tmux surface — whether the registrar should register
    /// the pane and spawn/ping the daemon.
    pub fn any_tmux(&self) -> bool {
        Element::ALL.iter().any(|&e| self.dest(e).is_tmux())
    }

    /// Distinct Claude lines used, ascending.
    pub fn claude_lines(&self) -> Vec<u8> {
        self.lines(|d| matches!(d, Dest::Claude { .. }).then(|| line_of(d)))
    }

    /// Distinct tmux lines used, ascending (0 = base row).
    pub fn tmux_lines(&self) -> Vec<u8> {
        self.lines(|d| matches!(d, Dest::Tmux { .. }).then(|| line_of(d)))
    }

    fn lines(&self, pick: impl Fn(Dest) -> Option<u8>) -> Vec<u8> {
        let mut v: Vec<u8> = Element::ALL
            .iter()
            .filter_map(|&e| pick(self.dest(e)))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Elements on Claude's statusline at `line`/`align`, in render order.
    pub fn claude_at(&self, line: u8, align: Align) -> Vec<Element> {
        self.at(Dest::Claude { line, align })
    }

    /// Elements on the tmux bar at `line`/`align`, in render order.
    pub fn tmux_at(&self, line: u8, align: Align) -> Vec<Element> {
        self.at(Dest::Tmux { line, align })
    }

    fn at(&self, want: Dest) -> Vec<Element> {
        let mut v: Vec<Element> = Element::ALL
            .iter()
            .copied()
            .filter(|&e| self.dest(e) == want)
            .collect();
        v.sort_by_key(|&e| self.order[e as usize]);
        v
    }

    /// Test-only: a routing from explicit (element, dest) pairs, ordered by
    /// pair position. No backgrounds.
    #[cfg(test)]
    pub fn from_pairs(pairs: &[(Element, Dest)]) -> Self {
        let mut r = Routing::empty();
        for (i, &(e, d)) in pairs.iter().enumerate() {
            r.dests[e as usize] = d;
            r.order[e as usize] = i as u32;
        }
        r
    }
}

/// The line number carried by a `Claude`/`Tmux` dest (0 for `Off`).
fn line_of(d: Dest) -> u8 {
    match d {
        Dest::Claude { line, .. } | Dest::Tmux { line, .. } => line,
        Dest::Off => 0,
    }
}

fn surface_background(surf: &Value) -> Option<(u8, u8, u8)> {
    parse_hex_rgb(surf.get("background")?.as_str()?)
}

/// Apply one surface's regions to the routing, mapping each `(line, align)` to
/// a dest via `mk`. Elements are assigned increasing order values so list
/// order becomes render order.
fn apply_surface(surf: &Value, r: &mut Routing, order: &mut u32, mk: impl Fn(u8, Align) -> Dest) {
    let Some(obj) = surf.as_object() else {
        return;
    };
    for (key, val) in obj {
        if key == "background" {
            continue;
        }
        let (Some((align, line)), Some(list)) = (parse_region(key), val.as_str()) else {
            continue;
        };
        for name in list.split(',') {
            if let Some(e) = Element::from_key(name.trim()) {
                r.dests[e as usize] = mk(line, align);
                r.order[e as usize] = *order;
                *order += 1;
            }
        }
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

    fn cfg(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parse_region_grammar() {
        assert_eq!(parse_region("left"), Some((Align::Left, 0)));
        assert_eq!(parse_region("right"), Some((Align::Right, 0)));
        assert_eq!(parse_region("left.1"), Some((Align::Left, 1)));
        assert_eq!(parse_region("right.12"), Some((Align::Right, 12)));
        assert_eq!(parse_region("centre"), None);
        assert_eq!(parse_region("left.1.2"), None);
        assert_eq!(parse_region("left.x"), None);
    }

    #[test]
    fn builtin_tmux_layout() {
        let r = Routing::builtin(true);
        assert_eq!(
            r.dest(Element::Model),
            Dest::Tmux {
                line: 3,
                align: Align::Left
            }
        );
        assert_eq!(
            r.dest(Element::HeatmapMain),
            Dest::Tmux {
                line: 2,
                align: Align::Left
            }
        );
        assert_eq!(
            r.dest(Element::HeatmapSub),
            Dest::Tmux {
                line: 1,
                align: Align::Left
            }
        );
        assert_eq!(r.dest(Element::Updates), Dest::Off);
        assert_eq!(r.tmux_lines(), vec![1, 2, 3]);
        assert!(r.any_tmux());
    }

    #[test]
    fn builtin_default_layout_is_all_claude() {
        let r = Routing::builtin(false);
        assert_eq!(
            r.dest(Element::Model),
            Dest::Claude {
                line: 0,
                align: Align::Left
            }
        );
        assert_eq!(r.dest(Element::Updates), line0()); // shown outside tmux
        assert_eq!(r.dest(Element::Warmth), Dest::Off); // no live surface
        assert!(!r.any_tmux());
    }

    fn line0() -> Dest {
        Dest::Claude {
            line: 0,
            align: Align::Left,
        }
    }

    #[test]
    fn builds_both_surfaces_with_alignment_and_order() {
        let v = cfg(r#"{ "tmux": {
                "claude": { "left": "cwd, effort", "right": "version" },
                "tmux":   { "left": "model", "right": "warmth",
                            "left.1": "heatmap_main", "left.2": "tokens, limits" }
            } }"#);
        let r = Routing::from_value(Some(v), true);
        // claude surface
        assert_eq!(r.dest(Element::Cwd), line0());
        assert_eq!(
            r.dest(Element::Version),
            Dest::Claude {
                line: 0,
                align: Align::Right
            }
        );
        // tmux surface: line 0 edges
        assert_eq!(
            r.dest(Element::Model),
            Dest::Tmux {
                line: 0,
                align: Align::Left
            }
        );
        assert_eq!(
            r.dest(Element::Warmth),
            Dest::Tmux {
                line: 0,
                align: Align::Right
            }
        );
        // tmux dedicated rows
        assert_eq!(
            r.dest(Element::HeatmapMain),
            Dest::Tmux {
                line: 1,
                align: Align::Left
            }
        );
        // list order within a region is preserved
        assert_eq!(
            r.tmux_at(2, Align::Left),
            vec![Element::Tokens, Element::Limits]
        );
        assert_eq!(
            r.claude_at(0, Align::Left),
            vec![Element::Cwd, Element::Effort]
        );
    }

    #[test]
    fn list_order_overrides_canonical_order() {
        let v = cfg(r#"{ "default": { "claude": { "left": "version, model, cwd" } } }"#);
        let r = Routing::from_value(Some(v), false);
        assert_eq!(
            r.claude_at(0, Align::Left),
            vec![Element::Version, Element::Model, Element::Cwd]
        );
    }

    #[test]
    fn claude_surface_wins_on_conflict() {
        let v = cfg(r#"{ "tmux": {
                "tmux":   { "left": "version" },
                "claude": { "right": "version" }
            } }"#);
        let r = Routing::from_value(Some(v), true);
        assert_eq!(
            r.dest(Element::Version),
            Dest::Claude {
                line: 0,
                align: Align::Right
            }
        );
    }

    #[test]
    fn tmux_surface_ignored_outside_tmux() {
        let v = cfg(r#"{ "default": { "tmux": { "left": "model" } } }"#);
        let r = Routing::from_value(Some(v), false);
        // the default layout has no claude surface and the tmux one is ignored
        assert_eq!(r.dest(Element::Model), Dest::Off);
        assert!(!r.any_tmux());
    }

    #[test]
    fn unlisted_element_is_off() {
        let v = cfg(r#"{ "default": { "claude": { "left": "model" } } }"#);
        let r = Routing::from_value(Some(v), false);
        assert_eq!(r.dest(Element::Cwd), Dest::Off);
    }

    #[test]
    fn missing_layout_falls_back_to_builtin() {
        // config present but only configures tmux; the default context uses the
        // built-in.
        let v = cfg(r#"{ "tmux": { "claude": { "left": "model" } } }"#);
        let r = Routing::from_value(Some(v), false);
        assert_eq!(r.dest(Element::Model), line0());
    }

    #[test]
    fn per_surface_background() {
        let v = cfg(r##"{ "tmux": {
                "claude": { "background": "#1a1b26", "left": "model" },
                "tmux":   { "background": "#222436", "left": "cwd" }
            } }"##);
        let r = Routing::from_value(Some(v), true);
        assert_eq!(r.claude_background(), Some((0x1a, 0x1b, 0x26)));
        assert_eq!(r.tmux_background(), Some((0x22, 0x24, 0x36)));
    }

    #[test]
    fn jump_command_reads_linux_key() {
        let v = cfg(r#"{ "jump": { "linux": "myjump $1" } }"#);
        assert_eq!(jump_command_from(Some(&v)).as_deref(), Some("myjump $1"));
        let blank = cfg(r#"{ "jump": { "linux": "" } }"#);
        assert_eq!(jump_command_from(Some(&blank)), None);
        assert_eq!(jump_command_from(None), None);
    }

    #[test]
    fn hex_parse_rejects_junk() {
        assert_eq!(parse_hex_rgb("#1a1b26"), Some((0x1a, 0x1b, 0x26)));
        assert_eq!(parse_hex_rgb("#1a1b2"), None);
        assert_eq!(parse_hex_rgb("1a1b26"), None);
        assert_eq!(parse_hex_rgb("#xxyyzz"), None);
    }

    /// The bundled example config (== the documented migration of the old
    /// flat layout). Pins the resolved destinations so the in-tmux bar keeps
    /// the same status-format slots: heatmap_main on tmux line 1
    /// (status-format[0]), tokens/limits on line 2 (status-format[1]), the
    /// segments on the base-row edges (line 0).
    #[test]
    fn example_config_resolves_in_both_contexts() {
        let v = cfg(include_str!("../examples/config.json"));

        let t = Routing::from_value(Some(v.clone()), true);
        let l = |line, align| Dest::Tmux { line, align };
        let c = |line, align| Dest::Claude { line, align };
        assert_eq!(t.dest(Element::Model), l(0, Align::Left)); // status-left
        assert_eq!(t.dest(Element::Warmth), l(0, Align::Right)); // status-right
        assert_eq!(t.dest(Element::HeatmapMain), l(1, Align::Left)); // -> slot 0
        assert_eq!(t.dest(Element::Tokens), l(2, Align::Left)); // -> slot 1
        assert_eq!(t.dest(Element::Limits), l(2, Align::Left));
        assert_eq!(t.dest(Element::Cwd), c(0, Align::Left)); // claude statusline
        assert_eq!(t.dest(Element::Version), c(0, Align::Right));
        assert_eq!(t.dest(Element::HeatmapSub), Dest::Off); // unlisted
        assert_eq!(t.dest(Element::Updates), Dest::Off);

        let d = Routing::from_value(Some(v), false);
        assert_eq!(d.dest(Element::Model), c(0, Align::Left));
        assert_eq!(d.dest(Element::Version), c(0, Align::Right));
        assert_eq!(d.dest(Element::Tokens), c(1, Align::Left));
        assert_eq!(d.dest(Element::HeatmapMain), c(2, Align::Left));
        assert_eq!(d.dest(Element::Warmth), Dest::Off); // not listed -> off
        assert!(!d.any_tmux());
    }
}
