use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute, queue,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::process::{Command, Stdio};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let split_on = parse_arg(&args, "-split-on").unwrap_or_else(|| "\n\n".to_string());
    let cmd = parse_arg(&args, "-cmd");

    let input = match cmd {
        Some(c) => read_from_pty_command(&c)?,
        None => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    if input.is_empty() {
        return Ok(());
    }

    let pages: Vec<&str> = input.split(&split_on)
        .filter(|p| !p.trim().is_empty())
        .collect();
    if pages.is_empty() {
        return Ok(());
    }
    let page_lines: Vec<Vec<&str>> = pages.iter().map(|p| p.lines().collect()).collect();

    let mut page = 0usize;
    let mut scroll = 0usize;

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let result = run(&mut stdout, &page_lines, &mut page, &mut scroll);

    execute!(stdout, cursor::Show, LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    result
}

fn read_from_pty_command(cmd: &str) -> io::Result<String> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let slave_out = unsafe { Stdio::from_raw_fd(libc::dup(slave)) };
    let slave_err = unsafe { Stdio::from_raw_fd(libc::dup(slave)) };
    unsafe { libc::close(slave) };

    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdout(slave_out)
        .stderr(slave_err)
        .stdin(Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .spawn()?;

    let mut output = Vec::new();
    let mut master_file = unsafe { std::fs::File::from_raw_fd(master) };
    let mut buf = [0u8; 8192];

    loop {
        match Read::read(&mut master_file, &mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => return Err(e),
        }
    }

    child.wait()?;

    // PTY converts \n to \r\n, undo that
    let text = String::from_utf8_lossy(&output);
    Ok(text.replace("\r\n", "\n"))
}

fn run(
    stdout: &mut impl Write,
    page_lines: &[Vec<&str>],
    page: &mut usize,
    scroll: &mut usize,
) -> io::Result<()> {
    loop {
        let (term_w, term_h) = terminal::size()?;
        let content_h = term_h as usize - 1;

        render(stdout, &page_lines[*page], *scroll, content_h, term_w, term_h, *page, page_lines.len())?;

        if let Event::Key(key) = event::read()? {
            let max_scroll = page_lines[*page].len().saturating_sub(content_h);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('j') | KeyCode::Down => {
                    if *scroll < max_scroll {
                        *scroll += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    if *page < page_lines.len() - 1 {
                        *page += 1;
                        *scroll = 0;
                    }
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    if *page > 0 {
                        *page -= 1;
                        *scroll = 0;
                    }
                }
                _ => {}
            }
        }
    }
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

fn parse_arg(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
    }
    None
}
