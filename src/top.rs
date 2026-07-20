//! `ccstatus top` — an interactive aggregate view of every live Claude
//! session, with "take me to this Claude" (jump).
//!
//! A poll-driven aggregate surface: it reads the whole state dir via
//! [`crate::fleet`] (no tmux focus needed), renders a table, and on Enter
//! jumps to the selected session's pane. Same-server jumps go straight through
//! the [`crate::tmux`] seam; cross-server jumps route through that server's
//! handler (which lives in the right tmux environment) via
//! [`crate::ipc::notify_focus`].
//!
//! Built on `ratatui` (with its bundled `crossterm` backend): the `Table`
//! widget owns column layout, truncation, and selection, and ratatui's
//! double-buffered renderer owns the redraw — so there's no hand-rolled
//! cursor/clear/width bookkeeping here.

use std::io::{self, IsTerminal, Stdout};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use crate::fleet::{self, Activity, SessionView};
use crate::ipc;
use crate::tmux::{self, Tmux};
use crate::usage;
use crate::window;

/// How often the table re-reads the state dir (also the input poll timeout, so
/// keys stay responsive while we wait for the next refresh). Short, so the
/// recency (`--lru`) order settles quickly after you switch sessions — the view
/// stamps it sorts on are only refreshed on the daemons' tick.
const REFRESH: Duration = Duration::from_millis(600);

// Truecolor palette, mirroring `crate::color` for the ANSI statusline.
const C_BLUE: Color = Color::Rgb(0, 153, 255);
const C_ORANGE: Color = Color::Rgb(255, 176, 85);
const C_GREEN: Color = Color::Rgb(0, 160, 0);
const C_CYAN: Color = Color::Rgb(46, 149, 153);
const C_WHITE: Color = Color::Rgb(220, 220, 220);
const C_RED: Color = Color::Rgb(255, 85, 85);
const C_YELLOW: Color = Color::Rgb(230, 200, 0);

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run(lru: bool) -> ExitCode {
    match run_inner(lru) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccstatus top: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Collect the fleet, ordered for the active mode: recency (most-recently-seen
/// first — tab-switcher) when `lru`, else the triage order `fleet::collect`
/// already applies.
fn collect(lru: bool) -> Vec<SessionView> {
    let mut views = fleet::collect();
    if lru {
        views.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.claude_session.cmp(&b.claude_session))
        });
    }
    views
}

fn run_inner(lru: bool) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(io::Error::other("not a terminal"));
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.hide_cursor()?;

    let res = event_loop(&mut terminal, lru);

    // Restore the terminal regardless of how the loop ended.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop(terminal: &mut Term, lru: bool) -> io::Result<()> {
    let my_server = tmux::server_id();
    let mut views = collect(lru);
    let mut state = TableState::default();
    // Tab-switcher: start on the *second* row (the session you were in before
    // this one), like alt-tab — so a bare invoke+Enter flips you back. Triage
    // starts on the top (most urgent) row.
    state.select(selected_index(if lru { 1 } else { 0 }, views.len()));
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &views, &mut state, my_server.as_deref(), lru))?;

        // Block for input only until the next scheduled refresh.
        let timeout = REFRESH.saturating_sub(last_refresh.elapsed());
        if event::poll(timeout)?
            && let Event::Key(k) = event::read()?
            && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('j') | KeyCode::Down => move_selection(&mut state, &views, 1),
                KeyCode::Char('k') | KeyCode::Up => move_selection(&mut state, &views, -1),
                KeyCode::Char('r') => {
                    views = collect(lru);
                    last_refresh = Instant::now();
                    reclamp(&mut state, &views);
                }
                KeyCode::Enter => {
                    if let Some(v) = state.selected().and_then(|i| views.get(i))
                        && v.jumpable
                    {
                        jump(v, my_server.as_deref());
                        break; // jumping switches the client away; nothing to show
                    }
                }
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH {
            views = collect(lru);
            last_refresh = Instant::now();
            reclamp(&mut state, &views);
        }
    }
    Ok(())
}

