//! The Claude Code session registry: `<config>/sessions/<pid>.json`.
//!
//! Reading it never touches a session socket.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::proc::{self, Liveness};

/// Registry files above this size are not read (Claude Code's own cap).
const MAX_REGISTRY_BYTES: u64 = 256 * 1024;

/// The only peer protocol version this tool speaks.
pub const PEER_PROTOCOL: u32 = 1;

/// One registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: String,
    pub name_source: String,
    pub kind: String,
    pub status: String,
    pub proc_start: String,
    pub socket: Option<PathBuf>,
    pub peer_protocol: u32,
}

/// Whether a live session can receive a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Eligible,
    NoSocket,
    Protocol(u32),
}

impl Session {
    /// Process identity check, through `/proc` only.
    pub fn liveness(&self) -> Liveness {
        proc::liveness(self.pid, &self.proc_start)
    }

    /// Whether this entry has an inbox in a protocol version we speak.
    pub fn eligibility(&self) -> Eligibility {
        if self.socket.is_none() {
            Eligibility::NoSocket
        } else if self.peer_protocol != PEER_PROTOCOL {
            Eligibility::Protocol(self.peer_protocol)
        } else {
            Eligibility::Eligible
        }
    }

    /// The name explicitly given by the user, if any (not derived from the
    /// working directory).
    pub fn explicit_name(&self) -> Option<&str> {
        (!self.name.is_empty() && self.name_source != "derived" && self.name_source != "collision")
            .then_some(self.name.as_str())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    name_source: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    proc_start: String,
    #[serde(default)]
    messaging_socket_path: String,
    #[serde(default)]
    peer_protocol: u32,
}

/// Parse one registry file. The PID comes from the file name.
pub fn parse_entry(pid: u32, data: &[u8]) -> Option<Session> {
    let raw: RawEntry = serde_json::from_slice(data).ok()?;
    if raw.session_id.is_empty() {
        return None;
    }
    Some(Session {
        pid,
        session_id: raw.session_id,
        cwd: PathBuf::from(raw.cwd),
        name: raw.name,
        name_source: raw.name_source,
        kind: raw.kind,
        status: raw.status,
        proc_start: raw.proc_start,
        socket: (!raw.messaging_socket_path.is_empty())
            .then(|| PathBuf::from(raw.messaging_socket_path)),
        peer_protocol: raw.peer_protocol,
    })
}

/// The PID encoded in a registry file name `<pid>.json`.
fn pid_of(file_name: &str) -> Option<u32> {
    let digits = file_name.strip_suffix(".json")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// Read one entry by PID.
pub fn load_one(dir: &Path, pid: u32) -> Option<Session> {
    let path = dir.join(format!("{pid}.json"));
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_REGISTRY_BYTES {
        return None;
    }
    parse_entry(pid, &std::fs::read(&path).ok()?)
}

/// Read every parsable registry entry, sorted by PID. A missing directory
/// gives no entries.
pub fn load_all(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Session> = entries
        .flatten()
        .filter_map(|e| pid_of(e.file_name().to_str()?))
        .filter_map(|pid| load_one(dir, pid))
        .collect();
    out.sort_by_key(|s| s.pid);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY: &str = r#"{"pid":4242,"sessionId":"11111111-2222-4333-8444-555555555555",
        "cwd":"/home/u/src/project","procStart":"1234567","version":"2.1.0",
        "peerProtocol":1,"kind":"interactive",
        "messagingSocketPath":"/run/user/1001/cc-socks/4242.sock",
        "name":"session-a","nameSource":"derived","status":"shell"}"#;

    #[test]
    fn parses_real_entry() {
        let s = parse_entry(4242, ENTRY.as_bytes()).unwrap();
        assert_eq!(s.session_id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(
            s.socket.as_deref(),
            Some(Path::new("/run/user/1001/cc-socks/4242.sock"))
        );
        assert_eq!(s.eligibility(), Eligibility::Eligible);
        assert_eq!(s.explicit_name(), None);
    }

    #[test]
    fn eligibility_rules() {
        let mut s = parse_entry(5, ENTRY.as_bytes()).unwrap();
        s.peer_protocol = 2;
        assert_eq!(s.eligibility(), Eligibility::Protocol(2));
        s.socket = None;
        assert_eq!(s.eligibility(), Eligibility::NoSocket);
    }

    #[test]
    fn file_names() {
        assert_eq!(pid_of("123.json"), Some(123));
        assert_eq!(pid_of("0123.json"), None);
        assert_eq!(pid_of("123.abc.key"), None);
        assert_eq!(pid_of(".json"), None);
    }

    #[test]
    fn load_all_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("10.json"), ENTRY).unwrap();
        std::fs::write(dir.path().join("11.json"), "{not json").unwrap();
        std::fs::write(dir.path().join("12.json"), r#"{"cwd":"/x"}"#).unwrap();
        std::fs::write(dir.path().join("x.json"), ENTRY).unwrap();
        let all = load_all(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pid, 10);
        assert!(load_all(&dir.path().join("missing")).is_empty());
    }
}
