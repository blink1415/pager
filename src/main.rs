use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute, queue,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use regex::Regex;
use rustix::termios::Winsize;
use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

/// Matches a difftastic hunk header (`path --- 2/7 --- Rust`), capturing the path.
const DIFFT_HEADER: &str = r"^(.*?) --- ";

#[derive(Parser)]
#[command(about = "A terminal pager that splits input into pages")]
struct Args {
    /// Delimiter to split input into pages
    #[arg(long, default_value = "\n\n")]
    split_on: String,

    /// Shell command to read input from (via PTY)
    #[arg(long)]
    cmd: Option<String>,

    /// Keep consecutive chunks on one page while this regex captures the same
    /// text from their first line (capture group 1, or the whole match)
    #[arg(long, value_name = "REGEX")]
    group_by: Option<String>,

    /// Shorthand for difftastic output: one page per file
    #[arg(long, conflicts_with = "group_by")]
    difft: bool,

    /// Config file with keybinds [default: $XDG_CONFIG_HOME/pager/config.toml]
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

/// A key the config binds to a shell command.
struct Bind {
    key: KeyCode,
    run: String,
}

#[derive(Default)]
struct Config {
    binds: Vec<Bind>,
    /// Pulls a file line number out of a visible line, for $PAGER_LINE.
    line_pattern: Option<Regex>,
}

/// Everything `render` draws in one frame.
struct View<'a> {
    page: &'a Page<'a>,
    scroll: usize,
    /// Rows available for content, i.e. the terminal height minus the status bar.
    height: usize,
    width: u16,
    term_h: u16,
    index: usize,
    total: usize,
    message: Option<&'a str>,
}

/// One page of input, plus the text `--group-by` captured for it (a file path,
/// for difftastic) to show in the status bar.
struct Page<'a> {
    title: Option<String>,
    lines: Vec<&'a str>,
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let input = match args.cmd {
        Some(ref c) => read_from_pty_command(c)?,
        None => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    if input.is_empty() {
        return Ok(());
    }

    let group_by = match args.difft {
        true => Some(DIFFT_HEADER),
        false => args.group_by.as_deref(),
    };
    let group_by = group_by
        .map(Regex::new)
        .transpose()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let pages = split_pages(&input, &args.split_on, group_by.as_ref());
    if pages.is_empty() {
        return Ok(());
    }

    let config = load_config(args.config.as_deref())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let _guard = RawModeGuard;

    run(&mut stdout, &pages, &config)
}

