//! The `watch` command: poll GitHub, keep the queue on disk, and deliver
//! queued review feedback to the one session that owns each PR.
//!
//! Delivery order per PR is prepare → commit → send: the frame is built
//! without side effects, the items are recorded as delivered (durably),
//! and only then is the frame sent. A crash between the last two steps
//! loses a message rather than duplicating it.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::app::Exit;
use crate::cli::{Authors, WatchArgs};
use crate::delivery::{self, Blocked};
use crate::github::{self, FetchError};
use crate::paths;
use crate::pipeline::{Ctx, gather, load_sessions, rank};
use crate::registry;
use crate::resolve::{Decision, Ranking, RouteView};
use crate::review::{Feedback, Item, RateLimit};
use crate::routes::Routes;
use crate::slug::RepoSlug;
use crate::target::Target;
use crate::time::now_ms;
use crate::transport::{self, Priority, SendError};
use crate::watch::{self, Filter, MAX_INSTRUCTION, STATE_VERSION, State};
use crate::{cli::Common, message};

const STATE_FILE: &str = "watch.json";
const LOCK_FILE: &str = "watch.lock";
const HOUR_MS: i64 = 3_600_000;
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MIN_RATE_WAIT: Duration = Duration::from_secs(60);

fn log(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

// ---------------------------------------------------------------- store

/// Read the state file. Missing → `None`; unreadable, unparsable, or of
/// another version → an error (never silently replaced).
fn load_state(dir: &Path) -> Result<Option<State>> {
    let path = dir.join(STATE_FILE);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let state: State = serde_json::from_slice(&data).with_context(|| {
        format!(
            "{} is corrupt; fix or delete it (deleting forgets what was delivered)",
            path.display()
        )
    })?;
    if state.version != STATE_VERSION {
        bail!(
            "{} has version {}; this build reads version {STATE_VERSION}",
            path.display(),
            state.version
        );
    }
    Ok(Some(state))
}

/// The only way to write the state: dry run never creates one. Holding it
/// also holds the single-instance lock.
struct Persist {
    dir: PathBuf,
    _lock: File,
}

impl Persist {
    /// Create the state directory and take the exclusive lock.
    fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join(LOCK_FILE))
            .context("opening the watch lock")?;
        // SAFETY: flock on an open descriptor; released when `lock` drops.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!("another gh-dash-claude watch is running");
            }
            return Err(e).context("locking the watch state");
        }
        Ok(Persist {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    /// Write the state durably: temp file, fsync, rename, fsync the dir.
    fn save(&self, state: &State) -> Result<()> {
        let data = serde_json::to_vec_pretty(state)?;
        let mut tmp = tempfile::Builder::new()
            .prefix("watch.json.")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .tempfile_in(&self.dir)
            .context("creating a temporary state file")?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.dir.join(STATE_FILE))
            .map_err(|e| e.error)
            .context("replacing watch.json")?;
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .context("syncing the state directory")?;
        Ok(())
    }
}

// ---------------------------------------------------------------- timing

/// How long to wait after a successful poll.
fn wait_after(rate: Option<RateLimit>, interval: Duration, now: i64) -> Duration {
    match rate {
        Some(r) if u64::from(r.remaining) < 2 * u64::from(r.cost) => {
            let until = Duration::from_millis((r.reset_at - now).max(0) as u64);
            until.max(interval)
        }
        _ => interval,
    }
}

/// How long to wait after the k-th consecutive failed poll (k ≥ 1).
fn backoff(err: &FetchError, k: u32, interval: Duration, now: i64) -> Duration {
    let exp = interval
        .saturating_mul(2u32.saturating_pow(k.min(16)))
        .min(MAX_BACKOFF);
    match err {
        FetchError::RateLimited {
            retry_at: Some(t), ..
        } => Duration::from_millis((t - now).max(0) as u64),
        FetchError::RateLimited { retry_at: None, .. } => exp.max(MIN_RATE_WAIT),
        FetchError::Other(_) => exp,
    }
}

// ---------------------------------------------------------------- runner

/// Settings fixed for the whole run.
struct Settings {
    repos: Vec<RepoSlug>,
    anyone: bool,
    bots: bool,
    pending_ms: i64,
    interval: Duration,
    instruction: String,
    priority: Priority,
    dry_run: bool,
    common: Common,
}

/// A failed poll: transient ones are retried; fatal ones end the run.
enum PollError {
    Transient(FetchError),
    Fatal(anyhow::Error),
}

impl From<FetchError> for PollError {
    fn from(e: FetchError) -> Self {
        PollError::Transient(e)
    }
}

