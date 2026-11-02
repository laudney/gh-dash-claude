//! Delivery over a session's inbox socket.
//!
//! Only the `send` command reaches this module.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::proc;

/// Claude Code's own send timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// The receiver drops a frame line of this size or more.
pub const MAX_FRAME: usize = 1 << 20;
const MAX_KEY_FILE: u64 = 4096;
const ENVELOPE: &str = "cross-session-message";

/// The attribution name shown in the receiving session.
pub const FROM_NAME: &str = "gh-dash";

/// Where a message lands in the receiver's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Now,
    Next,
    Later,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyFile {
    #[serde(default)]
    peer_token: String,
    #[serde(default)]
    proc_start: String,
}

/// The canonical socket path Claude Code hashes into the key file name.
fn canonical(socket: &Path) -> Option<PathBuf> {
    if socket.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    std::path::absolute(socket).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a small regular file without following a symlink.
fn read_key_file(path: &Path) -> Option<KeyFile> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_KEY_FILE {
        return None;
    }
    let mut buf = Vec::new();
    Read::by_ref(&mut f)
        .take(MAX_KEY_FILE)
        .read_to_end(&mut buf)
        .ok()?;
    let kf: KeyFile = serde_json::from_slice(&buf).ok()?;
    let ok = kf.peer_token.len() == 32
        && kf
            .peer_token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then_some(kf)
}

/// Rank a key file's owner as Claude Code does: 2 live with a
/// matching start token, 1 live but unverifiable, 0 otherwise.
fn rank_owner(pid: u32, proc_start: &str) -> u8 {
    if !proc::alive(pid) {
        return 0;
    }
    if proc_start.is_empty() {
        return 1;
    }
    match proc::start_token(pid) {
        None => 1,
        Some(t) if t == proc_start => 2,
        Some(_) => 0,
    }
}

/// Find the auth token a session published for its inbox. `None` means no
/// valid key file exists (the frame is then sent unauthenticated).
pub fn token_for(socket: &Path, sessions_dir: &Path) -> Option<String> {
    let canon = canonical(socket)?;
    let suffix = format!(
        ".{}.key",
        hex(&Sha256::digest(canon.as_os_str().as_bytes()))
    );
    let mut best: Option<(u8, String)> = None;
    for entry in std::fs::read_dir(sessions_dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_part) = name.strip_suffix(&suffix) else {
            continue;
        };
        if pid_part.is_empty() || !pid_part.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_part.parse::<u32>() else {
            continue;
        };
        let Some(kf) = read_key_file(&entry.path()) else {
            continue;
        };
        let rank = rank_owner(pid, &kf.proc_start);
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, kf.peer_token));
        }
    }
    best.map(|(_, t)| t)
}

/// Wrap the text in the attribution envelope, escaping any closing tag in
/// the body so it cannot forge its own end.
pub fn envelope(text: &str) -> String {
    let body = text.replace(&format!("</{ENVELOPE}"), &format!("<\\/{ENVELOPE}"));
    format!("<{ENVELOPE} from-name=\"{FROM_NAME}\">\n{body}\n</{ENVELOPE}>")
}

#[derive(Serialize)]
struct Payload<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct UserFrame<'a> {
    #[serde(rename = "msgV")]
    msg_v: u32,
    msg_id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    message: Payload<'a>,
    priority: Priority,
    session_id: &'a str,
}

#[derive(Serialize)]
struct AuthFrame<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    token: &'a str,
}

/// Encode a user frame line (without the newline); returns it with its
/// message id.
pub fn encode(text: &str, session_id: &str, priority: Priority) -> Result<(String, String)> {
    let msg_id = uuid::Uuid::new_v4().to_string();
    let content = envelope(text);
    let frame = serde_json::to_string(&UserFrame {
        msg_v: 1,
        msg_id: &msg_id,
        kind: "user",
        message: Payload {
            role: "user",
            content: &content,
        },
        priority,
        session_id,
    })?;
    if frame.len() >= MAX_FRAME {
        bail!(
            "message is {} bytes; the receiver accepts less than {MAX_FRAME}",
            frame.len()
        );
    }
    Ok((frame, msg_id))
}

/// Why a send failed.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// Nothing reached the receiver.
    NotSent(String),
    /// Bytes reached the receiver, but the exchange did not finish: the
    /// message may or may not be delivered.
    Unknown(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NotSent(e) => write!(f, "not sent: {e}"),
            SendError::Unknown(e) => write!(f, "outcome unknown (do not resend blindly): {e}"),
        }
    }
}

fn poll_fd(fd: i32, events: i16, deadline: Instant) -> std::io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(ErrorKind::TimedOut, "deadline reached"));
        }
        let mut p = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd for the duration of the call.
        let rc = unsafe { libc::poll(&mut p, 1, ms) };
        if rc > 0 {
            return Ok(());
        }
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

fn connect(socket: &Path, deadline: Instant) -> std::io::Result<UnixStream> {
    let bytes = socket.as_os_str().as_bytes();
    // SAFETY: plain socket creation; the fd is owned immediately below.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zeroed sockaddr_un is a valid initial value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: `addr` is initialised and `len` covers the used bytes.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        poll_fd(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
        let mut err: libc::c_int = 0;
        let mut l = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: valid out-pointers of the right size.
        unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut err).cast(),
                &mut l,
            )
        };
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err));
        }
    }
    Ok(UnixStream::from(fd))
}