fn run(stdout: &mut impl Write, pages: &[Page], config: &Config) -> io::Result<()> {
    let mut page = 0usize;
    let mut scroll = 0usize;
    let mut prev_key: Option<KeyCode> = None;
    let mut message: Option<String> = None;
    let mut pending: Option<Child> = None;
    let half_page = |h: usize| (h / 2).max(1);

    loop {
        let (width, height) = terminal::size()?;
        let content_h = height as usize - 1;

        // Reap a finished bind so a failure gets reported rather than lost.
        if let Some(err) = reap(&mut pending) {
            message = Some(err);
        }

        render(
            stdout,
            &View {
                page: &pages[page],
                scroll,
                height: content_h,
                width,
                term_h: height,
                index: page,
                total: pages.len(),
                message: message.as_deref(),
            },
        )?;

        match event::read()? {
            Event::Key(key) => {
                let max_scroll = pages[page].lines.len().saturating_sub(content_h);
                message = None;

                if let Some(bind) = config.binds.iter().find(|b| b.key == key.code) {
                    let line = config
                        .line_pattern
                        .as_ref()
                        .and_then(|re| visible_line_number(&pages[page].lines, scroll, content_h, re));
                    message = spawn_bind(bind, &pages[page], page, pages.len(), line, &mut pending)
                        .err()
                        .map(|e| format!("bind '{}': {}", bind.run, e));
                    prev_key = Some(key.code);
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => {
                        scroll = max_scroll.min(scroll + 1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        scroll = scroll.saturating_sub(1);
                    }
                    KeyCode::Char('d') | KeyCode::PageDown => {
                        scroll = max_scroll.min(scroll + half_page(content_h));
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => {
                        scroll = scroll.saturating_sub(half_page(content_h));
                    }
                    KeyCode::Char('g') if prev_key == Some(KeyCode::Char('g')) => {
                        scroll = 0;
                        prev_key = None;
                        continue;
                    }
                    KeyCode::Char('e') if prev_key == Some(KeyCode::Char('g')) => {
                        scroll = max_scroll;
                        prev_key = None;
                        continue;
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        scroll = max_scroll;
                    }
                    KeyCode::Home => {
                        scroll = 0;
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        if page < pages.len() - 1 {
                            page += 1;
                            scroll = 0;
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        if page > 0 {
                            page -= 1;
                            scroll = 0;
                        }
                    }
                    _ => {}
                }
                prev_key = Some(key.code);
            }
            Event::Resize(_, _) => {
                let max_scroll = pages[page].lines.len().saturating_sub(content_h);
                scroll = scroll.min(max_scroll);
            }
            _ => {}
        }
    }
}


fn read_from_pty_command(cmd: &str) -> io::Result<String> {
    // Hand the child the real terminal size. Left at the 0x0 default, width-aware
    // tools fall back to 80 columns — difftastic would size its side-by-side
    // columns for a terminal narrower than the one we are about to draw on.
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let winsize = Winsize {
        ws_row: rows.saturating_sub(1).max(1), // the last row is ours, for the status bar
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = rustix_openpty::openpty(None, Some(&winsize))?;
    let user_dup = pty.user.as_fd().try_clone_to_owned()?;

    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::from(pty.user))
        .stderr(Stdio::from(user_dup))
        .stdin(Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .spawn()?;

    let mut output = Vec::new();
    let mut master_file = std::fs::File::from(pty.controller);
    let mut buf = [0u8; 8192];

    loop {
        match Read::read(&mut master_file, &mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(ref e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => break,
            Err(e) => return Err(e),
        }
    }

    let status = child.wait()?;
    check_cmd_status(status, cmd)?;

    // PTY converts \n to \r\n, undo that
    let text = String::from_utf8_lossy(&output);
    Ok(text.replace("\r\n", "\n"))
}

fn check_cmd_status(status: ExitStatus, cmd: &str) -> io::Result<()> {
    if status.success() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("command '{}' exited with {}", cmd, status),
    ))
}


fn render(stdout: &mut impl Write, view: &View) -> io::Result<()> {
    let View { scroll, width, .. } = *view;
    let lines = &view.page.lines[..];
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    queue!(buf, terminal::Clear(ClearType::All))?;

    let ansi_prefix = collect_ansi_state(&lines[..scroll]);

    let end = lines.len().min(scroll + view.height);
    for (i, line) in lines[scroll..end].iter().enumerate() {
        queue!(buf, cursor::MoveTo(0, i as u16))?;
        if i == 0 && !ansi_prefix.is_empty() {
            buf.extend_from_slice(ansi_prefix.as_bytes());
        }
        buf.extend_from_slice(line.as_bytes());
    }

    buf.extend_from_slice(b"\x1b[0m");

    let status = match (view.message, &view.page.title) {
        (Some(message), _) => format!(" [{}/{}] {} ", view.index + 1, view.total, message),
        (None, Some(title)) => format!(" [{}/{}] {} ", view.index + 1, view.total, title),
        (None, None) => format!(" [{}/{}] ", view.index + 1, view.total),
    };
    let clipped: String = status.chars().take(width as usize).collect();
    let padded = format!("{:<w$}", clipped, w = width as usize);
    queue!(buf, cursor::MoveTo(0, view.term_h - 1))?;
    buf.extend_from_slice(format!("\x1b[7m{}\x1b[0m", padded).as_bytes());

    stdout.write_all(&buf)?;
    stdout.flush()
}

/// Loads the config from `explicit`, or from the default path if it exists.
/// A missing default config is not an error; a missing explicit one is.
fn load_config(explicit: Option<&std::path::Path>) -> Result<Config, String> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => match default_config_path() {
            Some(p) if p.is_file() => p,
            _ => return Ok(Config::default()),
        },
    };

    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {}", path.display(), e))?;
    parse_config(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

fn default_config_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(dir.join("pager").join("config.toml"))
}

fn parse_config(text: &str) -> Result<Config, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("{}", e))?;
    let mut config = Config::default();

    if let Some(value) = table.get("line_pattern") {
        let pattern = value
            .as_str()
            .ok_or("line_pattern must be a string")?;
        config.line_pattern =
            Some(Regex::new(pattern).map_err(|e| format!("line_pattern: {}", e))?);
    }

    let Some(binds) = table.get("bind") else {
        return Ok(config);
    };
    for bind in binds
        .as_array()
        .ok_or("bind must be a table array, written as [[bind]]")?
    {
        let key = bind
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("every [[bind]] needs a string 'key'")?;
        let run = bind
            .get("run")
            .and_then(|v| v.as_str())
            .ok_or("every [[bind]] needs a string 'run'")?;
        let key = parse_key(key).ok_or_else(|| format!("unrecognised key '{}'", key))?;
        config.binds.push(Bind {
            key,
            run: run.to_string(),
        });
    }
    Ok(config)
}

