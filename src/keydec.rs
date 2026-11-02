//! Terminal byte → key decoder for the dialog.
//!
//! The dialog reads raw bytes itself instead of using crossterm's event
//! reader, because terminal string replies (OSC/DCS, such as a late
//! background-colour report) must be swallowed whole, also when a read
//! splits them, and that must be testable.

/// A decoded key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// Ctrl+J or Alt+Enter: a newline in the instruction.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
    Esc,
    Ctrl(char),
    Paste(String),
    /// A paste larger than the cap was discarded.
    PasteRejected,
}

const ESC_WAIT_MS: i64 = 50;
const SEQ_WAIT_MS: i64 = 200;
const MAX_CSI: usize = 64;
const MAX_PASTE: usize = 1 << 20;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Ground,
    Utf8 { need: usize, buf: Vec<u8> },
    Esc,
    Csi { buf: Vec<u8> },
    Ss3,
    Str,
    StrEsc,
    Paste { buf: Vec<u8> },
    DiscardPaste { tail: Vec<u8> },
}

/// The decoder. Feed it bytes with their arrival time; call `tick` when
/// `deadline` passes.
#[derive(Debug)]
pub struct Decoder {
    state: State,
    last_ms: i64,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder {
            state: State::Ground,
            last_ms: 0,
        }
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `bytes` that arrived at `now_ms`.
    pub fn feed(&mut self, bytes: &[u8], now_ms: i64) -> Vec<Key> {
        let mut out = self.tick(now_ms);
        for &b in bytes {
            self.byte(b, &mut out);
        }
        self.last_ms = now_ms;
        out
    }

