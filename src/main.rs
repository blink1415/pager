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
use std::process::{Command, ExitStatus, Stdio};

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

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let _guard = RawModeGuard;

    run(&mut stdout, &pages)
}

fn run(stdout: &mut impl Write, pages: &[Page]) -> io::Result<()> {
    let mut page = 0usize;
    let mut scroll = 0usize;
    let mut prev_key: Option<KeyCode> = None;
    let half_page = |h: usize| (h / 2).max(1);

    loop {
        let (width, height) = terminal::size()?;
        let content_h = height as usize - 1;

        render(stdout, &pages[page], scroll, content_h, width, height, page, pages.len())?;

        match event::read()? {
            Event::Key(key) => {
                let max_scroll = pages[page].lines.len().saturating_sub(content_h);
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


fn render(
    stdout: &mut impl Write,
    page_content: &Page,
    scroll: usize,
    height: usize,
    width: u16,
    term_h: u16,
    page: usize,
    total: usize,
) -> io::Result<()> {
    let lines = &page_content.lines[..];
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    queue!(buf, terminal::Clear(ClearType::All))?;

    let ansi_prefix = collect_ansi_state(&lines[..scroll]);

    let end = lines.len().min(scroll + height);
    for (i, line) in lines[scroll..end].iter().enumerate() {
        queue!(buf, cursor::MoveTo(0, i as u16))?;
        if i == 0 && !ansi_prefix.is_empty() {
            buf.extend_from_slice(ansi_prefix.as_bytes());
        }
        buf.extend_from_slice(line.as_bytes());
    }

    buf.extend_from_slice(b"\x1b[0m");

    let status = match &page_content.title {
        Some(title) => format!(" [{}/{}] {} ", page + 1, total, title),
        None => format!(" [{}/{}] ", page + 1, total),
    };
    let clipped: String = status.chars().take(width as usize).collect();
    let padded = format!("{:<w$}", clipped, w = width as usize);
    queue!(buf, cursor::MoveTo(0, term_h - 1))?;
    buf.extend_from_slice(format!("\x1b[7m{}\x1b[0m", padded).as_bytes());

    stdout.write_all(&buf)?;
    stdout.flush()
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
        render(&mut buf, &page(vec!["hello", "world"]), 0, 10, 40, 11, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("[1/1]"));
    }

    #[test]
    fn render_respects_scroll() {
        let mut buf = Vec::new();
        let p = page(vec!["line0", "line1", "line2", "line3"]);
        render(&mut buf, &p, 2, 10, 40, 11, 0, 1).unwrap();
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
        render(&mut buf, &p, 0, 2, 40, 3, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("a"));
        assert!(output.contains("b"));
        assert!(!output.contains("c"));
    }

    #[test]
    fn render_page_indicator() {
        let mut buf = Vec::new();
        render(&mut buf, &page(vec!["x"]), 0, 10, 40, 11, 2, 5).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[3/5]"));
    }

    #[test]
    fn render_shows_page_title() {
        let mut buf = Vec::new();
        let p = Page { title: Some("src/main.rs".into()), lines: vec!["x"] };
        render(&mut buf, &p, 0, 10, 40, 11, 0, 3).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[1/3] src/main.rs"));
    }

    #[test]
    fn render_clips_status_to_width() {
        let mut buf = Vec::new();
        let p = Page { title: Some("a/very/long/path/that/overflows.rs".into()), lines: vec!["x"] };
        render(&mut buf, &p, 0, 10, 20, 11, 0, 3).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let status = output.rsplit("\x1b[7m").next().unwrap();
        let status = status.strip_suffix("\x1b[0m").unwrap();
        assert_eq!(status.chars().count(), 20);
    }

    #[test]
    fn render_ansi_prefix_on_scroll() {
        let mut buf = Vec::new();
        let p = page(vec!["\x1b[31mred", "still red", "more"]);
        render(&mut buf, &p, 1, 10, 40, 11, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        // scrolled past line 0 which set red — prefix should carry it forward
        assert!(output.contains("\x1b[31m"));
        assert!(output.contains("still red"));
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