/// Perform the jump. In tmux: same server as us → straight through the tmux
/// seam (fast); a different server (or we're not in tmux) → route through that
/// server's handler, which runs in the correct tmux environment. Not in tmux:
/// raise the hosting OS terminal window (iTerm2/Terminal). Caller gates on
/// `jumpable`, so at least one path applies.
fn jump(v: &SessionView, my_server: Option<&str>) {
    if let Some(addr) = &v.address {
        if my_server == Some(addr.server_id.as_str()) {
            tmux::CliTmux.focus_pane(&addr.pane_id);
        } else {
            ipc::notify_focus(&addr.server_id, &addr.pane_id);
        }
        return;
    }
    if let Some(w) = &v.window {
        window::focus(w);
    }
}

// ---- selection -----------------------------------------------------------

/// The valid selection for a list of `len` rows given a desired index: `None`
/// when empty, else clamped into range.
fn selected_index(want: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| want.min(len - 1))
}

fn move_selection(state: &mut TableState, views: &[SessionView], delta: i64) {
    if views.is_empty() {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i64;
    let next = (cur + delta).clamp(0, views.len() as i64 - 1) as usize;
    state.select(Some(next));
}

/// Keep the selection in range after the row set changes under us.
fn reclamp(state: &mut TableState, views: &[SessionView]) {
    state.select(selected_index(state.selected().unwrap_or(0), views.len()));
}

// ---- rendering -----------------------------------------------------------

fn ui(
    f: &mut Frame,
    views: &[SessionView],
    state: &mut TableState,
    my_server: Option<&str>,
    lru: bool,
) {
    let [header, divider, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(Paragraph::new(header_line(views)), header);
    f.render_widget(divider_line(divider), divider);

    if views.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("No live Claude sessions.", dim())),
            body,
        );
    } else {
        f.render_stateful_widget(table(views, my_server, body.width), body, state);
    }

    // Note the sort mode so it's clear whether rows are triaged or by recency.
    let order = if lru { "recent" } else { "triage" };
    f.render_widget(
        Paragraph::new(Line::styled(
            format!("j/k or ↑/↓ move · Enter jump · r refresh · q quit · sort: {order}"),
            dim(),
        )),
        footer,
    );
}

fn sep() -> Span<'static> {
    Span::styled(" · ", dim())
}

fn header_line(views: &[SessionView]) -> Line<'static> {
    let count = |a: Activity| views.iter().filter(|v| v.activity == a).count();
    let needs_input = count(Activity::NeedsInput);
    let suspended = count(Activity::Suspended);
    let working = count(Activity::Working);
    let bg = count(Activity::BgRunning);
    let waiting = count(Activity::Waiting);
    let mut spans = vec![
        Span::styled("ccstatus", Style::default().fg(C_BLUE)),
        sep(),
        Span::raw(format!("{} session(s)", views.len())),
    ];
    // Lead with the attention-grabbers only when present.
    if needs_input > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("⚑ {needs_input} need input"),
            Style::default().fg(C_RED).add_modifier(Modifier::BOLD),
        ));
    }
    if suspended > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{suspended} stopped"),
            Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(sep());
    spans.push(Span::styled(
        format!("{working} working"),
        Style::default().fg(C_GREEN),
    ));
    // Background runners only when present, so the common case stays uncluttered.
    if bg > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("⚙ {bg} bg"),
            Style::default().fg(C_CYAN),
        ));
    }
    spans.push(sep());
    spans.push(Span::styled(
        format!("{waiting} waiting"),
        Style::default().fg(C_ORANGE),
    ));
    spans.extend(usage_spans());
    Line::from(spans)
}

/// The account-global usage tail of the header (5h/7d/extra), or empty when no
/// usage cache exists. Account usage is identical across sessions, so it lives
/// once in the header rather than per row.
fn usage_spans() -> Vec<Span<'static>> {
    let Some(u) = usage::summary() else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut pct = |label: &str, p: i64| {
        spans.push(sep());
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(C_WHITE),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{p}%"),
            Style::default().fg(usage_color(p)),
        ));
    };
    if let Some(p) = u.five_hour_pct {
        pct("5h", p);
    }
    if let Some(p) = u.seven_day_pct {
        pct("7d", p);
    }
    if u.extra_enabled {
        spans.push(sep());
        spans.push(Span::styled("extra", Style::default().fg(C_WHITE)));
        spans.push(Span::raw(" "));
        let tail = match (u.extra_used, u.extra_limit) {
            (Some(used), Some(limit)) if used > 0.0 || limit > 0.0 => {
                format!("${used:.2}/${limit:.2}")
            }
            _ => "enabled".to_string(),
        };
        spans.push(Span::styled(tail, Style::default().fg(C_GREEN)));
    }
    spans
}

