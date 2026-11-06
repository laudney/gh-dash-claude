//! The non-interactive delivery core: re-check the chosen session, encode
//! the frame, send it. Nothing here prints or asks; callers decide what a
//! blocked or failed delivery means.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::proc::Liveness;
use crate::registry::{self, Session};
use crate::transport::{self, Priority, SendError};

/// Why a chosen session cannot be sent to now.
#[derive(Debug)]
pub enum Blocked {
    /// Not registered, not live, restarted, or moved its inbox. The text
    /// is the full user-facing reason.
    Gone(String),
    /// Same process and inbox, but `/clear` started a new conversation.
    Cleared { now: Box<Session> },
}

/// Re-read the registry entry of `chosen`. `Ok(current entry)` when it is
/// the same live process with the same socket and the same session id.
pub fn recheck(sessions_dir: &Path, chosen: &Session) -> Result<Session, Blocked> {
    let Some(now) = registry::load_one(sessions_dir, chosen.pid) else {
        return Err(Blocked::Gone(format!(
            "target changed: {} (pid {}) is no longer registered; nothing sent",
            chosen.name, chosen.pid
        )));
    };
    if now.liveness() != Liveness::Live
        || now.proc_start != chosen.proc_start
        || now.socket != chosen.socket
    {
        return Err(Blocked::Gone(format!(
            "target changed: {} (pid {}) restarted or moved its inbox; nothing sent",
            chosen.name, chosen.pid
        )));
    }
    if now.session_id != chosen.session_id {
        return Err(Blocked::Cleared { now: Box::new(now) });
    }
    Ok(now)
}

/// A frame ready to go.
#[derive(Debug)]
pub struct Outbound {
    pub socket: PathBuf,
    pub token: Option<String>,
    pub frame: String,
    pub msg_id: String,
}

/// Encode `body` for `session_id` on `now`'s inbox socket, with the auth
/// token the session published (if any).
pub fn encode(
    now: &Session,
    session_id: &str,
    body: &str,
    priority: Priority,
    sessions_dir: &Path,
) -> Result<Outbound> {
    let socket = now
        .socket
        .clone()
        .context("the session has no inbox socket")?;
    let (frame, msg_id) = transport::encode(body, session_id, priority)?;
    let token = transport::token_for(&socket, sessions_dir);
    Ok(Outbound {
        socket,
        token,
        frame,
        msg_id,
    })
}

/// Send under Claude Code's own timeout.
pub fn send(o: &Outbound) -> Result<(), SendError> {
    transport::send(
        &o.socket,
        o.token.as_deref(),
        &o.frame,
        transport::SEND_TIMEOUT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc;

    fn me(session_id: &str, socket: &str, proc_start: &str) -> Session {
        Session {
            pid: std::process::id(),
            session_id: session_id.into(),
            cwd: "/".into(),
            name: "me".into(),
            name_source: "derived".into(),
            kind: "interactive".into(),
            status: "idle".into(),
            proc_start: proc_start.into(),
            socket: Some(socket.into()),
            peer_protocol: 1,
        }
    }

    fn write_entry(dir: &Path, s: &Session) {
        let json = serde_json::json!({
            "sessionId": s.session_id,
            "cwd": "/",
            "name": s.name,
            "nameSource": s.name_source,
            "kind": s.kind,
            "status": s.status,
            "procStart": s.proc_start,
            "messagingSocketPath": s.socket.as_ref().unwrap(),
            "peerProtocol": s.peer_protocol,
        });
        std::fs::write(
            dir.join(format!("{}.json", s.pid)),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
    }

    fn start() -> String {
        proc::start_token(std::process::id()).unwrap()
    }

    #[test]
    fn same_session_passes() {
        let dir = tempfile::tempdir().unwrap();
        let s = me("session-a", "/run/u/a.sock", &start());
        write_entry(dir.path(), &s);
        let now = recheck(dir.path(), &s).unwrap();
        assert_eq!(now, s);
    }

    #[test]
    fn cleared_session_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = me("session-a", "/run/u/a.sock", &start());
        write_entry(dir.path(), &me("session-b", "/run/u/a.sock", &start()));
        match recheck(dir.path(), &s) {
            Err(Blocked::Cleared { now }) => assert_eq!(now.session_id, "session-b"),
            other => panic!("expected Cleared, got {other:?}"),
        }
    }

    #[test]
    fn moved_socket_restart_or_missing_entry_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let s = me("session-a", "/run/u/a.sock", &start());
        write_entry(dir.path(), &me("session-a", "/run/u/b.sock", &start()));
        assert!(matches!(recheck(dir.path(), &s), Err(Blocked::Gone(m)) if m.contains("moved")));
        write_entry(dir.path(), &me("session-a", "/run/u/a.sock", "1"));
        assert!(matches!(recheck(dir.path(), &s), Err(Blocked::Gone(_))));
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            recheck(empty.path(), &s),
            Err(Blocked::Gone(m)) if m.contains("no longer registered")
        ));
    }

    #[test]
    fn encode_requires_a_socket() {
        let mut s = me("session-a", "/run/u/a.sock", &start());
        s.socket = None;
        let dir = tempfile::tempdir().unwrap();
        assert!(encode(&s, "session-a", "hi", Priority::Next, dir.path()).is_err());
    }
}
