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

## Keys

- `j`/`k` or arrows - scroll up/down
- `d`/`u` or PgDn/PgUp - half-page scroll
- `gg`/Home - top of page
- `ge`/`G`/End - bottom of page
- `h`/`l` or left/right - prev/next page
- `q`/Esc - quit
