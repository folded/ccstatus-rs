use std::env;

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
    Help,
    Version,
    Error(String),
}

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> ParseOutcome {
    let mut cfg = Config::default();
    for arg in args {
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
  -h, --help        Show this help and exit
  -V, --version     Show ccstatus version and exit

Environment:
  STATUSLINE_HEATMAP=false         Same as --no-heatmap
  STATUSLINE_CHECK_UPDATES=true    Same as --updates
  CLAUDE_CODE_EFFORT_LEVEL=<lvl>   Fallback effort level when not in input

CLI flags take precedence over environment variables.
";
