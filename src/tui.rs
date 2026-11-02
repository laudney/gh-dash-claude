//! The terminal side of the dialog.

use std::io::Stderr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, Once};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{cursor, event, execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::dialog::{Compose, Outcome};
use crate::keydec::{Decoder, Key};

/// A termination signal arrived while the dialog was open.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

static SIGNALLED: AtomicBool = AtomicBool::new(false);
/// The terminal is in dialog mode and needs a restore.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static UI_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);
static PANIC_HOOK: Once = Once::new();

const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// Whether a termination signal arrived since the dialog opened.
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

/// Undo dialog mode; safe to call more than once.
fn restore_terminal() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let mut err = std::io::stderr();
        let _ = execute!(
            err,
            event::DisableBracketedPaste,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn now_ms() -> i64 {
    crate::time::now_ms()
}

/// The open dialog. Dropping it restores the terminal.
pub struct Tui {
    term: Terminal<CrosstermBackend<Stderr>>,
    input: OwnedFd,
    decoder: Decoder,
    old_actions: Vec<(libc::c_int, libc::sigaction)>,
}

impl Tui {
    /// Enter dialog mode. On any failure the steps
    /// already done are undone.
    pub fn enter() -> Result<Tui> {
        SIGNALLED.store(false, Ordering::SeqCst);
        // 1. Signal handlers first, before any terminal change.
        let mut old_actions = Vec::new();
        for sig in SIGNALS {
            // SAFETY: zeroed sigaction is a valid starting value; the
            // handler only stores to an atomic, which is async-signal-safe.
            unsafe {
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut new.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &new, &mut old) == 0 {
                    old_actions.push((sig, old));
                }
            }
        }
        let undo_signals = |acts: &[(libc::c_int, libc::sigaction)]| {
            for (sig, old) in acts {
                // SAFETY: restores the disposition saved above.
                unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
            }
        };
        let fail = |acts: &[(libc::c_int, libc::sigaction)], e: anyhow::Error| {
            restore_terminal();
            undo_signals(acts);
            Err(e)
        };

        // 2. The input device.
        let input = match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
            Ok(f) => OwnedFd::from(f),
            Err(e) => return fail(&old_actions, anyhow!("opening /dev/tty: {e}")),
        };
        // SAFETY: drops input queued on our own terminal descriptor.
        unsafe { libc::tcflush(input.as_raw_fd(), libc::TCIFLUSH) };

        // 3. Terminal modes, each undone by `restore_terminal`.
        let setup = || -> Result<Terminal<CrosstermBackend<Stderr>>> {
            terminal::enable_raw_mode().context("raw mode")?;
            ACTIVE.store(true, Ordering::SeqCst);
            if SIGNALLED.load(Ordering::SeqCst) {
                return Err(Interrupted.into());
            }
            let mut err = std::io::stderr();
            execute!(
                err,
                terminal::EnterAlternateScreen,
                cursor::Hide,
                event::DisableFocusChange,
                event::EnableBracketedPaste
            )
            .context("terminal set-up")?;
            if SIGNALLED.load(Ordering::SeqCst) {
                return Err(Interrupted.into());
            }
            Terminal::new(CrosstermBackend::new(std::io::stderr())).context("terminal")
        };
        let term = match setup() {
            Ok(t) => t,
            Err(e) => return fail(&old_actions, e),
        };

        // 4. Panic restore, only for the thread that owns the dialog.
        *UI_THREAD.lock().unwrap() = Some(std::thread::current().id());
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let ui = *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner());
                if ui == Some(std::thread::current().id()) {
                    restore_terminal();
                }
                previous(info);
            }));
        });

        let tui = Tui {
            term,
            input,
            decoder: Decoder::new(),
            old_actions,
        };
        // 5. Drop bytes that are already readable.
        let mut buf = [0u8; 4096];
        while tui.readable(0) {
            if tui.read(&mut buf) == 0 {
                break;
            }
        }
        Ok(tui)
    }

    fn readable(&self, timeout_ms: i32) -> bool {
        let mut p = libc::pollfd {
            fd: self.input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        unsafe { libc::poll(&mut p, 1, timeout_ms) > 0 }
    }

    fn read(&self, buf: &mut [u8]) -> usize {
        // SAFETY: reads into a valid buffer from our own descriptor.
        let n = unsafe { libc::read(self.input.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        n.max(0) as usize
    }

    /// Wait up to 100 ms for keys.
    fn keys(&mut self) -> Result<Vec<Key>> {
        let mut out = self.decoder.tick(now_ms());
        if SIGNALLED.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        if !out.is_empty() {
            return Ok(out);
        }
        let timeout = match self.decoder.deadline() {
            Some(d) => (d - now_ms()).clamp(0, 100),
            None => 100,
        } as i32;
        if self.readable(timeout) {
            let mut buf = [0u8; 4096];
            let n = self.read(&mut buf);
            out.extend(self.decoder.feed(&buf[..n], now_ms()));
        } else {
            out.extend(self.decoder.tick(now_ms()));
        }
        if SIGNALLED.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        Ok(out)
    }

    /// Show a progress screen until `rx` yields (`None`: the user cancelled).
    pub fn loading<T>(&mut self, title: &str, text: &str, rx: &Receiver<T>) -> Result<Option<T>> {
        loop {
            self.term.draw(|f| render_message(f, title, text, None))?;
            match rx.recv_timeout(Duration::from_millis(0)) {
                Ok(v) => return Ok(Some(v)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("the resolution thread stopped unexpectedly"));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            for k in self.keys()? {
                if matches!(k, Key::Esc | Key::Ctrl('c')) {
                    return Ok(None);
                }
            }
        }
    }

    /// Run the compose phase until Send or Cancel.
    pub fn compose(&mut self, c: &mut Compose) -> Result<Outcome> {
        loop {
            self.term.draw(|f| render_compose(f, c))?;
            for k in self.keys()? {
                match c.handle(k) {
                    Outcome::None => {}
                    o => return Ok(o),
                }
            }
        }
    }

    /// Ask a yes/no question; the default is no.
    pub fn confirm(&mut self, title: &str, question: &str) -> Result<bool> {
        let text = format!("{question}\n\ny = yes · any other key = no");
        loop {
            self.term.draw(|f| render_message(f, title, &text, None))?;
            if let Some(k) = self.keys()?.into_iter().next() {
                return Ok(matches!(k, Key::Char('y' | 'Y')));
            }
        }
    }

    /// Show an outcome: success closes after 1.5 s or a key; an error stays
    /// until a key.
    pub fn message(&mut self, title: &str, text: &str, ok: bool) -> Result<()> {
        let until = Instant::now() + Duration::from_millis(1500);
        let hint = if ok { "closing…" } else { "press any key" };
        let body = format!("{text}\n\n{hint}");
        loop {
            self.term
                .draw(|f| render_message(f, title, &body, Some(ok)))?;
            if !self.keys()?.is_empty() || (ok && Instant::now() >= until) {
                return Ok(());
            }
        }
    }

    /// Draw one frame of a message, without waiting (used before a
    /// blocking step such as the send).
    pub fn show(&mut self, title: &str, text: &str) -> Result<()> {
        self.term.draw(|f| render_message(f, title, text, None))?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        restore_terminal();
        for (sig, old) in &self.old_actions {
            // SAFETY: restores the disposition saved in `enter`.
            unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
        }
        *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Lay the instruction out as display lines of at most `width` columns
/// (`> ` on the first line, two spaces after), breaking at newlines and at
/// the width, and return the lines with the caret's (line, column).
pub fn wrap_input(text: &[char], cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut lines = vec![String::from("> ")];
    let mut col = 2;
    let mut caret = (0, 2);
    for (i, &ch) in text.iter().enumerate() {
        if ch == '\n' {
            if i == cursor {
                caret = if col >= width {
                    (lines.len(), 2)
                } else {
                    (lines.len() - 1, col)
                };
            }
            lines.push(String::from("  "));
            col = 2;
            continue;
        }
        // Zero-width (combining) characters do not advance the column.
        let w = ratatui::text::Span::raw(ch.to_string()).width();
        // Wrap first, so a caret on this character lands where it is drawn.
        if col + w > width {
            lines.push(String::from("  "));
            col = 2;
        }
        if i == cursor {
            caret = (lines.len() - 1, col);
        }
        lines.last_mut().unwrap().push(ch);
        col += w;
    }
    if cursor >= text.len() {
        if col >= width {
            lines.push(String::from("  "));
            col = 2;
        }
        caret = (lines.len() - 1, col);
    }
    (lines, caret)
}

/// The dialog box: centred, at most 100 columns wide.
fn dialog_area(area: Rect, height: u16) -> Rect {
    let w = area
        .width
        .saturating_sub(4)
        .min(100)
        .max(area.width.min(20));
    let h = height
        .min(area.height.saturating_sub(2))
        .max(area.height.min(5));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn frame_block(title: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {title} "),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// Render a titled message (loading, confirm, result).
pub fn render_message(f: &mut Frame, title: &str, text: &str, ok: Option<bool>) {
    let lines = text.lines().count() as u16;
    let area = dialog_area(f.area(), lines + 4);
    f.render_widget(Clear, area);
    let style = match ok {
        Some(true) => Style::default().fg(Color::Green),
        Some(false) => Style::default().fg(Color::Red),
        None => Style::default(),
    };
    let p = Paragraph::new(text.to_string())
        .style(style)
        .wrap(Wrap { trim: false })
        .block(frame_block(title));
    f.render_widget(p, area);
}

/// Render the compose phase.
pub fn render_compose(f: &mut Frame, c: &Compose) {
    let details = c.rows[c.selected].details.len() as u16;
    let text_lines = (c.text.iter().filter(|&&ch| ch == '\n').count() + 1).min(6) as u16;
    let want = 2 + 3 + c.rows.len() as u16 + 1 + details + 1 + 1 + text_lines + 2 + 2;
    let area = dialog_area(f.area(), want);
    f.render_widget(Clear, area);
    let block = frame_block(&c.title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let list_h = (c.rows.len() as u16).min(inner.height.saturating_sub(8).max(1));
    let [head, list, det, label, input, notice, help] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(list_h + 1),
        Constraint::Length(details),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let dim = Style::default().fg(Color::DarkGray);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(c.subtitle.clone()),
            Line::from(Span::styled(c.verdict.clone(), dim)),
        ]),
        head,
    );

    // Keep the selection visible when the list is taller than the area.
    let visible = list_h as usize;
    let first = c.selected.saturating_sub(visible.saturating_sub(1));
    let rows: Vec<Line> = c
        .rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, r)| {
            if i == c.selected {
                Line::from(Span::styled(
                    format!("▶ {}", r.line),
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                ))
            } else {
                Line::from(format!("  {}", r.line))
            }
        })
        .collect();
    f.render_widget(Paragraph::new(rows), list);
    let det_lines: Vec<Line> = c.rows[c.selected]
        .details
        .iter()
        .map(|d| Line::from(Span::styled(format!("    {d}"), dim)))
        .collect();
    f.render_widget(Paragraph::new(det_lines), det);

    let label_text = if c.needs_text {
        "Instruction"
    } else {
        "Enter opens the editor"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(label_text, dim))),
        label,
    );
    if c.needs_text {
        let (lines, caret) = wrap_input(&c.text, c.cursor, input.width.max(3) as usize);
        let h = input.height.max(1) as usize;
        let first = caret.0.saturating_sub(h - 1);
        let shown: Vec<Line> = lines
            .iter()
            .skip(first)
            .take(h)
            .map(|l| Line::from(l.clone()))
            .collect();
        f.render_widget(Paragraph::new(shown), input);
        if !c.fixed_text {
            f.set_cursor_position((input.x + caret.1 as u16, input.y + (caret.0 - first) as u16));
        }
    }
    if let Some(n) = &c.notice {
        f.render_widget(
            Paragraph::new(Span::styled(n.clone(), Style::default().fg(Color::Yellow))),
            notice,
        );
    }
    let help_text = if c.needs_text {
        "↑↓ choose · Enter send · Alt+Enter newline · Esc cancel"
    } else {
        "↑↓ choose · Enter continue · Esc cancel"
    };
    f.render_widget(Paragraph::new(Span::styled(help_text, dim)), help);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::Row;
    use ratatui::backend::TestBackend;

    fn sample() -> Compose {
        let rows = vec![
            Row {
                line: "session-a  idle  58  ~/worktrees/project-ready-base · M mentions 62 of 62 PR refs".into(),
                details: vec!["title: Release planning and PR status".into()],
            },
            Row { line: "session-b  busy   0  no evidence".into(), details: vec![] },
        ];
        let mut c = Compose::new(
            "Send to Claude — upstream/project#4321".into(),
            "Add streaming support to the parser".into(),
            "not decisive — choose".into(),
            rows,
        );
        c.text = "Address the review".chars().collect();
        c.cursor = c.text.len();
        c
    }

    fn screen(w: u16, h: u16, c: &Compose) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render_compose(f, c)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn compose_renders_at_80x24_and_50x15() {
        let c = sample();
        let big = screen(80, 24, &c);
        assert!(big.contains("Send to Claude"));
        assert!(big.contains("▶ session-a"));
        assert!(big.contains("session-b"));
        assert!(big.contains("> Address the review"));
        assert!(big.contains("Enter send"));
        let small = screen(50, 15, &c);
        assert!(small.contains("session-a"), "{small}");
    }

    #[test]
    fn long_input_wraps_and_keeps_the_caret_visible() {
        let text: Vec<char> = "abcdefghij".chars().collect();
        let (lines, caret) = wrap_input(&text, text.len(), 6);
        assert_eq!(lines, vec!["> abcd", "  efgh", "  ij"]);
        assert_eq!(caret, (2, 4));
        let (_, caret) = wrap_input(&text, 0, 6);
        assert_eq!(caret, (0, 2));
        // At every wrap boundary the caret is on the line where the
        // character is drawn, never past the width.
        for cur in 0..=text.len() {
            let (lines, (row, col)) = wrap_input(&text, cur, 6);
            assert!(col < 6, "cursor {cur}: column {col}");
            assert!(row < lines.len());
        }
        assert_eq!(wrap_input(&text, 4, 6).1, (1, 2));
        let nl: Vec<char> = "abcd\nx".chars().collect();
        assert_eq!(wrap_input(&nl, 4, 6).1, (1, 2), "newline after a full row");
        let many: Vec<char> = std::iter::repeat_n('\n', 70_000)
            .chain("x".chars())
            .collect();
        let mut big = sample();
        big.text = many;
        big.cursor = big.text.len();
        let _ = screen(80, 24, &big);
        let multi: Vec<char> = "a\nb".chars().collect();
        assert_eq!(wrap_input(&multi, 3, 20).0, vec!["> a", "  b"]);
        // A long instruction still shows the caret line at the bottom.
        let mut c = sample();
        c.text = "word ".repeat(200).chars().collect();
        c.cursor = c.text.len();
        let s = screen(80, 24, &c);
        assert!(s.contains("word"), "{s}");
    }

    #[test]
    fn messages_render() {
        let mut t = Terminal::new(TestBackend::new(60, 12)).unwrap();
        t.draw(|f| render_message(f, "Send to Claude", "sent to session-a", Some(true)))
            .unwrap();
        let buf = t.backend().buffer().clone();
        let all: String = (0..12)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .map(|p| buf[p].symbol().to_string())
            .collect();
        assert!(all.contains("sent to session-a"));
    }
}
