//! Remembered user choices.
//!
//! Only `send` writes this file; every read-only command just loads it.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::proc;
use crate::registry::Session;
use crate::resolve::RouteView;

/// One remembered choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub pid: u32,
    pub proc_start: String,
    pub session_id: String,
    pub name: String,
    /// Epoch milliseconds.
    pub at: i64,
}

/// What can be said about a route's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    Live,
    /// The process is gone or its PID belongs to another process.
    Dead,
    /// The process exists but its identity cannot be read now.
    Unknown,
}

impl Route {
    fn standing(&self) -> Standing {
        if self.proc_start.is_empty() || !proc::alive(self.pid) {
            return Standing::Dead;
        }
        match proc::start_token(self.pid) {
            Some(t) if t == self.proc_start => Standing::Live,
            Some(_) => Standing::Dead,
            None => Standing::Unknown,
        }
    }

    fn live(&self) -> bool {
        self.standing() == Standing::Live
    }
}

/// Read the store: `Ok(None)` when it does not exist.
fn read_file(path: &Path) -> Result<Option<File>> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    routes: BTreeMap<String, Route>,
}

/// The route store in a state directory.
pub struct Routes {
    dir: PathBuf,
    routes: BTreeMap<String, Route>,
    /// The store exists but could not be read or parsed.
    unreadable: bool,
}

impl Routes {
    /// Load the store. A missing file is empty; a corrupt one is empty and
    /// reported through the returned warning.
    pub fn load(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join("routes.json");
        let (routes, unreadable, warning) = match read_file(&path) {
            Ok(f) => (f.map(|f| f.routes).unwrap_or_default(), false, None),
            Err(e) => (
                BTreeMap::new(),
                true,
                Some(format!("ignoring {}: {e:#}", path.display())),
            ),
        };
        (
            Routes {
                dir: dir.to_path_buf(),
                routes,
                unreadable,
            },
            warning,
        )
    }

    /// The live routes for these target keys.
    pub fn live_view(&self, keys: &[String]) -> RouteView {
        let routes: Vec<&Route> = keys.iter().filter_map(|k| self.routes.get(k)).collect();
        let mut pids: Vec<u32> = routes.iter().filter(|r| r.live()).map(|r| r.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        // An unreadable store or an unverifiable route may hide a conflict.
        let uncertain = self.unreadable || routes.iter().any(|r| r.standing() == Standing::Unknown);
        RouteView { pids, uncertain }
    }

    /// Remember `session` for `key`, under an exclusive lock, with an
    /// atomic replace. Routes whose process is no longer live are dropped;
    /// routes to live processes are kept.
    pub fn record(&mut self, key: &str, session: &Session, now: i64) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.dir.join("routes.lock"))
            .context("opening the routes lock")?;
        // SAFETY: flock on an open descriptor; released when `lock` drops.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking routes");
        }
        // Re-load under the lock so that a concurrent writer is not lost.
        // A store that exists but cannot be read must not be replaced:
        // that would erase live routes.
        self.routes = read_file(&self.dir.join("routes.json"))
            .context("not updating routes")?
            .map(|f| f.routes)
            .unwrap_or_default();
        self.routes.insert(
            key.to_string(),
            Route {
                pid: session.pid,
                proc_start: session.proc_start.clone(),
                session_id: session.session_id.clone(),
                name: session.name.clone(),
                at: now,
            },
        );
        // Prune only routes whose process is certainly gone.
        self.routes.retain(|_, r| r.standing() != Standing::Dead);
        let file = File {
            version: 1,
            routes: self.routes.clone(),
        };
        let data = serde_json::to_vec_pretty(&file)?;
        let mut tmp = tempfile::Builder::new()
            .prefix("routes.json.")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .tempfile_in(&self.dir)
            .context("creating a temporary routes file")?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.dir.join("routes.json"))
            .map_err(|e| e.error)
            .context("replacing routes.json")?;
        drop(lock);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Session {
        Session {
            pid: std::process::id(),
            session_id: "s1".into(),
            cwd: "/".into(),
            name: "me".into(),
            name_source: "derived".into(),
            kind: "interactive".into(),
            status: "idle".into(),
            proc_start: proc::start_token(std::process::id()).unwrap(),
            socket: None,
            peer_protocol: 1,
        }
    }

    #[test]
    fn record_and_view() {
        let dir = tempfile::tempdir().unwrap();
        let (mut r, w) = Routes::load(dir.path());
        assert!(w.is_none());
        r.record("a/b#1", &me(), 5).unwrap();
        let (r2, _) = Routes::load(dir.path());
        assert_eq!(
            r2.live_view(&["a/b#1".into()]).pids,
            vec![std::process::id()]
        );
        assert!(r2.live_view(&["a/b#2".into()]).pids.is_empty());
    }

    #[test]
    fn dead_routes_are_ignored_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut dead = me();
        dead.proc_start = "0".into();
        let (mut r, _) = Routes::load(dir.path());
        r.record("a/b#9", &dead, 1).unwrap();
        let (r2, _) = Routes::load(dir.path());
        assert!(r2.live_view(&["a/b#9".into()]).pids.is_empty());
        let (mut r3, _) = Routes::load(dir.path());
        r3.record("a/b#1", &me(), 2).unwrap();
        let text = std::fs::read_to_string(dir.path().join("routes.json")).unwrap();
        assert!(!text.contains("a/b#9"), "dead route pruned");
    }

    #[test]
    fn concurrent_records_keep_both() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    let (mut r, _) = Routes::load(&p);
                    r.record(&format!("a/b#{i}"), &me(), i).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let (r, _) = Routes::load(&p);
        let keys: Vec<String> = (0..8).map(|i| format!("a/b#{i}")).collect();
        assert_eq!(r.routes.len(), 8);
        assert_eq!(r.live_view(&keys).pids.len(), 1);
    }

    #[test]
    fn unreadable_store_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("routes.json"), "{").unwrap();
        let (mut r, _) = Routes::load(dir.path());
        assert!(r.record("a/b#1", &me(), 1).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("routes.json")).unwrap(),
            "{"
        );
    }

    #[test]
    fn corrupt_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("routes.json"), "{").unwrap();
        let (r, w) = Routes::load(dir.path());
        assert!(w.is_some() && r.routes.is_empty());
        assert!(
            r.live_view(&["a/b#1".into()]).uncertain,
            "no auto on a broken store"
        );
    }
}
