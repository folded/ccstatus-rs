# ccstatus

Fast Rust port of [daniel3303/ClaudeCodeStatusLine](https://github.com/daniel3303/ClaudeCodeStatusLine).

A single static binary that reads the JSON Claude Code feeds to a status-line
program on stdin and emits a coloured one-line summary (plus an optional
two-row token-usage heatmap) on stdout.

## Install

Two steps with a Rust toolchain (stable):

```sh
cargo install --git https://github.com/folded/ccstatus-rs --locked
ccstatus --install
```

The first command drops a `ccstatus` binary into `~/.cargo/bin/` (make sure
that's on `PATH`). The second writes `statusLine.command` plus the `Stop` and
`UserPromptSubmit` hooks into `~/.claude/settings.json` (respecting
`$CLAUDE_CONFIG_DIR`), preserving any other keys already in the file. The hooks
feed the per-session state that `ccstatus top` and the tmux warmth indicator
read; they leave any non-`ccstatus` hooks you've configured untouched.
Re-running `ccstatus --install` after an upgrade refreshes the path; it refuses
to clobber a `statusLine` set to a non-`ccstatus` command.

To customise which blocks render, edit `statusLine.command` in
`settings.json` afterwards:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/Users/you/.cargo/bin/ccstatus --no-heatmap --no-cli-version"
  }
}
```

## Options

```
--no-cwd          Hide current directory (also hides git info)
--no-git          Hide git branch and diff stats
--no-tokens       Hide token usage block
--no-effort       Hide reasoning effort label
--no-limits       Hide rate-limit and quota info
--no-cli-version  Hide installed Claude CLI version
--no-heatmap      Hide the per-day token-usage heatmap rows
--updates         Check for newer ccstatus releases (off by default)
--install         Wire this binary into ~/.claude/settings.json and exit
-h, --help        Show help
-V, --version     Show version
```

Subcommands:

```
ccstatus top      Interactive table of every live Claude session, with
                  jump-to-session (Enter). j/k or arrows move, r refreshes,
                  q quits. "htop for Claude sessions."
```

Enter jumps to the selected session: in tmux it switches panes; for a Claude
running directly in a terminal it raises that emulator's OS window (macOS
iTerm2/Terminal out of the box; Linux X11 via `wmctrl`/`xdotool`, with a
`jump.linux` config hook for Wayland or other setups — see
[`examples/README.md`](examples/README.md)).

Environment variables (overridden by CLI flags):

- `STATUSLINE_HEATMAP=false` — same as `--no-heatmap`
- `STATUSLINE_CHECK_UPDATES=true` — same as `--updates`
- `CLAUDE_CODE_EFFORT_LEVEL=<level>` — fallback when input JSON omits effort

## tmux mode

Inside tmux, ccstatus can drive the tmux status bar instead of (or alongside)
Claude's own statusline, so the live cache-warmth indicator keeps ticking while
a session sits idle — something Claude's pull-only statusline can't do. This is
automatic: while a Claude pane is focused, a per-session helper composes
ccstatus content onto session-local bar overrides and reverts cleanly when you
switch away or Claude exits. **Your tmux config is never modified** and the bar
behaves normally when Claude isn't active. Outside tmux ccstatus renders to
Claude's own statusline, which can span multiple lines and right-align segments
(see below).

Which elements land where is controlled by an optional
`~/.config/ccstatus/config.json` — each element (`model`, `cwd`, `tokens`,
`effort`, `limits`, `version`, `updates`, `warmth`, `heatmap_main`,
`heatmap_sub`) routes to a dedicated tmux row, the base row's
`status-left`/`status-right`, Claude's statusline (optionally a specific line
and left/right alignment, e.g. `claude.1.right`), or `off`. An optional
`"background"` hex colour (e.g. `"#1a1b26"`) paints ccstatus's surfaces a
consistent colour instead of inheriting the terminal or tmux theme. The file
hot-reloads on save. See [`examples/README.md`](examples/README.md) for a
worked config and the destination table. With no config file, ccstatus uses a
sensible default layout.

If a crashed helper ever leaves the bar in a bad state, `ccstatus --tmux-reset`
restores the tmux defaults.

## Build from source

```sh
git clone https://github.com/folded/ccstatus-rs
cd ccstatus-rs
cargo build --release
# binary at target/release/ccstatus
```

## License

MIT