struct Runner {
    s: Settings,
    dir: PathBuf,
    state: State,
    /// `None` in dry run: nothing can be written.
    persist: Option<Persist>,
    /// Per PR, the last "holding" reason logged.
    held: HashMap<String, String>,
    /// Item ids already logged as skipped (no usable time).
    skipped: HashSet<String>,
}

impl Runner {
    fn filter(&self, viewer: &str) -> Filter {
        Filter {
            anyone: self.s.anyone,
            bots: self.s.bots,
            viewer: viewer.to_string(),
        }
    }

    /// Persist the state; on failure, reload it from disk (dropping the
    /// in-memory changes) and report `false`. A failed reload is fatal.
    fn save(&mut self) -> Result<bool, PollError> {
        let Some(p) = &self.persist else {
            return Ok(true);
        };
        match p.save(&self.state) {
            Ok(()) => Ok(true),
            Err(e) => {
                log(format!("could not save the state: {e:#}; reloading it"));
                self.state = load_state(&self.dir)
                    .and_then(|s| s.ok_or_else(|| anyhow!("the state file disappeared")))
                    .map_err(PollError::Fatal)?;
                Ok(false)
            }
        }
    }

    /// Fold one fetch into the state: expire, admit, refilter, and save if
    /// anything changed. Returns `false` when the save failed.
    fn apply(&mut self, fb: &Feedback, now: i64) -> Result<bool, PollError> {
        let before = self.state.clone();
        let filter = self.filter(&fb.viewer);
        for (key, i) in self.state.advance(now, self.s.pending_ms) {
            log(format!(
                "{key}: dropped after {}h without a decisive owner: {}",
                self.s.pending_ms / HOUR_MS,
                item_line(&i)
            ));
        }
        for pf in &fb.prs {
            let key = pf.pr.key();
            let r = self.state.admit(&key, &pf.items, &filter);
            for i in &r.added {
                log(format!("{key}: queued {}", item_line(i)));
            }
            for i in r.skipped {
                if self.skipped.insert(i.id.clone()) {
                    log(format!(
                        "{key}: skipped (no publication time): {}",
                        item_line(&i)
                    ));
                }
            }
        }
        for (key, i) in self.state.refilter(&filter) {
            log(format!(
                "{key}: dropped (excluded by the current options): {}",
                item_line(&i)
            ));
        }
        if self.state != before {
            return self.save();
        }
        Ok(true)
    }

    /// One poll: fetch, update the queue, deliver what is decisive.
    fn poll(&mut self) -> Result<Duration, PollError> {
        let ctx = Ctx::new(&self.s.common);
        let fb = github::fetch_my_open_prs_with_feedback(&self.s.repos)?;
        ctx.phase("fetch");
        let now = now_ms();
        let wait = wait_after(fb.rate, self.s.interval, now);
        if !self.apply(&fb, now)? {
            return Ok(wait);
        }
        let targets: Vec<Target> = fb
            .prs
            .iter()
            .filter(|p| !self.state.pending(&p.pr.key()).is_empty())
            .map(|p| Target::Pr(p.pr.clone()))
            .collect();
        if targets.is_empty() {
            return Ok(wait);
        }
        let sessions = load_sessions(&ctx);
        let fetched = targets.clone();
        let (_, world) = gather(&ctx, &sessions.eligible, move || Ok(fetched))
            .map_err(|e| PollError::Transient(FetchError::Other(e)))?;
        let (routes, warn) = Routes::load(&paths::state_dir());
        if let Some(w) = warn {
            log(format!("warning: {w}"));
        }
        for t in &targets {
            let view = routes.live_view_checked(&t.route_keys(), |pid| {
                registry::load_one(&ctx.sessions_dir, pid).map(|s| s.session_id)
            });
            let ranking = rank(&ctx, &world, &view, t);
            match ranking.decision {
                Decision::Auto(i) => self.deliver(&ctx, &world, t, &ranking, i)?,
                _ => self.hold(&t.key(), &undecided_reason(&ranking, &view, &world)),
            }
        }
        Ok(wait)
    }

    fn hold(&mut self, key: &str, reason: &str) {
        let n = self.state.pending(key).len();
        let line = format!("{key}: holding {n} item(s): {reason}");
        if self.held.get(key) != Some(&line) {
            log(&line);
            self.held.insert(key.to_string(), line);
        }
    }

