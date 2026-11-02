//! The dialog's pure state: no terminal access here.

use crate::keydec::Key;
use crate::resolve::Decision;

/// Edited text is capped; `-m` text is not edited and has no cap here.
pub const MAX_TEXT: usize = 64 * 1024;

/// One candidate row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: String,
    pub details: Vec<String>,
}

/// The compose phase.
#[derive(Debug, Clone)]
pub struct Compose {
    pub title: String,
    pub subtitle: String,
    pub verdict: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub text: Vec<char>,
    pub cursor: usize,
    /// The instruction came from `-m` and cannot be edited.
    pub fixed_text: bool,
    /// Enter needs a non-empty instruction (false with `--editor`).
    pub needs_text: bool,
    pub notice: Option<String>,
}

/// What a key asks the dialog to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    None,
    Send,
    Cancel,
}

impl Compose {
    /// A compose state; `rows` must not be empty.
    pub fn new(title: String, subtitle: String, verdict: String, rows: Vec<Row>) -> Self {
        assert!(!rows.is_empty(), "Compose needs at least one row");
        Compose {
            title,
            subtitle,
            verdict,
            rows,
            selected: 0,
            text: Vec::new(),
            cursor: 0,
            fixed_text: false,
            needs_text: true,
            notice: None,
        }
    }

    /// The instruction, trimmed.
    pub fn instruction(&self) -> String {
        self.text.iter().collect::<String>().trim().to_string()
    }

    fn insert(&mut self, s: &str) {
        if self.fixed_text {
            self.notice = Some("instruction given with -m".into());
            return;
        }
        let add: Vec<char> = s.chars().collect();
        if self.text.len() + add.len() > MAX_TEXT {
            self.notice = Some("instruction too long".into());
            return;
        }
        let n = add.len();
        self.text.splice(self.cursor..self.cursor, add);
        self.cursor += n;
        self.notice = None;
    }

    fn edit(&mut self, f: impl FnOnce(&mut Self)) {
        if self.fixed_text {
            self.notice = Some("instruction given with -m".into());
        } else {
            f(self);
            self.notice = None;
        }
    }

    /// Apply one key.
    pub fn handle(&mut self, key: Key) -> Outcome {
        match key {
            Key::Up | Key::Ctrl('p') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Ctrl('n') => {
                self.selected = (self.selected + 1).min(self.rows.len() - 1)
            }
            Key::Esc | Key::Ctrl('c') => return Outcome::Cancel,
            Key::Enter => {
                if self.needs_text && self.instruction().is_empty() {
                    self.notice = Some("type an instruction first".into());
                } else {
                    return Outcome::Send;
                }
            }
            Key::Char(c) => self.insert(&c.to_string()),
            Key::Newline => self.insert("\n"),
            Key::Paste(s) => self.insert(&s),
            Key::PasteRejected => self.notice = Some("paste too large; ignored".into()),
            Key::Backspace => self.edit(|s| {
                if s.cursor > 0 {
                    s.cursor -= 1;
                    s.text.remove(s.cursor);
                }
            }),
            Key::Delete => self.edit(|s| {
                if s.cursor < s.text.len() {
                    s.text.remove(s.cursor);
                }
            }),
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.text.len(),
            Key::Ctrl('u') => self.edit(|s| {
                s.text.clear();
                s.cursor = 0;
            }),
            Key::Ctrl('w') => self.edit(|s| {
                let mut i = s.cursor;
                while i > 0 && s.text[i - 1].is_whitespace() {
                    i -= 1;
                }
                while i > 0 && !s.text[i - 1].is_whitespace() {
                    i -= 1;
                }
                s.text.drain(i..s.cursor);
                s.cursor = i;
            }),
            _ => {}
        }
        Outcome::None
    }
}

