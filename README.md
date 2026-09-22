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

Or run a command directly (uses a PTY so colors work automatically, and the
command sees the real terminal size):

```
pager --cmd "git diff" --split-on "\n\n"
```

All flags are optional. Default split is `\n\n`.

## Options

| Flag | Description |
| --- | --- |
| `--split-on <TEXT>` | Delimiter to split input into pages. Default `\n\n`. |
| `--cmd <SHELL>` | Run a command and page its output, instead of reading stdin. |
| `--group-by <REGEX>` | Keep consecutive chunks on one page while this regex captures the same text from their first line. |
| `--difft` | Shorthand for difftastic output: one page per file. |
| `--config <PATH>` | Config file with keybinds. Default: `$XDG_CONFIG_HOME/pager/config.toml`. |

### `--group-by`

`--split-on` alone can only cut where the delimiter is, which for tools that
emit many small records per logical unit means a page per record rather than a
page per unit. `--group-by` fixes that without changing where the cuts are: each
chunk's *first non-blank line* is matched against the regex, and consecutive
chunks that capture the same text are merged back into one page. Capture group 1
is the key, or the whole match if the pattern has no groups. The captured text is
shown in the status bar.

Matching ignores ANSI escapes, so patterns are written against the text as it
reads on screen. Chunks whose first line does not match have no key, and a run of
them groups into an untitled page of its own — nothing is ever dropped.

Because only the first line of each chunk is tested, content that happens to look
like a header never causes a spurious split.

## Difftastic

[difftastic](https://difftastic.wilfred.me.uk/) prints one header per hunk:

```
src/main.rs --- 1/4 --- Rust
```

with a blank line between hunks. Under a plain `--split-on "\n\n"` that gives a
page per *hunk*, so a file with four changed regions is spread over four pages.
`--difft` groups them by the path in the header, giving a page per *file*:

```
pager --cmd "git diff" --difft
```

It is shorthand for:

```
pager --cmd "git diff" --group-by '^(.*?) --- '
```

Prefer the `--cmd` form over a pipe. It runs the command on a PTY sized to your
terminal, so difftastic emits color and lays its side-by-side columns out for
your full width; through a pipe it falls back to no color and 80 columns.

To make `git diff` use difftastic, set it as git's external diff:

```
git config --global diff.external difft
```

Then, for history commands, which need `--ext-diff`:

```
pager --cmd "git log -p --ext-diff" --difft
pager --cmd "git show --ext-diff HEAD" --difft
```

A shell function is the most convenient way to use it:

```sh
gd() { pager --cmd "git diff $*" --difft; }
```

If you would rather not set `diff.external` globally, pass it per invocation:

```
pager --cmd "GIT_EXTERNAL_DIFF=difft git diff --ext-diff" --difft
```

`git log -p` puts each commit's metadata before the first file header. That
metadata has no path to group on, so it becomes its own page ahead of the files
it introduces.

## Config

Pager reads `$XDG_CONFIG_HOME/pager/config.toml` (falling back to
`~/.config/pager/config.toml`), or whatever `--config` points at. A missing
default config is fine; a missing `--config` path is an error.

```toml
line_pattern = '^\s*\d*\s+(\d+)\s'

[[bind]]
key = "o"
run = 'zellij action edit --in-place "$(git rev-parse --show-toplevel)/$PAGER_FILE" -l "${PAGER_LINE:-1}"'
```

Each `[[bind]]` maps a key to a shell command, run with `sh -c`. Binds are
checked before the built-in keys, so they can override them. `key` is a single
character, or one of `enter`, `tab`, `esc`, `space`, `backspace`, `f1`–`f12`.

The page is passed in the environment rather than substituted into the command,
so paths with spaces or quotes cannot break it:

| Variable | Value |
| --- | --- |
| `PAGER_FILE` | The current page's title — the file path, under `--difft`. Empty if the page has none. |
| `PAGER_LINE` | Line number pulled from the top visible row by `line_pattern`. Empty if unset or unmatched. |
| `PAGER_PAGE` | Current page number, 1-based. |
| `PAGER_TOTAL` | Total pages. |

Commands are not waited on, so a bind that takes over the terminal will not
block the pager. Output is discarded; a non-zero exit is reported in the status
bar. Nothing is quoted for you — quote `"$PAGER_FILE"` yourself.

### `line_pattern`

An optional regex, matched against each visible row from the top down, on the
ANSI-stripped text. The first row that matches sets `PAGER_LINE` from capture
group 1 (or the whole match). Without it, `PAGER_LINE` is empty.

For difftastic, treat this as **approximate**. Its gutter right-aligns the
old-file and new-file line numbers in fixed-width columns, and a row showing
only an old-file number is padded so it looks exactly like a new-file one. The
pattern above therefore reports the old number on removal-only rows, landing you
near the change rather than on it. Use `--display inline`; side-by-side puts the
new-file number after the old file's text, where no regex can reliably find it.

## Opening the current file in an editor

Because binds are just shell commands, pager needs to know nothing about your
multiplexer. Under zellij, `edit --in-place` suspends pager's own pane, opens
`$EDITOR` there, and restores pager when the editor exits — like `v` in `less`:

```toml
[[bind]]
key = "o"
run = 'zellij action edit --in-place "$(git rev-parse --show-toplevel)/$PAGER_FILE" -l "${PAGER_LINE:-1}"'
```

Two things to know:

- The command must run **inside** the zellij pane. That is what pager does, but
  the same command typed in another session silently does nothing.
- difftastic prints repo-relative paths, so resolve them against the repo root
  with `git rev-parse --show-toplevel`, as above.

To open in a new pane instead of in place, drop `--in-place` and add a direction:

```toml
run = 'zellij action edit "$(git rev-parse --show-toplevel)/$PAGER_FILE" -l "${PAGER_LINE:-1}" -d down'
```

## Keys

- `j`/`k` or arrows - scroll up/down
- `d`/`u` or PgDn/PgUp - half-page scroll
- `gg`/Home - top of page
- `ge`/`G`/End - bottom of page
- `h`/`l` or left/right - prev/next page
- `q`/Esc - quit