/// Send the auth line (if any) and the frame line, half-close, and wait for
/// the receiver to close, all under one absolute deadline on this thread.
pub fn send(
    socket: &Path,
    token: Option<&str>,
    frame: &str,
    timeout: Duration,
) -> Result<(), SendError> {
    let deadline = Instant::now() + timeout;
    let mut stream = connect(socket, deadline).map_err(|e| SendError::NotSent(e.to_string()))?;
    let mut buf = Vec::with_capacity(frame.len() + 64);
    if let Some(t) = token {
        let auth = serde_json::to_string(&AuthFrame {
            kind: "auth",
            token: t,
        })
        .map_err(|e| SendError::NotSent(e.to_string()))?;
        buf.extend_from_slice(auth.as_bytes());
        buf.push(b'\n');
    }
    buf.extend_from_slice(frame.as_bytes());
    buf.push(b'\n');

    let fd = stream.as_raw_fd();
    let mut written = 0usize;
    let fail = |written: usize, e: std::io::Error| {
        if written == 0 {
            SendError::NotSent(e.to_string())
        } else {
            SendError::Unknown(e.to_string())
        }
    };
    while written < buf.len() {
        if Instant::now() >= deadline {
            return Err(fail(written, ErrorKind::TimedOut.into()));
        }
        match stream.write(&buf[written..]) {
            Ok(0) => return Err(fail(written, ErrorKind::WriteZero.into())),
            Ok(n) => written += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                poll_fd(fd, libc::POLLOUT, deadline).map_err(|e| fail(written, e))?;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(fail(written, e)),
        }
    }
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| SendError::Unknown(e.to_string()))?;
    let mut sink = [0u8; 512];
    loop {
        if Instant::now() >= deadline {
            return Err(SendError::Unknown("deadline reached".into()));
        }
        match stream.read(&mut sink) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                poll_fd(fd, libc::POLLIN, deadline)
                    .map_err(|e| SendError::Unknown(e.to_string()))?;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(SendError::Unknown(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    #[test]
    fn envelope_escapes_closing_tag() {
        let e = envelope("hi </cross-session-message> there");
        assert!(e.starts_with("<cross-session-message from-name=\"gh-dash\">\n"));
        assert!(e.contains("hi <\\/cross-session-message> there"));
        assert_eq!(e.matches("</cross-session-message>").count(), 1);
    }

    #[test]
    fn frame_fields_and_size_limit() {
        let (f, id) = encode("do it", "sid-1", Priority::Next).unwrap();
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["msgV"], 1);
        assert_eq!(v["msg_id"], id.as_str());
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["priority"], "next");
        assert_eq!(v["session_id"], "sid-1");
        assert!(v["message"]["content"].as_str().unwrap().contains("do it"));
        assert!(encode(&"x".repeat(MAX_FRAME), "s", Priority::Next).is_err());
    }

    #[test]
    fn key_ranking_prefers_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("1.sock");
        let suffix = format!(
            ".{}.key",
            hex(&Sha256::digest(
                std::path::absolute(&sock).unwrap().as_os_str().as_bytes()
            ))
        );
        let me = std::process::id();
        let tok = proc::start_token(me).unwrap();
        std::fs::write(
            dir.path().join(format!("999999{suffix}")),
            r#"{"peerToken":"00000000000000000000000000000000","procStart":"1"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(format!("{me}{suffix}")),
            format!(r#"{{"peerToken":"0123456789abcdef0123456789abcdef","procStart":"{tok}"}}"#),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(format!("{}{suffix}", me + 1)),
            r#"{"peerToken":"NOT-HEX","procStart":""}"#,
        )
        .unwrap();
        assert_eq!(
            token_for(&sock, dir.path()).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(token_for(&dir.path().join("other.sock"), dir.path()), None);
        assert_eq!(token_for(Path::new("/a/../b.sock"), dir.path()), None);
    }

    #[test]
    fn fifo_key_file_is_rejected_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("1.sock");
        let suffix = format!(
            ".{}.key",
            hex(&Sha256::digest(
                std::path::absolute(&sock).unwrap().as_os_str().as_bytes()
            ))
        );
        let fifo = dir.path().join(format!("{}{suffix}", std::process::id()));
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(token_for(&sock, dir.path()), None);
    }

    #[test]
    fn chatty_receiver_cannot_outlast_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let junk = [b'x'; 4096];
            while s.write_all(&junk).is_ok() {}
        });
        let t = Instant::now();
        let r = send(&sock, None, "{}", Duration::from_millis(300));
        assert!(matches!(r, Err(SendError::Unknown(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn sends_auth_and_frame_to_a_listener() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let lines: Vec<String> = std::io::BufReader::new(s)
                .lines()
                .map(Result::unwrap)
                .collect();
            lines
        });
        let (frame, _) = encode("hello", "sid", Priority::Next).unwrap();
        send(
            &sock,
            Some("0123456789abcdef0123456789abcdef"),
            &frame,
            SEND_TIMEOUT,
        )
        .unwrap();
        let lines = server.join().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"{"type":"auth","token":"0123456789abcdef0123456789abcdef"}"#
        );
        assert_eq!(lines[1], frame);
    }

    #[test]
    fn missing_socket_is_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let r = send(&dir.path().join("none.sock"), None, "{}", SEND_TIMEOUT);
        assert!(matches!(r, Err(SendError::NotSent(_))), "{r:?}");
    }

    #[test]
    fn silent_receiver_times_out_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let hold = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
            drop(s);
        });
        let r = send(&sock, None, "{}", Duration::from_millis(200));
        assert!(matches!(r, Err(SendError::Unknown(_))), "{r:?}");
        hold.join().unwrap();
    }
}