fn parse_key(name: &str) -> Option<KeyCode> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    match name.to_ascii_lowercase().as_str() {
        "enter" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "esc" => Some(KeyCode::Esc),
        "space" => Some(KeyCode::Char(' ')),
        "backspace" => Some(KeyCode::Backspace),
        rest => rest
            .strip_prefix('f')
            .and_then(|n| n.parse().ok())
            .filter(|n| (1..=12).contains(n))
            .map(KeyCode::F),
    }
}

/// First line number the pattern finds in the visible rows, for $PAGER_LINE.
fn visible_line_number(lines: &[&str], scroll: usize, height: usize, re: &Regex) -> Option<String> {
    let end = lines.len().min(scroll + height);
    lines.get(scroll..end)?.iter().find_map(|line| {
        let plain = strip_ansi(line);
        let caps = re.captures(&plain)?;
        let m = caps.get(1).or_else(|| caps.get(0))?;
        Some(m.as_str().to_string())
    })
}

/// Runs a bind's command under `sh`, with the page exposed as environment
/// variables. The command is not waited on: a bind that suspends this pane (an
/// editor opening in place) would otherwise block the loop until it returned.
fn spawn_bind(
    bind: &Bind,
    page: &Page,
    index: usize,
    total: usize,
    line: Option<String>,
    pending: &mut Option<Child>,
) -> Result<(), String> {
    let child = Command::new("sh")
        .args(["-c", &bind.run])
        .env("PAGER_FILE", page.title.clone().unwrap_or_default())
        .env("PAGER_LINE", line.unwrap_or_default())
        .env("PAGER_PAGE", (index + 1).to_string())
        .env("PAGER_TOTAL", total.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    // Only one bind is tracked at a time; an earlier one is left to finish on its own.
    *pending = Some(child);
    Ok(())
}

/// Collects a finished bind, returning a message if it failed.
fn reap(pending: &mut Option<Child>) -> Option<String> {
    let child = pending.as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) if status.success() => {
            *pending = None;
            None
        }
        Ok(Some(status)) => {
            *pending = None;
            Some(format!("bind exited with {}", status))
        }
        Ok(None) => None,
        Err(e) => {
            *pending = None;
            Some(format!("bind failed: {}", e))
        }
    }
}

fn split_pages<'a>(input: &'a str, delimiter: &str, group_by: Option<&Regex>) -> Vec<Page<'a>> {
    let mut ranges: Vec<(Option<String>, usize, usize)> = Vec::new();

    for (start, end) in chunk_ranges(input, delimiter) {
        let chunk = &input[start..end];
        if chunk.trim().is_empty() {
            continue;
        }

        let title = group_by.and_then(|re| chunk_key(chunk, re));
        // Neighbouring chunks share a page while their keys match: consecutive
        // difftastic hunks for one file land on one page, and runs of chunks the
        // pattern does not recognise (a commit message ahead of its files) group
        // together into a page of their own.
        let merge = group_by.is_some() && ranges.last().is_some_and(|(prev, ..)| *prev == title);

        match ranges.last_mut() {
            // Extend over the delimiter rather than rejoining the lines, so the
            // merged page keeps the input's original spacing.
            Some(last) if merge => last.2 = end,
            _ => ranges.push((title, start, end)),
        }
    }

    ranges
        .into_iter()
        .map(|(title, start, end)| Page {
            title,
            lines: input[start..end].lines().collect(),
        })
        .collect()
}

