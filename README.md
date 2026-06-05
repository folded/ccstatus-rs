# ccstatus

Fast Rust port of [daniel3303/ClaudeCodeStatusLine](https://github.com/daniel3303/ClaudeCodeStatusLine).

A single static binary that reads the JSON Claude Code feeds to a status-line
program on stdin and emits a coloured one-line summary (plus an optional
two-row token-usage heatmap) on stdout.

## Install

With a Rust toolchain (stable):

```sh
cargo install --git https://github.com/folded/ccstatus-rs --locked
```

This drops a `ccstatus` binary into `~/.cargo/bin/` (make sure that's on
`PATH`).

To update later, rerun the same command.

## Wire into Claude Code

Edit `~/.claude/settings.json` and point the status line at the binary:

```json
{
  "statusLine": {
    "type": "command",
    "command": "ccstatus"
  }
}
```

Add CLI flags to disable features you don't want:

```json
{
  "statusLine": {
    "type": "command",
    "command": "ccstatus --no-heatmap --no-cli-version"
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
-h, --help        Show help
-V, --version     Show version
```

Environment variables (overridden by CLI flags):

- `STATUSLINE_HEATMAP=false` — same as `--no-heatmap`
- `STATUSLINE_CHECK_UPDATES=true` — same as `--updates`
- `CLAUDE_CODE_EFFORT_LEVEL=<level>` — fallback when input JSON omits effort

## Build from source

```sh
git clone https://github.com/folded/ccstatus-rs
cd ccstatus-rs
cargo build --release
# binary at target/release/ccstatus
```

## License

MIT
