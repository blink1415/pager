use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute, queue,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::{Command, ExitStatus, Stdio};

#[derive(Parser)]
#[command(about = "A terminal pager that splits input into pages")]
struct Args {
    /// Delimiter to split input into pages
    #[arg(long, default_value = "\n\n")]
    split_on: String,

    /// Shell command to read input from (via PTY)
    #[arg(long)]
    cmd: Option<String>,
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

    let page_lines = split_pages(&input, &args.split_on);
    if page_lines.is_empty() {
        return Ok(());
    }

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let _guard = RawModeGuard;

    run(&mut stdout, &page_lines)
}

fn run(stdout: &mut impl Write, page_lines: &[Vec<&str>]) -> io::Result<()> {
    let mut page = 0usize;
    let mut scroll = 0usize;
    let mut prev_key: Option<KeyCode> = None;
    let half_page = |h: usize| (h / 2).max(1);

    loop {
        let (width, height) = terminal::size()?;
        let content_h = height as usize - 1;

        render(stdout, &page_lines[page], scroll, content_h, width, height, page, page_lines.len())?;

        match event::read()? {
            Event::Key(key) => {
                let max_scroll = page_lines[page].len().saturating_sub(content_h);
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
                        if page < page_lines.len() - 1 {
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
                let max_scroll = page_lines[page].len().saturating_sub(content_h);
                scroll = scroll.min(max_scroll);
            }
            _ => {}
        }
    }
}


fn read_from_pty_command(cmd: &str) -> io::Result<String> {
    let pty = rustix_openpty::openpty(None, None)?;
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
    lines: &[&str],
    scroll: usize,
    height: usize,
    width: u16,
    term_h: u16,
    page: usize,
    total: usize,
) -> io::Result<()> {
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

    let status = format!(" [{}/{}] ", page + 1, total);
    let padded = format!("{:<w$}", status, w = width as usize);
    queue!(buf, cursor::MoveTo(0, term_h - 1))?;
    buf.extend_from_slice(format!("\x1b[7m{}\x1b[0m", padded).as_bytes());

    stdout.write_all(&buf)?;
    stdout.flush()
}

fn split_pages<'a>(input: &'a str, delimiter: &str) -> Vec<Vec<&'a str>> {
    input
        .split(delimiter)
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.lines().collect())
        .collect()
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

    // -- split_pages --

    #[test]
    fn split_pages_default_delimiter() {
        let input = "page one\n\npage two\n\npage three";
        let pages = split_pages(input, "\n\n");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0], vec!["page one"]);
        assert_eq!(pages[1], vec!["page two"]);
        assert_eq!(pages[2], vec!["page three"]);
    }

    #[test]
    fn split_pages_multiline_page() {
        let input = "line1\nline2\n\nline3\nline4\nline5";
        let pages = split_pages(input, "\n\n");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0], vec!["line1", "line2"]);
        assert_eq!(pages[1], vec!["line3", "line4", "line5"]);
    }

    #[test]
    fn split_pages_custom_delimiter() {
        let input = "aaa---bbb---ccc";
        let pages = split_pages(input, "---");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0], vec!["aaa"]);
    }

    #[test]
    fn split_pages_filters_empty() {
        let input = "\n\n\n\nactual content\n\n\n\n";
        let pages = split_pages(input, "\n\n");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], vec!["actual content"]);
    }

    #[test]
    fn split_pages_empty_input() {
        assert!(split_pages("", "\n\n").is_empty());
    }

    #[test]
    fn split_pages_only_whitespace() {
        assert!(split_pages("   \n\n   \n\n   ", "\n\n").is_empty());
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
        let lines = vec!["hello", "world"];
        render(&mut buf, &lines, 0, 10, 40, 11, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("[1/1]"));
    }

    #[test]
    fn render_respects_scroll() {
        let mut buf = Vec::new();
        let lines = vec!["line0", "line1", "line2", "line3"];
        render(&mut buf, &lines, 2, 10, 40, 11, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.contains("line0"));
        assert!(!output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[test]
    fn render_respects_height() {
        let mut buf = Vec::new();
        let lines = vec!["a", "b", "c", "d", "e"];
        render(&mut buf, &lines, 0, 2, 40, 3, 0, 1).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("a"));
        assert!(output.contains("b"));
        assert!(!output.contains("c"));
    }

    #[test]
    fn render_page_indicator() {
        let mut buf = Vec::new();
        let lines = vec!["x"];
        render(&mut buf, &lines, 0, 10, 40, 11, 2, 5).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("[3/5]"));
    }

    #[test]
    fn render_ansi_prefix_on_scroll() {
        let mut buf = Vec::new();
        let lines = vec!["\x1b[31mred", "still red", "more"];
        render(&mut buf, &lines, 1, 10, 40, 11, 0, 1).unwrap();
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
