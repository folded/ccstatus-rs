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
`~/.config/ccstatus/config.json`, keyed by **layout** (`tmux` when inside tmux,
else `default`), then **surface** (`claude` = the statusline, `tmux` = the
bar), then **region** (`left`/`right` with an optional line index, e.g.
`left.1`):

```json
{
  "tmux": {
    "claude": { "left": "cwd, effort", "right": "version" },
    "tmux":   { "left": "model", "right": "warmth",
                "left.1": "heatmap_main", "left.2": "tokens, limits" }
  },
  "default": {
    "claude": { "left": "model, cwd, effort", "right": "version",
                "left.1": "tokens, limits", "left.2": "heatmap_main" }
  }
}
```

A region's value is an ordered element list (list order is render order);
unlisted elements are hidden. `background` is a reserved per-surface key
(`"#rrggbb"`) that paints that surface a consistent colour instead of
inheriting the terminal or tmux theme. The file hot-reloads on save. See
[`examples/README.md`](examples/README.md) for the full grammar. With no
matching layout, ccstatus uses a sensible built-in.

## Ghostty mode (no tmux)

For Claude sessions running directly in [Ghostty](https://ghostty.org),
an opt-in `ghostty` config block stamps each session's **tab title** with the
same activity/git label the tmux window flag uses (`◐ ccstatus-rs ↑`,
`⚑ repo` for a session that finished while you weren't looking) — the
across-tab "which one needs me?" cue without tmux. See
[`examples/README.md`](examples/README.md#ghostty-activity-in-the-tab-title-no-tmux).

## Build from source

```sh
git clone https://github.com/folded/ccstatus-rs
cd ccstatus-rs
cargo build --release
# binary at target/release/ccstatus
```

### Picking up a rebuild

The statusline command is re-executed by Claude Code on every render, so it
runs the freshly built binary immediately. The per-session **tmux daemon**,
however, is long-lived: it hot-reloads `config.json` (by mtime) but keeps its
own executable until restarted, so a code change won't show on the tmux bar
until the running daemon is replaced. After a rebuild, kill it — it respawns
from the new binary on the next render:

```sh
pkill -f 'ccstatus --session'
```

`ccstatus --tmux-reset` restores the tmux defaults if a crashed daemon ever
leaves the bar in a bad state (it resets bar options; it does not kill the
daemon).

## License

MIT
