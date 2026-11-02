//! Process liveness through `/proc`, without touching any session socket.

/// The kernel start token Claude Code records as `procStart`: field 22 of
/// `/proc/<pid>/stat`, the start time in clock ticks since boot.
pub fn start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_token(&stat)
}

/// Extract the start token from the text of a `/proc/<pid>/stat` file. The
/// `comm` field is parenthesised and can hold spaces, so fields are counted
/// after its last `)`; `starttime` is then the 20th field.
pub fn parse_start_token(stat: &str) -> Option<String> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19).map(str::to_string)
}

/// Whether a process with this PID exists. `EPERM` means it exists but
/// belongs to someone else.
pub fn alive(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence and
    // permission check; it sends nothing.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Process identity check of a registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The PID exists and its start token matches the entry.
    Live,
    /// The PID does not exist.
    Dead,
    /// The PID exists but belongs to a different process.
    Recycled,
}

/// Classify a (pid, procStart) pair. An empty `proc_start` can never be
/// verified, so it is never `Live`.
pub fn liveness(pid: u32, proc_start: &str) -> Liveness {
    if !alive(pid) {
        return Liveness::Dead;
    }
    match start_token(pid) {
        Some(tok) if !proc_start.is_empty() && tok == proc_start => Liveness::Live,
        _ => Liveness::Recycled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_token_skips_parenthesised_comm() {
        let stat = "1234 (a b) c) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 19 20";
        assert_eq!(parse_start_token(stat).as_deref(), Some("777"));
    }

    #[test]
    fn own_process_is_live() {
        let me = std::process::id();
        let tok = start_token(me).expect("own stat");
        assert_eq!(liveness(me, &tok), Liveness::Live);
        assert_eq!(liveness(me, "0"), Liveness::Recycled);
        assert_eq!(liveness(me, ""), Liveness::Recycled);
    }

    #[test]
    fn low_pids_are_never_alive() {
        assert!(!alive(0));
        assert!(!alive(1));
    }
}