/// Byte ranges of the delimiter-separated chunks of `input`, in order.
fn chunk_ranges(input: &str, delimiter: &str) -> Vec<(usize, usize)> {
    if delimiter.is_empty() {
        return vec![(0, input.len())];
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(offset) = input[start..].find(delimiter) {
        let end = start + offset;
        ranges.push((start, end));
        start = end + delimiter.len();
    }
    ranges.push((start, input.len()));
    ranges
}

/// The text `re` captures from a chunk's first non-blank line — group 1, or the
/// whole match when the pattern has no groups. Matching ignores ANSI escapes, so
/// a pattern is written against the text as it reads on screen.
fn chunk_key(chunk: &str, re: &Regex) -> Option<String> {
    let first = chunk.lines().find(|l| !l.trim().is_empty())?;
    let plain = strip_ansi(first);
    let caps = re.captures(&plain)?;
    let m = caps.get(1).or_else(|| caps.get(0))?;
    Some(m.as_str().to_string())
}

fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }

    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(esc) = rest.find('\x1b') {
        out.push_str(&rest[..esc]);
        let tail = &rest[esc..];
        rest = &tail[escape_len(tail)..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Length of the escape sequence starting at `s`. A CSI sequence runs to its
/// first final byte; anything else is treated as a bare ESC.
fn escape_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.get(1) != Some(&b'[') {
        return 1;
    }
    let mut i = 2;
    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
        i += 1;
    }
    (i + 1).min(bytes.len())
}