/// Whether sending to `sent_row` counts as the user's choice, so that the
/// route is recorded.
pub fn picked(decision: Decision, sent_row: usize, force_pick: bool, to: bool) -> bool {
    match decision {
        Decision::Auto(owner) => force_pick || to || owner != sent_row,
        Decision::Pick { .. } | Decision::NoCandidates => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rows: usize) -> Compose {
        let rows = (0..rows)
            .map(|i| Row {
                line: format!("row {i}"),
                details: vec![],
            })
            .collect();
        Compose::new("t".into(), "s".into(), "v".into(), rows)
    }

    fn typed(c: &mut Compose, s: &str) {
        for ch in s.chars() {
            c.handle(Key::Char(ch));
        }
    }

    #[test]
    fn default_highlight_and_bounds() {
        let mut d = c(3);
        assert_eq!(d.selected, 0);
        d.handle(Key::Up);
        assert_eq!(d.selected, 0);
        d.handle(Key::Down);
        d.handle(Key::Down);
        d.handle(Key::Down);
        assert_eq!(d.selected, 2);
        d.handle(Key::Ctrl('p'));
        assert_eq!(d.selected, 1);
    }

    #[test]
    fn enter_needs_text_then_sends_to_the_highlight() {
        let mut d = c(2);
        assert_eq!(d.handle(Key::Enter), Outcome::None);
        assert_eq!(d.notice.as_deref(), Some("type an instruction first"));
        typed(&mut d, "  fix it ");
        assert_eq!(d.notice, None);
        assert_eq!(d.handle(Key::Enter), Outcome::Send);
        assert_eq!(d.instruction(), "fix it");
        assert_eq!(d.selected, 0);
        let mut e = c(2);
        e.needs_text = false;
        assert_eq!(e.handle(Key::Enter), Outcome::Send, "--editor: Enter picks");
    }

    #[test]
    fn editing_keys() {
        let mut d = c(1);
        typed(&mut d, "hello world");
        d.handle(Key::Ctrl('w'));
        assert_eq!(d.instruction(), "hello");
        d.handle(Key::Home);
        d.handle(Key::Delete);
        d.handle(Key::End);
        d.handle(Key::Backspace);
        assert_eq!(
            d.instruction(),
            "ello",
            "Backspace removed the space left by Ctrl+W"
        );
        d.handle(Key::Newline);
        d.handle(Key::Paste("a\nb".into()));
        assert_eq!(d.text.iter().collect::<String>(), "ello\na\nb");
        d.handle(Key::Ctrl('u'));
        assert!(d.text.is_empty());
        assert_eq!(d.handle(Key::Esc), Outcome::Cancel);
        assert_eq!(d.handle(Key::Ctrl('c')), Outcome::Cancel);
    }

    #[test]
    fn fixed_text_and_limits() {
        let mut d = c(1);
        d.text = "from -m".chars().collect();
        d.cursor = d.text.len();
        d.fixed_text = true;
        d.handle(Key::Char('x'));
        d.handle(Key::Ctrl('u'));
        assert_eq!(d.instruction(), "from -m");
        assert_eq!(d.notice.as_deref(), Some("instruction given with -m"));
        assert_eq!(d.handle(Key::Enter), Outcome::Send);

        let mut e = c(1);
        typed(&mut e, "keep");
        e.handle(Key::Paste("x".repeat(MAX_TEXT)));
        assert_eq!(e.instruction(), "keep");
        assert_eq!(e.notice.as_deref(), Some("instruction too long"));
        e.handle(Key::PasteRejected);
        assert_eq!(e.notice.as_deref(), Some("paste too large; ignored"));
    }

    #[test]
    fn route_rule() {
        let auto = Decision::Auto(0);
        assert!(!picked(auto, 0, false, false), "kept the auto owner");
        assert!(picked(auto, 1, false, false), "changed the row");
        assert!(picked(auto, 0, true, false), "--pick");
        assert!(picked(auto, 0, false, true), "--to");
        assert!(picked(Decision::Pick { default: 0 }, 0, false, false));
    }
}
