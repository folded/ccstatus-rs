use std::env;

use crate::hooks::HookKind;
use crate::render_tmux::RenderFlavor;

pub struct Config {
    pub cwd: bool,
    pub git: bool,
    pub tokens: bool,
    pub effort: bool,
    pub limits: bool,
    pub cli_version: bool,
    pub heatmap: bool,
    pub updates: bool,
}

impl Default for Config {
    fn default() -> Self {
        let heatmap = env::var("STATUSLINE_HEATMAP")
            .as_deref()
            .unwrap_or("true")
            != "false";
        let updates = env::var("STATUSLINE_CHECK_UPDATES")
            .as_deref()
            .unwrap_or("false")
            == "true";
        Self {
            cwd: true,
            git: true,
            tokens: true,
            effort: true,
            limits: true,
            cli_version: true,
            heatmap,
            updates,
        }
    }
}

pub enum ParseOutcome {
    Run(Config),
    Hook(HookKind),
    Render(RenderFlavor, String),
    /// Recompute the tmux status-row count for the focused pane and tell
    /// the running tmux server. Optional pane id is treated as a hint
    /// only — the handler queries tmux for the actually-focused pane
    /// because not every hook event reports the new focus correctly.
    TmuxOnFocus(Option<String>),
    /// Run the long-lived tmux control-mode daemon.
    Daemon,
    Install,
    Help,
    Version,
    Error(String),
}

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> ParseOutcome {
    let mut cfg = Config::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-cwd" => {
                cfg.cwd = false;
                cfg.git = false;
            }
            "--no-git" => cfg.git = false,
            "--no-tokens" => cfg.tokens = false,
            "--no-effort" => cfg.effort = false,
            "--no-limits" => cfg.limits = false,
            "--no-cli-version" => cfg.cli_version = false,
            "--no-heatmap" => cfg.heatmap = false,
            "--heatmap" => cfg.heatmap = true,
            "--updates" => cfg.updates = true,
            "--no-updates" => cfg.updates = false,
            "--install" => return ParseOutcome::Install,
            "--daemon" => return ParseOutcome::Daemon,
            "--tmux-on-focus" => {
                let hint = iter.next().filter(|s| !s.is_empty());
                return ParseOutcome::TmuxOnFocus(hint);
            }
            "--hook" => {
                let kind = match iter.next().as_deref() {
                    Some("stop") => HookKind::Stop,
                    Some(other) => {
                        return ParseOutcome::Error(format!("unknown hook kind: {other}"));
                    }
                    None => {
                        return ParseOutcome::Error(
                            "--hook requires a kind (e.g. --hook stop)".into(),
                        );
                    }
                };
                return ParseOutcome::Hook(kind);
            }
            "--render-tmux" => {
                let flavor = match iter.next().as_deref() {
                    Some("row") => RenderFlavor::Row,
                    Some("line") => {
                        let n = match iter.next() {
                            Some(s) => match s.parse::<usize>() {
                                Ok(n) => n,
                                Err(_) => {
                                    return ParseOutcome::Error(format!(
                                        "--render-tmux line: not an integer: {s}"
                                    ));
                                }
                            },
                            None => {
                                return ParseOutcome::Error(
                                    "--render-tmux line requires an index and pane id".into(),
                                );
                            }
                        };
                        RenderFlavor::Line(n)
                    }
                    Some(other) => {
                        return ParseOutcome::Error(format!(
                            "unknown --render-tmux flavor: {other}"
                        ));
                    }
                    None => {
                        return ParseOutcome::Error(
                            "--render-tmux requires <flavor> <pane_id>".into(),
                        );
                    }
                };
                let pane_id = match iter.next() {
                    Some(p) if !p.is_empty() => p,
                    _ => {
                        return ParseOutcome::Error(
                            "--render-tmux requires a <pane_id> argument".into(),
                        );
                    }
                };
                return ParseOutcome::Render(flavor, pane_id);
            }
            "-h" | "--help" => return ParseOutcome::Help,
            "-V" | "--version" => return ParseOutcome::Version,
            other => return ParseOutcome::Error(format!("unknown argument: {other}")),
        }
    }
    ParseOutcome::Run(cfg)
}

pub const HELP: &str = "\
ccstatus - status line for Claude Code

Usage: ccstatus [OPTIONS]

Reads the Claude Code session JSON from stdin and prints a status line.

Options:
  --no-cwd          Hide current directory (also hides git info)
  --no-git          Hide git branch and diff stats
  --no-tokens       Hide token usage block
  --no-effort       Hide reasoning effort label
  --no-limits       Hide rate-limit and quota info
  --no-cli-version  Hide installed Claude CLI version
  --no-heatmap      Hide the per-day token-usage heatmap rows
  --updates         Check for newer ccstatus releases (off by default)
  --no-updates      Disable update check (default)
  --install         Wire this binary into ~/.claude/settings.json and exit
  --hook <kind>     Run as a Claude Code hook (kinds: stop)
  --daemon          Run the long-lived tmux control-mode daemon that
                    drives the status bar while Claude sessions are active
  --render-tmux <flavor> <pane_id>
                    Emit a tmux status line for the given pane.
                    Flavors: row | line <N>
  --tmux-on-focus [<pane_id>]
                    Resize the tmux status bar based on whether the focused
                    pane has a registered Claude session. Run from tmux
                    focus-related hooks. The pane id is a hint only; the
                    handler queries tmux for the actually-focused pane.
  -h, --help        Show this help and exit
  -V, --version     Show ccstatus version and exit

Environment:
  STATUSLINE_HEATMAP=false         Same as --no-heatmap
  STATUSLINE_CHECK_UPDATES=true    Same as --updates
  CLAUDE_CODE_EFFORT_LEVEL=<lvl>   Fallback effort level when not in input

CLI flags take precedence over environment variables.
";