fn collect_ansi_state(lines: &[&str]) -> String {
    let mut state = String::new();
    for line in lines {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\x1b' && bytes.get(i + 1) == Some(&b'[') {
                let start = i;
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' && bytes[i] != b'\x1b' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'm' {
                    let seq = &line[start..=i];
                    if seq == "\x1b[0m" || seq == "\x1b[m" {
                        state.clear();
                    } else {
                        state.push_str(seq);
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn page_lines<'a>(input: &'a str, delimiter: &str) -> Vec<Vec<&'a str>> {
        split_pages(input, delimiter, None)
            .into_iter()
            .map(|p| p.lines)
            .collect()
    }

    fn difft_pages(input: &str) -> Vec<Page<'_>> {
        split_pages(input, "\n\n", Some(&Regex::new(DIFFT_HEADER).unwrap()))
    }

    fn page(lines: Vec<&str>) -> Page<'_> {
        Page { title: None, lines }
    }

    fn view<'a>(page: &'a Page<'a>, scroll: usize, height: usize, width: u16) -> View<'a> {
        View {
            page,
            scroll,
            height,
            width,
            term_h: height as u16 + 1,
            index: 0,
            total: 1,
            message: None,
        }
    }

    // -- split_pages --

    #[test]
    fn split_pages_default_delimiter() {
        let input = "page one\n\npage two\n\npage three";
        let pages = page_lines(input, "\n\n");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0], vec!["page one"]);
        assert_eq!(pages[1], vec!["page two"]);
        assert_eq!(pages[2], vec!["page three"]);
    }

    #[test]
    fn split_pages_multiline_page() {
        let input = "line1\nline2\n\nline3\nline4\nline5";
        let pages = page_lines(input, "\n\n");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0], vec!["line1", "line2"]);
        assert_eq!(pages[1], vec!["line3", "line4", "line5"]);
    }

    #[test]
    fn split_pages_custom_delimiter() {
        let input = "aaa---bbb---ccc";
        let pages = page_lines(input, "---");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0], vec!["aaa"]);
    }

    #[test]
    fn split_pages_filters_empty() {
        let input = "\n\n\n\nactual content\n\n\n\n";
        let pages = page_lines(input, "\n\n");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], vec!["actual content"]);
    }

    #[test]
    fn split_pages_empty_input() {
        assert!(page_lines("", "\n\n").is_empty());
    }

    #[test]
    fn split_pages_only_whitespace() {
        assert!(page_lines("   \n\n   \n\n   ", "\n\n").is_empty());
    }

    #[test]
    fn split_pages_empty_delimiter_is_one_page() {
        let pages = page_lines("a\nb", "");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], vec!["a", "b"]);
    }

    // -- split_pages with --group-by --

    #[test]
    fn group_by_merges_consecutive_hunks_of_one_file() {
        let input = "a.rs --- 1/2 --- Rust\nfirst\n\na.rs --- 2/2 --- Rust\nsecond\n\nb.rs --- Rust\nthird";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title.as_deref(), Some("a.rs"));
        assert_eq!(
            pages[0].lines,
            vec!["a.rs --- 1/2 --- Rust", "first", "", "a.rs --- 2/2 --- Rust", "second"]
        );
        assert_eq!(pages[1].title.as_deref(), Some("b.rs"));
        assert_eq!(pages[1].lines, vec!["b.rs --- Rust", "third"]);
    }

    #[test]
    fn group_by_separates_interleaved_files() {
        let input = "a.rs --- Rust\none\n\nb.rs --- Rust\ntwo\n\na.rs --- Rust\nthree";
        let titles: Vec<_> = difft_pages(input)
            .iter()
            .map(|p| p.title.clone().unwrap())
            .collect();
        assert_eq!(titles, vec!["a.rs", "b.rs", "a.rs"]);
    }

    #[test]
    fn group_by_ignores_ansi_in_the_header() {
        let input = "\x1b[1m\x1b[93ma.rs\x1b[39m\x1b[0m\x1b[2m --- 1/2 --- Rust\x1b[0m\nfirst\n\n\x1b[1m\x1b[93ma.rs\x1b[39m\x1b[0m\x1b[2m --- 2/2 --- Rust\x1b[0m\nsecond";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].title.as_deref(), Some("a.rs"));
    }

    #[test]
    fn group_by_puts_unmatched_chunks_on_their_own_page() {
        let input = "a.rs --- Rust\nfirst\n\nstray line\n\nb.rs --- Rust\nsecond";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].title.as_deref(), Some("a.rs"));
        assert_eq!(pages[1].title, None);
        assert!(pages[1].lines.contains(&"stray line"));
        assert_eq!(pages[2].title.as_deref(), Some("b.rs"));
    }

    #[test]
    fn group_by_merges_a_run_of_unmatched_chunks() {
        // a commit message ahead of its file diffs stays on one page
        let input = "commit abc123\nAuthor: someone\n\n    subject line\n\na.rs --- Rust\nfirst";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title, None);
        assert_eq!(
            pages[0].lines,
            vec!["commit abc123", "Author: someone", "", "    subject line"]
        );
        assert_eq!(pages[1].title.as_deref(), Some("a.rs"));
    }

    #[test]
    fn group_by_keeps_a_leading_preamble_as_its_own_page() {
        let input = "commit abc123\nAuthor: someone\n\na.rs --- Rust\nfirst";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title, None);
        assert_eq!(pages[0].lines, vec!["commit abc123", "Author: someone"]);
    }

    #[test]
    fn group_by_only_reads_the_first_line_of_a_chunk() {
        // a content line containing " --- " must not start a new page
        let input = "a.rs --- Rust\n 1  1 let x = \"a --- b\";\n\na.rs --- Rust\nmore";
        let pages = difft_pages(input);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].title.as_deref(), Some("a.rs"));
    }

    #[test]
    fn group_by_without_capture_group_uses_whole_match() {
        let re = Regex::new(r"^== \w+").unwrap();
        let input = "== alpha\none\n\n== alpha\ntwo\n\n== beta\nthree";
        let pages = split_pages(input, "\n\n", Some(&re));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title.as_deref(), Some("== alpha"));
        assert_eq!(pages[1].title.as_deref(), Some("== beta"));
    }

    #[test]
    fn difft_header_captures_path_with_spaces() {
        let re = Regex::new(DIFFT_HEADER).unwrap();
        let key = chunk_key("my dir/some file.rs --- 1/3 --- Rust", &re);
        assert_eq!(key.as_deref(), Some("my dir/some file.rs"));
    }

    // -- strip_ansi --

    #[test]
    fn strip_ansi_leaves_plain_text_borrowed() {
        assert!(matches!(strip_ansi("plain"), Cow::Borrowed("plain")));
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[93mfile\x1b[0m --- Rust"), "file --- Rust");
    }

    #[test]
    fn strip_ansi_handles_trailing_escape() {
        assert_eq!(strip_ansi("text\x1b"), "text");
        assert_eq!(strip_ansi("text\x1b["), "text");
    }

    #[test]
    fn strip_ansi_preserves_multibyte_text() {
        assert_eq!(strip_ansi("\x1b[31mnaïve → ok\x1b[0m"), "naïve → ok");
    }

    // -- collect_ansi_state --

    #[test]
    fn ansi_state_empty() {
        assert_eq!(collect_ansi_state(&[]), "");
    }

    #[test]
    fn ansi_state_no_escapes() {
        assert_eq!(collect_ansi_state(&["hello", "world"]), "");
    }

    #[test]
    fn ansi_state_tracks_color() {
        let lines = &["\x1b[31mred text"];
        assert_eq!(collect_ansi_state(lines), "\x1b[31m");
    }

    #[test]
    fn ansi_state_reset_clears() {
        let lines = &["\x1b[31mred\x1b[0m plain"];
        assert_eq!(collect_ansi_state(lines), "");
    }

    #[test]
    fn ansi_state_short_reset_clears() {
        let lines = &["\x1b[32mgreen\x1b[m plain"];
        assert_eq!(collect_ansi_state(lines), "");
    }

    #[test]
    fn ansi_state_accumulates_across_lines() {
        let lines = &["\x1b[1mbold", "\x1b[31mred"];
        assert_eq!(collect_ansi_state(lines), "\x1b[1m\x1b[31m");
    }

    #[test]
    fn ansi_state_reset_mid_sequence() {
        let lines = &["\x1b[1m\x1b[31mbold red", "\x1b[0m\x1b[34mblue"];
        assert_eq!(collect_ansi_state(lines), "\x1b[34m");
    }

    // -- render --

    #[test]
    fn render_basic_output() {
        let mut buf = Vec::new();
        let p = page(vec!["hello", "world"]);
        render(&mut buf, &view(&p, 0, 10, 40)).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("[1/1]"));
    }

    #[test]
    fn render_respects_scroll() {
        let mut buf = Vec::new();
        let p = page(vec!["line0", "line1", "line2", "line3"]);
        render(&mut buf, &view(&p, 2, 10, 40)).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.contains("line0"));
        assert!(!output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[test]
    fn render_respects_height() {
        let mut buf = Vec::new();
        let p = page(vec!["a", "b", "c", "d", "e"]);
        render(&mut buf, &view(&p, 0, 2, 40)).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("a"));
        assert!(output.contains("b"));
        assert!(!output.contains("c"));
    }

    #[test]
    fn render_page_indicator() {
        let mut buf = Vec::new();
        let p = page(vec!["x"]);
        let v = View { index: 2, total: 5, ..view(&p, 0, 10, 40) };
        render(&mut buf, &v).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[3/5]"));
    }

    #[test]
    fn render_shows_page_title() {
        let mut buf = Vec::new();
        let p = Page { title: Some("src/main.rs".into()), lines: vec!["x"] };
        let v = View { total: 3, ..view(&p, 0, 10, 40) };
        render(&mut buf, &v).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[1/3] src/main.rs"));
    }

    #[test]
    fn render_clips_status_to_width() {
        let mut buf = Vec::new();
        let p = Page { title: Some("a/very/long/path/that/overflows.rs".into()), lines: vec!["x"] };
        let v = View { total: 3, ..view(&p, 0, 10, 20) };
        render(&mut buf, &v).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let status = output.rsplit("\x1b[7m").next().unwrap();
        let status = status.strip_suffix("\x1b[0m").unwrap();
        assert_eq!(status.chars().count(), 20);
    }

    #[test]
    fn render_ansi_prefix_on_scroll() {
        let mut buf = Vec::new();
        let p = page(vec!["\x1b[31mred", "still red", "more"]);
        render(&mut buf, &view(&p, 1, 10, 40)).unwrap();
        let output = String::from_utf8(buf).unwrap();
        // scrolled past line 0 which set red — prefix should carry it forward
        assert!(output.contains("\x1b[31m"));
        assert!(output.contains("still red"));
    }

    #[test]
    fn render_message_replaces_the_title() {
        let mut buf = Vec::new();
        let p = Page { title: Some("src/main.rs".into()), lines: vec!["x"] };
        let v = View { message: Some("bind exited with 1"), ..view(&p, 0, 10, 60) };
        render(&mut buf, &v).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("bind exited with 1"));
        assert!(!output.contains("src/main.rs"));
    }

    // -- config --

    #[test]
    fn parse_config_empty_is_default() {
        let c = parse_config("").unwrap();
        assert!(c.binds.is_empty());
        assert!(c.line_pattern.is_none());
    }

    #[test]
    fn parse_config_reads_binds() {
        let c = parse_config(
            r#"
            [[bind]]
            key = "o"
            run = "echo one"

            [[bind]]
            key = "enter"
            run = "echo two"
            "#,
        )
        .unwrap();
        assert_eq!(c.binds.len(), 2);
        assert_eq!(c.binds[0].key, KeyCode::Char('o'));
        assert_eq!(c.binds[0].run, "echo one");
        assert_eq!(c.binds[1].key, KeyCode::Enter);
    }

    #[test]
    fn parse_config_reads_line_pattern() {
        let c = parse_config(r#"line_pattern = '^\s*\d*\s+(\d+)\s'"#).unwrap();
        assert!(c.line_pattern.is_some());
    }

    #[test]
    fn parse_config_rejects_bad_input() {
        assert!(parse_config("line_pattern = 5").is_err());
        assert!(parse_config("line_pattern = '('").is_err());
        assert!(parse_config("[[bind]]\nrun = \"x\"").is_err());
        assert!(parse_config("[[bind]]\nkey = \"o\"").is_err());
        assert!(parse_config("[[bind]]\nkey = \"nope\"\nrun = \"x\"").is_err());
        assert!(parse_config("bind = 3").is_err());
    }

    #[test]
    fn parse_key_forms() {
        assert_eq!(parse_key("o"), Some(KeyCode::Char('o')));
        assert_eq!(parse_key("O"), Some(KeyCode::Char('O')));
        assert_eq!(parse_key("?"), Some(KeyCode::Char('?')));
        assert_eq!(parse_key("space"), Some(KeyCode::Char(' ')));
        assert_eq!(parse_key("Enter"), Some(KeyCode::Enter));
        assert_eq!(parse_key("f5"), Some(KeyCode::F(5)));
        assert_eq!(parse_key("f13"), None);
        assert_eq!(parse_key("nonsense"), None);
    }

    // -- visible_line_number --

    /// The pattern the README recommends for `difft --display inline`.
    const INLINE_PATTERN: &str = r"^\s*\d*\s+(\d+)\s";

    #[test]
    fn line_number_from_difft_inline_gutter() {
        // rows copied from real `difft --display inline` output
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let added = vec!["     1 use clap::Parser;"];
        let added_later = vec!["    10 use std::process::{Command, ExitStatus, Stdio};"];
        let context = vec!["  2   3 edition = \"2024\""];
        assert_eq!(visible_line_number(&added, 0, 10, &re).as_deref(), Some("1"));
        assert_eq!(visible_line_number(&added_later, 0, 10, &re).as_deref(), Some("10"));
        assert_eq!(visible_line_number(&context, 0, 10, &re).as_deref(), Some("3"));
    }

    #[test]
    fn line_number_is_approximate_on_removal_rows() {
        // A removal row carries only an old-file number, and the gutter pads it
        // with leading spaces, so the pattern cannot tell it from a new-file one
        // and reports 14 where the new file has drifted a little past it. The
        // jump lands near the change rather than exactly on it.
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let removed = vec![r#" 14        let cmd = parse_arg(&args, "-cmd");"#];
        assert_eq!(visible_line_number(&removed, 0, 10, &re).as_deref(), Some("14"));
    }

    #[test]
    fn line_number_skips_rows_with_no_number_at_all() {
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let lines = vec!["src/main.rs --- 1/4 --- Rust", "    9 use std::os::fd::AsFd;"];
        assert_eq!(visible_line_number(&lines, 0, 10, &re).as_deref(), Some("9"));
    }

    #[test]
    fn line_number_respects_scroll_and_height() {
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let lines = vec!["   1 one", "   2 two", "   3 three"];
        assert_eq!(visible_line_number(&lines, 1, 10, &re).as_deref(), Some("2"));
        // only the first visible row is in range, so the later match is not used
        assert_eq!(visible_line_number(&lines, 0, 1, &re).as_deref(), Some("1"));
    }

    #[test]
    fn line_number_ignores_ansi() {
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let lines = vec!["\x1b[2m  9  12\x1b[0m code"];
        assert_eq!(visible_line_number(&lines, 0, 10, &re).as_deref(), Some("12"));
    }

    #[test]
    fn line_number_none_when_nothing_matches() {
        let re = Regex::new(INLINE_PATTERN).unwrap();
        let lines = vec!["no numbers here"];
        assert_eq!(visible_line_number(&lines, 0, 10, &re), None);
    }

    // -- check_cmd_status --

    #[test]
    fn check_cmd_status_success() {
        let status = ExitStatus::from_raw(0);
        assert!(check_cmd_status(status, "echo hi").is_ok());
    }

    #[test]
    fn check_cmd_status_failure() {
        // exit code 1 — raw value is code << 8 on unix
        let status = ExitStatus::from_raw(1 << 8);
        let err = check_cmd_status(status, "false").unwrap_err();
        assert!(err.to_string().contains("false"));
        assert!(err.to_string().contains("exit"));
    }
}