    /// Resolve timeouts: a lone ESC becomes Esc after 50 ms; an unfinished
    /// sequence is dropped after 200 ms. Pastes never time out.
    pub fn tick(&mut self, now_ms: i64) -> Vec<Key> {
        let idle = now_ms - self.last_ms;
        match self.state {
            State::Esc if idle >= ESC_WAIT_MS => {
                self.state = State::Ground;
                vec![Key::Esc]
            }
            State::Utf8 { .. } | State::Csi { .. } | State::Ss3 | State::Str | State::StrEsc
                if idle >= SEQ_WAIT_MS =>
            {
                self.state = State::Ground;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The time at which `tick` will have something to do.
    pub fn deadline(&self) -> Option<i64> {
        match self.state {
            State::Esc => Some(self.last_ms + ESC_WAIT_MS),
            State::Utf8 { .. } | State::Csi { .. } | State::Ss3 | State::Str | State::StrEsc => {
                Some(self.last_ms + SEQ_WAIT_MS)
            }
            _ => None,
        }
    }

    fn byte(&mut self, b: u8, out: &mut Vec<Key>) {
        match std::mem::replace(&mut self.state, State::Ground) {
            State::Ground => self.ground(b, out),
            State::Utf8 { need, mut buf } => {
                if b & 0xc0 != 0x80 {
                    // Broken sequence: drop it and read this byte afresh.
                    self.ground(b, out);
                    return;
                }
                buf.push(b);
                if buf.len() == need {
                    if let Some(c) = std::str::from_utf8(&buf)
                        .ok()
                        .and_then(|s| s.chars().next())
                    {
                        out.push(Key::Char(c));
                    }
                } else {
                    self.state = State::Utf8 { need, buf };
                }
            }
            State::Esc => match b {
                b'[' => self.state = State::Csi { buf: Vec::new() },
                b'O' => self.state = State::Ss3,
                b']' | b'P' | b'_' | b'^' | b'X' => self.state = State::Str,
                b'\r' => out.push(Key::Newline),
                0x1b => {
                    out.push(Key::Esc);
                    self.state = State::Esc;
                }
                // Alt+key is not used: drop the ESC, keep the key.
                _ => self.ground(b, out),
            },
            State::Csi { mut buf } => {
                if (0x40..=0x7e).contains(&b) {
                    self.csi(&buf, b, out);
                } else if (0x20..=0x3f).contains(&b) {
                    if buf.len() < MAX_CSI {
                        buf.push(b);
                        self.state = State::Csi { buf };
                    }
                    // An over-long CSI is dropped.
                } else {
                    // A byte that cannot belong to a CSI (such as `\r` or
                    // ESC): abandon the sequence and read the byte afresh,
                    // so a real Enter is never swallowed.
                    self.ground(b, out);
                }
            }
            State::Ss3 if b < 0x20 || b == 0x7f => self.ground(b, out),
            State::Ss3 => match b {
                b'A' => out.push(Key::Up),
                b'B' => out.push(Key::Down),
                b'C' => out.push(Key::Right),
                b'D' => out.push(Key::Left),
                b'H' => out.push(Key::Home),
                b'F' => out.push(Key::End),
                _ => {}
            },
            State::Str => match b {
                0x07 => {}
                0x1b => self.state = State::StrEsc,
                _ => self.state = State::Str,
            },
            State::StrEsc => {
                if b != b'\\' {
                    self.state = if b == 0x1b { State::StrEsc } else { State::Str };
                }
            }
            State::Paste { mut buf } => {
                buf.push(b);
                if buf.ends_with(PASTE_END) {
                    buf.truncate(buf.len() - PASTE_END.len());
                    let text = String::from_utf8_lossy(&buf)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                    out.push(Key::Paste(text));
                } else if buf.len() > MAX_PASTE {
                    let tail = buf[buf.len() - (PASTE_END.len() - 1)..].to_vec();
                    self.state = State::DiscardPaste { tail };
                } else {
                    self.state = State::Paste { buf };
                }
            }
            State::DiscardPaste { mut tail } => {
                tail.push(b);
                if tail.ends_with(PASTE_END) {
                    out.push(Key::PasteRejected);
                } else {
                    if tail.len() > PASTE_END.len() {
                        tail.drain(..tail.len() - PASTE_END.len());
                    }
                    self.state = State::DiscardPaste { tail };
                }
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<Key>) {
        match b {
            0x1b => self.state = State::Esc,
            b'\r' => out.push(Key::Enter),
            b'\n' => out.push(Key::Newline),
            0x7f | 0x08 => out.push(Key::Backspace),
            b'\t' => out.push(Key::Tab),
            0x07 | 0x00 | 0x1c..=0x1f => {}
            0x01..=0x1a => out.push(Key::Ctrl((b'a' + b - 1) as char)),
            0x20..=0x7e => out.push(Key::Char(b as char)),
            0xc2..=0xdf => {
                self.state = State::Utf8 {
                    need: 2,
                    buf: vec![b],
                }
            }
            0xe0..=0xef => {
                self.state = State::Utf8 {
                    need: 3,
                    buf: vec![b],
                }
            }
            0xf0..=0xf4 => {
                self.state = State::Utf8 {
                    need: 4,
                    buf: vec![b],
                }
            }
            _ => {}
        }
    }

    fn csi(&mut self, params: &[u8], fin: u8, out: &mut Vec<Key>) {
        let text = String::from_utf8_lossy(params);
        let nums: Vec<u32> = text
            .split(';')
            .map(|p| p.split(':').next().unwrap_or("").parse().unwrap_or(0))
            .collect();
        let first = nums.first().copied().unwrap_or(0);
        match fin {
            b'A' => out.push(Key::Up),
            b'B' => out.push(Key::Down),
            b'C' => out.push(Key::Right),
            b'D' => out.push(Key::Left),
            b'H' => out.push(Key::Home),
            b'F' => out.push(Key::End),
            b'~' => match first {
                1 | 7 => out.push(Key::Home),
                4 | 8 => out.push(Key::End),
                3 => out.push(Key::Delete),
                200 => self.state = State::Paste { buf: Vec::new() },
                _ => {}
            },
            b'u' => {
                // kitty keyboard protocol: CSI code[;mods] u
                let mods = nums.get(1).copied().unwrap_or(1).saturating_sub(1);
                let alt = mods & 0b10 != 0;
                let ctrl = mods & 0b100 != 0;
                let key = match first {
                    13 if alt => Some(Key::Newline),
                    13 => Some(Key::Enter),
                    27 => Some(Key::Esc),
                    127 => Some(Key::Backspace),
                    9 => Some(Key::Tab),
                    c if ctrl => char::from_u32(c)
                        .filter(char::is_ascii_alphabetic)
                        .map(|c| Key::Ctrl(c.to_ascii_lowercase())),
                    c => char::from_u32(c).filter(|c| !c.is_control()).map(Key::Char),
                };
                out.extend(key);
            }
            // Focus (`I`/`O`), cursor and device reports, mouse, …: ignored.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode in one go, then flush timeouts.
    fn whole(bytes: &[u8]) -> Vec<Key> {
        let mut d = Decoder::new();
        let mut k = d.feed(bytes, 1000);
        k.extend(d.tick(1300));
        k
    }

    /// Decode split at `i` (1 ms apart), then flush timeouts.
    fn split(bytes: &[u8], i: usize) -> Vec<Key> {
        let mut d = Decoder::new();
        let mut k = d.feed(&bytes[..i], 1000);
        k.extend(d.feed(&bytes[i..], 1001));
        k.extend(d.tick(1301));
        k
    }

    /// Assert the same keys for the whole input and for every split.
    fn every_split(bytes: &[u8], want: &[Key]) {
        assert_eq!(whole(bytes), want, "whole {bytes:?}");
        for i in 1..bytes.len() {
            assert_eq!(split(bytes, i), want, "split at {i} of {bytes:?}");
        }
    }

    fn text(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    #[test]
    fn replies_are_swallowed_at_every_split() {
        let replies: [&[u8]; 5] = [
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07",
            b"\x1bP1$r0 q\x1b\\",
            b"\x1b[I",
            b"\x1b[?62;22c",
        ];
        for r in replies {
            let mut bytes = b"ab".to_vec();
            bytes.extend_from_slice(r);
            bytes.extend_from_slice(b"cd");
            every_split(&bytes, &text("abcd"));
        }
    }

    #[test]
    fn keys_decode_at_every_split() {
        let cases: Vec<(&[u8], Vec<Key>)> = vec![
            (b"\x1b[A", vec![Key::Up]),
            (b"\x1b[B", vec![Key::Down]),
            (b"\x1bOA", vec![Key::Up]),
            (b"\x1b[H", vec![Key::Home]),
            (b"\x1b[4~", vec![Key::End]),
            (b"\x1b[3~", vec![Key::Delete]),
            (b"\x1b\r", vec![Key::Newline]),
            (b"\x1b[13u", vec![Key::Enter]),
            (b"\x1b[13;3u", vec![Key::Newline]),
            (b"\x1b[99;5u", vec![Key::Ctrl('c')]),
            (
                b"\x1b[200~hi\r\nthere\x1b[201~",
                vec![Key::Paste("hi\nthere".into())],
            ),
            ("é→".as_bytes(), text("é→")),
            (b"\r", vec![Key::Enter]),
            (b"\n", vec![Key::Newline]),
            (b"\x7f", vec![Key::Backspace]),
            (b"\x15", vec![Key::Ctrl('u')]),
        ];
        for (bytes, want) in cases {
            every_split(bytes, &want);
        }
    }

    #[test]
    fn incomplete_sequences_do_not_swallow_keys() {
        for prefix in [&b"\x1b["[..], b"\x1b[1;", b"\x1bO"] {
            for (key, want) in [
                (&b"\r"[..], Key::Enter),
                (b"\x03", Key::Ctrl('c')),
                (b"\x1b[A", Key::Up),
            ] {
                let mut bytes = prefix.to_vec();
                bytes.extend_from_slice(key);
                assert_eq!(whole(&bytes), vec![want.clone()], "{bytes:?}");
            }
        }
    }

    #[test]
    fn lone_escape_waits_50ms() {
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b", 1000).is_empty());
        assert_eq!(d.deadline(), Some(1050));
        assert!(d.tick(1049).is_empty());
        assert_eq!(d.tick(1050), vec![Key::Esc]);
        // An ESC followed quickly by `]` starts a reply, not Esc.
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b", 1000).is_empty());
        assert!(
            d.feed(b"]11;rgb:0/0/0\x07x", 1010)
                .contains(&Key::Char('x'))
        );
        assert!(!d.tick(2000).contains(&Key::Esc));
    }

    #[test]
    fn unterminated_reply_is_dropped_after_200ms() {
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b]11;rgb:", 1000).is_empty());
        assert_eq!(d.deadline(), Some(1200));
        assert!(d.tick(1200).is_empty());
        assert_eq!(d.feed(b"z", 1300), vec![Key::Char('z')]);
    }

    #[test]
    fn oversized_paste_is_rejected_whole() {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_PASTE + 10));
        bytes.extend_from_slice(b"\r\x03more\r");
        bytes.extend_from_slice(PASTE_END);
        bytes.push(b'z');
        assert_eq!(whole(&bytes), vec![Key::PasteRejected, Key::Char('z')]);
    }
}