fn usage_color(pct: i64) -> Color {
    match pct {
        p if p >= 90 => C_RED,
        p if p >= 70 => C_ORANGE,
        p if p >= 50 => C_YELLOW,
        _ => C_GREEN,
    }
}

fn divider_line(area: Rect) -> Paragraph<'static> {
    Paragraph::new(Span::styled("─".repeat(area.width as usize), dim()))
}

fn table<'a>(views: &'a [SessionView], my_server: Option<&str>, width: u16) -> Table<'a> {
    let dir_w = dir_width(width);
    let rows = views.iter().map(|v| session_row(v, my_server, dir_w));
    let widths = [
        // Directory first: it's the most useful "which session is this" cue, so
        // it leads and gets the slack. The cell is pre-elided to `dir_w` keeping
        // the tail (last path component), so it never clips the useful end.
        Constraint::Length(dir_w as u16),
        Constraint::Length(9),  // activity ("⚑ waiting" is the widest)
        Constraint::Length(22), // model (longest is "Opus 4.8 (1M context)")
        Constraint::Length(8),  // ctx N%
        Constraint::Length(10), // idle <age>
        Constraint::Length(13), // note (jumpability) — "(not in tmux)" = 13
    ];
    Table::new(rows, widths)
        .column_spacing(1)
        .highlight_symbol(Span::styled("▶ ", Style::default().fg(C_BLUE)))
}

/// Columns to the left of the fixed ones, i.e. the directory column's width:
/// the table width minus the fixed columns, the inter-column spacing, the
/// selection gutter, and a 1-col margin so the elided path can't spill into the
/// activity column. Floored so a narrow terminal still shows a usable stub.
fn dir_width(total: u16) -> usize {
    const FIXED: u16 = 9 + 22 + 8 + 10 + 13;
    const SPACING: u16 = 5; // 6 columns at column_spacing(1)
    const GUTTER: u16 = 2; // the "▶ " highlight symbol
    const MARGIN: u16 = 1;
    total
        .saturating_sub(FIXED + SPACING + GUTTER + MARGIN)
        .max(10) as usize
}

fn session_row<'a>(v: &'a SessionView, my_server: Option<&str>, dir_w: usize) -> Row<'a> {
    // A worktree shows its compact `⎇ repo/leaf` label (the parent repo is lost
    // if we elide the raw `.claude/worktrees/…` path from the head); a normal
    // checkout keeps its full path, elided from the head to keep the useful tail.
    let raw = v.cwd.as_deref().unwrap_or("");
    let dir_text = if crate::util::worktree_of(raw).is_some() {
        elide_left(&crate::util::dir_label(raw), dir_w)
    } else {
        elide_left(raw, dir_w)
    };
    let dir = Span::styled(dir_text, Style::default().fg(C_CYAN));
    // Finished while you weren't looking floats up with a flag, the same ⚑ cue
    // as NeedsInput — the word (waiting/idle) says why it's flagged.
    let activity = if v.attention {
        Span::styled(
            match v.activity {
                Activity::Idle => "⚑ idle",
                _ => "⚑ waiting",
            },
            Style::default().fg(C_ORANGE).add_modifier(Modifier::BOLD),
        )
    } else {
        match v.activity {
            Activity::Suspended => Span::styled(
                "stopped",
                Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Activity::NeedsInput => Span::styled(
                "⚑ input",
                Style::default().fg(C_RED).add_modifier(Modifier::BOLD),
            ),
            Activity::Working => Span::styled("working", Style::default().fg(C_GREEN)),
            Activity::BgRunning => Span::styled("⚙ bg", Style::default().fg(C_CYAN)),
            Activity::Waiting => Span::styled("waiting", Style::default().fg(C_ORANGE)),
            Activity::Idle => Span::styled("idle", dim()),
            Activity::Unknown => Span::styled("-", dim()),
        }
    };
    let model = Span::styled(
        v.model.as_deref().unwrap_or("Claude"),
        Style::default().fg(C_WHITE),
    );
    let ctx = v
        .context_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "-".into());
    let age = v.idle_secs.map(human_age).unwrap_or_else(|| "-".into());
    // `*` = on a different tmux server than us (still jumpable via its handler).
    let here = match &v.address {
        Some(a) if my_server != Some(a.server_id.as_str()) => "*",
        _ => "",
    };
    let note = match (&v.address, &v.window) {
        (Some(_), _) if !v.jumpable => "(no handler)",
        (Some(_), _) => "",
        // No tmux pane: jumpable iff we can raise its OS window.
        (None, Some(_)) => "",
        (None, None) => "(not in tmux)",
    };

    Row::new(vec![
        Line::from(dir),
        Line::from(activity),
        Line::from(model),
        Line::from(vec![Span::styled("ctx ", dim()), Span::raw(ctx)]),
        Line::from(vec![
            Span::styled("idle ", dim()),
            Span::raw(format!("{age}{here}")),
        ]),
        Line::styled(note, dim()),
    ])
}

