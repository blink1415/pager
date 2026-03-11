# pager

Split piped text into pages. Preserves ANSI colors/formatting.

## Install

```
cargo install --git https://github.com/blink1415/pager
```

## Usage

```
some-command | pager --split-on "---"
```

Or run a command directly (uses a PTY so colors work automatically):

```
pager --cmd "git diff" --split-on "\n\n"
```

Both flags are optional. Default split is `\n\n`.

## Keys

- `j`/`k` or arrows - scroll up/down
- `d`/`u` or PgDn/PgUp - half-page scroll
- `gg`/Home - top of page
- `ge`/`G`/End - bottom of page
- `h`/`l` or left/right - prev/next page
- `q`/Esc - quit