    fn deliver(
        &mut self,
        ctx: &Ctx,
        world: &crate::pipeline::World,
        t: &Target,
        ranking: &Ranking,
        i: usize,
    ) -> Result<(), PollError> {
        let key = t.key();
        let cand = &ranking.candidates[i];
        let chosen = &world.ev[cand.session].session;
        // Prepare: no side effects.
        let now = match delivery::recheck(&ctx.sessions_dir, chosen) {
            Ok(now) => now,
            Err(Blocked::Gone(why)) => {
                self.hold(&key, &why);
                return Ok(());
            }
            Err(Blocked::Cleared { .. }) => {
                self.hold(
                    &key,
                    &format!(
                        "{} was cleared (/clear) since its evidence was read",
                        chosen.name
                    ),
                );
                return Ok(());
            }
        };
        let items: Vec<Item> = self.state.pending(&key).to_vec();
        let body_for = |n: usize| {
            message::build(
                t,
                cand.worktree.as_deref(),
                &ctx.home,
                &watch::compose(&self.s.instruction, &items, n),
            )
        };
        let frame_len = |n: usize| {
            transport::encode(&body_for(n), &now.session_id, self.s.priority)
                .map_or(usize::MAX, |(f, _)| f.len())
        };
        let Some(n) = watch::fit(&items, frame_len) else {
            log(format!(
                "{key}: bug: one item does not fit the message budget; keeping it queued"
            ));
            return Ok(());
        };
        let body = body_for(n);
        let out = match delivery::encode(
            &now,
            &now.session_id,
            &body,
            self.s.priority,
            &ctx.sessions_dir,
        ) {
            Ok(o) => o,
            Err(e) => {
                log(format!("{key}: bug: could not encode the message: {e:#}"));
                return Ok(());
            }
        };
        let sent = &items[..n];
        let ids: Vec<String> = sent.iter().map(|i| i.id.clone()).collect();
        if self.s.dry_run {
            println!(
                "=== {key} → {} (pid {}), {n} of {} item(s)\n{body}\n",
                now.name,
                now.pid,
                items.len()
            );
            return Ok(());
        }
        // Commit, durably, before sending.
        self.state.commit(&key, &ids);
        if !self.save()? {
            log(format!("{key}: not sent (state not saved)"));
            return Ok(());
        }
        self.held.remove(&key);
        match delivery::send(&out) {
            Ok(()) => log(format!(
                "{key}: sent {n} item(s) to {} (pid {}, msg {})",
                now.name, now.pid, out.msg_id
            )),
            Err(SendError::Unknown(e)) => log(format!(
                "{key}: outcome unknown for {n} item(s) to {} (msg {}): {e}; not resending",
                now.name, out.msg_id
            )),
            Err(SendError::NotSent(e)) => {
                self.state.rollback(&key, sent);
                if self.save()? {
                    log(format!(
                        "{key}: not sent to {}: {e}; {n} item(s) stay queued",
                        now.name
                    ));
                } else {
                    // The reload restored the committed state: the items
                    // count as delivered and are not retried.
                    log(format!(
                        "{key}: not sent to {}: {e}; the state could not be saved, so \
                         {n} item(s) are dropped",
                        now.name
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One line naming an item: kind, author, URL.
fn item_line(i: &Item) -> String {
    let who = i.author.as_ref().map_or("ghost", |a| a.login.as_str());
    format!("{:?} by @{who} {}", i.kind, i.url)
}

/// Why a PR's queue is held, stable across polls (no scores).
fn undecided_reason(r: &Ranking, view: &RouteView, world: &crate::pipeline::World) -> String {
    if r.decision == Decision::NoCandidates {
        return "no eligible Claude Code session".into();
    }
    if view.uncertain {
        return "a remembered route is uncertain (cleared or unreadable); pick the session \
                again in gh-dash"
            .into();
    }
    let owners = r.candidates.iter().filter(|c| !c.owner.is_empty()).count();
    match (owners, r.top()) {
        (0, _) => "no session shows owner evidence".into(),
        (1, Some(c)) => format!(
            "{} owns it, but a remembered route names another session",
            world.ev[c.session].session.name
        ),
        (n, _) => format!("{n} sessions show owner evidence"),
    }
}

/// Hours to milliseconds, refusing values that do not fit.
fn hours_ms(h: u64) -> Result<i64> {
    i64::try_from(h)
        .ok()
        .and_then(|h| h.checked_mul(HOUR_MS))
        .ok_or_else(|| anyhow!("{h} hours is out of range"))
}

/// The instruction text: the flag, the file, or the default.
fn instruction(a: &WatchArgs) -> Result<String> {
    let text = match (&a.instruction, &a.instruction_file) {
        (Some(t), _) => t.clone(),
        (None, Some(p)) => {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
        }
        (None, None) => watch::DEFAULT_INSTRUCTION.to_string(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("the instruction is empty");
    }
    if text.len() > MAX_INSTRUCTION {
        bail!(
            "the instruction is {} bytes; at most {MAX_INSTRUCTION} are accepted",
            text.len()
        );
    }
    Ok(text)
}

/// Entry point of `gh-dash-claude watch`.
pub fn run(a: &WatchArgs) -> Result<Exit> {
    let repos: Vec<RepoSlug> = a
        .repos
        .iter()
        .map(|r| RepoSlug::parse(r).ok_or_else(|| anyhow!("invalid repository {r:?}")))
        .collect::<Result<_>>()?;
    let s = Settings {
        repos,
        anyone: a.from == Authors::Anyone,
        bots: !a.no_bots,
        pending_ms: hours_ms(a.pending_hours)?,
        interval: Duration::from_secs(a.interval),
        instruction: instruction(a)?,
        priority: a.priority,
        dry_run: a.dry_run,
        common: a.common.clone(),
    };
    let dir = paths::state_dir();
    let start = now_ms() - hours_ms(a.lookback)?;
    let mut runner = start_runner(s, &dir, start)?;
    let mut failures: u32 = 0;
    loop {
        match runner.poll() {
            Ok(wait) => {
                failures = 0;
                if a.once {
                    return Ok(Exit::Ok);
                }
                std::thread::sleep(wait);
            }
            Err(PollError::Fatal(e)) => return Err(e),
            Err(PollError::Transient(e)) => {
                if a.once {
                    return Err(anyhow!("{e}"));
                }
                failures = failures.saturating_add(1);
                let wait = backoff(&e, failures, runner.s.interval, now_ms());
                log(format!(
                    "poll failed: {e}; next attempt in {}s",
                    wait.as_secs()
                ));
                std::thread::sleep(wait);
            }
        }
    }
}

/// Load (or create) the state; outside dry run, lock and save it first.
fn start_runner(s: Settings, dir: &Path, start: i64) -> Result<Runner> {
    let persist = if s.dry_run {
        None
    } else {
        Some(Persist::acquire(dir)?)
    };
    let state = match load_state(dir)? {
        Some(st) => st,
        None => State::new(start),
    };
    let runner = Runner {
        s,
        dir: dir.to_path_buf(),
        state,
        persist,
        held: HashMap::new(),
        skipped: HashSet::new(),
    };
    if let Some(p) = &runner.persist {
        p.save(&runner.state)?;
    }
    if runner.s.dry_run {
        log("dry run: nothing is sent and no file is written");
    }
    Ok(runner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{Author, AuthorKind, ItemKind, PrFeedback};
    use crate::target::PrTarget;

    fn settings(dry_run: bool) -> Settings {
        Settings {
            repos: vec![],
            anyone: false,
            bots: true,
            pending_ms: 24 * HOUR_MS,
            interval: Duration::from_secs(120),
            instruction: watch::DEFAULT_INSTRUCTION.into(),
            priority: Priority::Next,
            dry_run,
            common: Common {
                tail_mib: 8,
                max_age_days: 7,
                timing: false,
            },
        }
    }

    fn feedback(published: i64) -> Feedback {
        let pr = PrTarget {
            base: RepoSlug::parse("upstream/project").unwrap(),
            number: 1,
            title: "t".into(),
            url: "https://github.com/upstream/project/pull/1".into(),
            head_repo: RepoSlug::parse("alice/project"),
            head_ref: "pr-topic".into(),
            head_oid: "abc".into(),
            files: vec![],
        };
        let item = Item {
            id: "C1".into(),
            kind: ItemKind::Comment,
            author: Some(Author {
                login: "bob".into(),
                kind: AuthorKind::User,
            }),
            association: "MEMBER".into(),
            published: Some(published),
            url: "u".into(),
            body: "b".into(),
            path: None,
            line: None,
            review_state: None,
        };
        Feedback {
            viewer: "alice".into(),
            prs: vec![PrFeedback {
                pr,
                items: vec![item],
            }],
            rate: None,
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn store_round_trip_is_durable_and_clean() {
        let dir = tempfile::tempdir().unwrap();
        let p = Persist::acquire(dir.path()).unwrap();
        let mut s = State::new(5);
        s.admit(
            "upstream/project#1",
            &feedback(10).prs[0].items,
            &Filter {
                anyone: false,
                bots: true,
                viewer: "alice".into(),
            },
        );
        p.save(&s).unwrap();
        assert_eq!(load_state(dir.path()).unwrap(), Some(s));
        assert_eq!(listing(dir.path()), vec!["watch.json", "watch.lock"]);
    }

    #[test]
    fn missing_state_is_none_and_bad_state_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_state(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join(STATE_FILE), "{").unwrap();
        assert!(load_state(dir.path()).is_err());
        std::fs::write(
            dir.path().join(STATE_FILE),
            r#"{"version":2,"cutoff":0,"prs":{}}"#,
        )
        .unwrap();
        assert!(load_state(dir.path()).is_err());
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = Persist::acquire(dir.path()).unwrap();
        let second = Persist::acquire(dir.path());
        assert!(second.is_err_and(|e| e.to_string().contains("another")));
        drop(first);
        assert!(Persist::acquire(dir.path()).is_ok());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A running watcher's state, with an item about to expire.
        let mut s = State::new(0);
        let old = feedback(1);
        s.admit(
            "upstream/project#1",
            &old.prs[0].items,
            &Filter {
                anyone: false,
                bots: true,
                viewer: "alice".into(),
            },
        );
        Persist::acquire(dir.path()).unwrap().save(&s).unwrap();
        let before = std::fs::read(dir.path().join(STATE_FILE)).unwrap();
        let files = listing(dir.path());

        let mut r = start_runner(settings(true), dir.path(), 0).unwrap();
        assert!(r.persist.is_none());
        let now = 100 * HOUR_MS;
        let mut fresh = feedback(now - 1);
        fresh.prs[0].items[0].id = "C2".into();
        assert!(r.apply(&fresh, now).ok().unwrap());
        // In memory: expired C1, queued C2. On disk: unchanged.
        assert_eq!(r.state.pending("upstream/project#1")[0].id, "C2");
        assert_eq!(std::fs::read(dir.path().join(STATE_FILE)).unwrap(), before);
        assert_eq!(listing(dir.path()), files);
    }

    #[test]
    fn dry_run_creates_no_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let r = start_runner(settings(true), &state_dir, 0).unwrap();
        assert!(r.persist.is_none());
        assert!(!state_dir.exists());
    }

    #[test]
    fn rate_budget_and_backoff() {
        let i = Duration::from_secs(120);
        let now = 1_000_000;
        assert_eq!(wait_after(None, i, now), i);
        let plenty = RateLimit {
            cost: 8,
            remaining: 100,
            reset_at: now + 3_600_000,
        };
        assert_eq!(wait_after(Some(plenty), i, now), i);
        let low = RateLimit {
            remaining: 15,
            ..plenty
        };
        assert_eq!(wait_after(Some(low), i, now), Duration::from_secs(3_600));

        let other = FetchError::Other(anyhow!("x"));
        assert_eq!(backoff(&other, 1, i, now), Duration::from_secs(240));
        assert_eq!(backoff(&other, 10, i, now), MAX_BACKOFF);
        let header = FetchError::RateLimited {
            secondary: true,
            retry_at: Some(now + 30_000),
        };
        assert_eq!(backoff(&header, 5, i, now), Duration::from_secs(30));
        let blind = FetchError::RateLimited {
            secondary: true,
            retry_at: None,
        };
        let short = Duration::from_secs(10);
        assert_eq!(backoff(&blind, 1, short, now), MIN_RATE_WAIT);
        assert_eq!(backoff(&blind, 1, i, now), Duration::from_secs(240));
    }

    #[test]
    fn hour_conversion_is_checked() {
        assert_eq!(hours_ms(2).unwrap(), 2 * HOUR_MS);
        assert!(hours_ms(u64::MAX).is_err());
        assert!(hours_ms(u64::MAX / 1000).is_err());
    }

    #[test]
    fn instruction_limits() {
        let mut a = WatchArgs {
            once: true,
            interval: 120,
            repos: vec![],
            from: Authors::Maintainers,
            no_bots: false,
            lookback: 0,
            pending_hours: 24,
            instruction: None,
            instruction_file: None,
            priority: Priority::Next,
            dry_run: true,
            common: settings(true).common,
        };
        assert_eq!(instruction(&a).unwrap(), watch::DEFAULT_INSTRUCTION);
        a.instruction = Some("  Fix it.  ".into());
        assert_eq!(instruction(&a).unwrap(), "Fix it.");
        a.instruction = Some(" ".into());
        assert!(instruction(&a).is_err());
        a.instruction = Some("x".repeat(MAX_INSTRUCTION + 1));
        assert!(instruction(&a).is_err());
    }
}
