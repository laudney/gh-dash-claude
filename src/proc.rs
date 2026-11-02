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

/// Output of a bounded subprocess run.
pub struct Bounded {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `cmd` to completion under one deadline, in its own process group.
///
/// The leader is waited for with `waitid(WNOWAIT)`, so it stays unreaped
/// (and its group id stays ours) until the output is collected or the group
/// is killed; output collection is bounded by the same deadline, so a
/// descendant that keeps a pipe open cannot block the caller. `None` means
/// the command could not run or did not finish in time.
pub fn run_bounded(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<Bounded> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;

    static SIGCHLD_DEFAULT: std::sync::Once = std::sync::Once::new();
    // An inherited ignored SIGCHLD makes the kernel reap children at once.
    // SAFETY: restores the default disposition of one signal.
    SIGCHLD_DEFAULT.call_once(|| unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    });

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .ok()?;
    let pgid = child.id() as libc::pid_t;
    let pipe = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut r) = r {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
        }
        rx
    };
    let out_rx = pipe(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_rx = pipe(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let pid = child.id() as libc::id_t;
    let exited = loop {
        // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waits on our own child; WNOWAIT leaves it unreaped.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        // SAFETY: si_pid is valid to read after a successful waitid.
        if rc == 0 && unsafe { info.si_pid() } != 0 {
            break true;
        }
        if rc != 0 {
            // ECHILD: reaped behind our back; the group id may be reused,
            // so never signal it.
            return None;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let collect = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let (stdout, stderr) = if exited {
        (collect(&out_rx), collect(&err_rx))
    } else {
        (None, None)
    };
    if stdout.is_none() || stderr.is_none() {
        // SAFETY: the leader is unreaped, so this group id is still ours.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    let status = child.wait().ok()?;
    Some(Bounded {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
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