/// Truncate `s` to `width` columns keeping its *right* end — for a path that's
/// the last component, which identifies a session far better than its shared
/// prefix — eliding the head with `…`. Char-based, which is fine for paths.
fn elide_left(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (width - 1)).collect();
    format!("…{tail}")
}

fn human_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_age_units() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(90), "1m");
        assert_eq!(human_age(3700), "1h");
        assert_eq!(human_age(90_000), "1d");
    }

    #[test]
    fn selection_clamps_to_row_count() {
        assert_eq!(selected_index(0, 0), None);
        assert_eq!(selected_index(5, 0), None);
        assert_eq!(selected_index(5, 3), Some(2));
        assert_eq!(selected_index(1, 3), Some(1));
    }

    #[test]
    fn usage_color_thresholds() {
        assert_eq!(usage_color(95), C_RED);
        assert_eq!(usage_color(75), C_ORANGE);
        assert_eq!(usage_color(55), C_YELLOW);
        assert_eq!(usage_color(10), C_GREEN);
    }

    fn view(cwd: &str, address: Option<crate::fleet::PaneAddr>, jumpable: bool) -> SessionView {
        SessionView {
            claude_session: "s".into(),
            model: Some("Opus 4.8 (1M context)".into()),
            cwd: Some(cwd.into()),
            context_pct: Some(7),
            activity: Activity::Waiting,
            attention: false,
            idle_secs: Some(42),
            last_seen: Some(1_000),
            address,
            window: None,
            jumpable,
        }
    }

    /// Render one frame to ratatui's test backend and read the cells back as
    /// text: confirms the header, the row, and the not-in-tmux note all land,
    /// and that a long cwd is elided from the *head* (keeping the useful tail)
    /// rather than clipped from the tail or overrunning the table width.
    #[test]
    fn renders_header_row_and_elides_cwd_head() {
        use ratatui::backend::TestBackend;

        let long = "/Users/tjs/populationgenomics/metamist/.claude/worktrees/some-very-long-branch";
        let views = vec![view(long, None, false)];
        let mut term = Terminal::new(TestBackend::new(80, 8)).unwrap();
        let mut state = TableState::default();
        state.select(Some(0));
        term.draw(|f| ui(f, &views, &mut state, None, false))
            .unwrap();

        let buf = term.backend().buffer();
        let mut lines: Vec<String> = Vec::new();
        for y in 0..buf.area.height {
            let mut s = String::new();
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            lines.push(s.trim_end().to_string());
        }
        let text = lines.join("\n");

        assert!(text.contains("ccstatus"), "header missing:\n{text}");
        assert!(text.contains("1 session(s)"), "count missing:\n{text}");
        assert!(text.contains("▶"), "selection marker missing:\n{text}");
        assert!(text.contains("waiting"), "activity missing:\n{text}");
        assert!(text.contains("(not in tmux)"), "note missing:\n{text}");
        // The head is elided and the tail (last component) is preserved; the
        // shared prefix is gone.
        assert!(text.contains('…'), "cwd not elided:\n{text}");
        assert!(text.contains("branch"), "cwd tail missing:\n{text}");
        assert!(
            !text.contains("Users"),
            "cwd prefix should be elided:\n{text}"
        );
        // No physical line exceeds the 80-col terminal.
        for l in &lines {
            assert!(l.chars().count() <= 80, "line over width: {l:?}");
        }
    }

    #[test]
    fn elide_left_keeps_the_tail() {
        assert_eq!(elide_left("/a/b/short", 20), "/a/b/short");
        assert_eq!(elide_left("/very/long/path/to/proj", 10), "…h/to/proj");
        assert_eq!(elide_left("anything", 0), "");
        // The result never exceeds the requested width.
        assert!(elide_left("/very/long/path/to/proj", 10).chars().count() <= 10);
    }
}
