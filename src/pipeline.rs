//! The resolution pipeline shared by every command: load sessions, read
//! their evidence alongside a GitHub fetch, describe worktrees, and rank.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, anyhow};

use crate::cli::Common;
use crate::gitinfo::{self, RootCache, Worktree};
use crate::keys::KeySet;
use crate::paths;
use crate::proc::Liveness;
use crate::registry::{self, Eligibility, Session};
use crate::resolve::{self, GitView, Params, Ranking, RouteView};
use crate::target::Target;
use crate::time::{DAY_MS, now_ms};
use crate::transcript::{self, DestRepo, SessionEvidence};

#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) config: PathBuf,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) tail: u64,
    pub(crate) max_age: i64,
    pub(crate) timing: bool,
    pub(crate) started: Instant,
}

impl Ctx {
    pub(crate) fn new(c: &Common) -> Self {
        let config = paths::config_dir();
        Ctx {
            sessions_dir: paths::sessions_dir(&config),
            config,
            home: paths::home(),
            tail: c.tail_mib.max(1) << 20,
            max_age: c.max_age_days.max(1) * DAY_MS,
            timing: c.timing,
            started: Instant::now(),
        }
    }

    pub(crate) fn phase(&self, name: &str) {
        if self.timing {
            eprintln!("[timing] {name}: {} ms", self.started.elapsed().as_millis());
        }
    }
}

/// Registry entries split into eligible sessions and excluded ones.
pub(crate) struct Sessions {
    pub(crate) eligible: Vec<Session>,
    pub(crate) excluded: Vec<(Session, String)>,
}

pub(crate) fn load_sessions(ctx: &Ctx) -> Sessions {
    let me = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for s in registry::load_all(&ctx.sessions_dir) {
        if me.as_deref() == Some(s.session_id.as_str()) {
            excluded.push((s, "this session".into()));
            continue;
        }
        match (s.liveness(), s.eligibility()) {
            (Liveness::Live, Eligibility::Eligible) => eligible.push(s),
            (Liveness::Live, Eligibility::NoSocket) => excluded.push((s, "no inbox socket".into())),
            (Liveness::Live, Eligibility::Protocol(v)) => {
                excluded.push((s, format!("peer protocol {v}")))
            }
            (Liveness::Dead, _) => excluded.push((s, "not running".into())),
            (Liveness::Recycled, _) => excluded.push((s, "PID reused".into())),
        }
    }
    Sessions { eligible, excluded }
}

/// Everything the resolver needs.
pub(crate) struct World {
    pub(crate) ev: Vec<SessionEvidence>,
    pub(crate) git: GitView,
    pub(crate) keys: KeySet,
}

/// Run the pipeline phases: fetch targets while reading tails,
/// then scan, describe worktrees, and gather tag and push facts.
pub(crate) fn gather<F>(ctx: &Ctx, sessions: &[Session], fetch: F) -> Result<(Vec<Target>, World)>
where
    F: FnOnce() -> Result<Vec<Target>> + Send,
{
    let (targets, tails) = std::thread::scope(|scope| {
        let gh = scope.spawn(fetch);
        let readers: Vec<_> = sessions
            .iter()
            .map(|s| {
                let config = &ctx.config;
                let tail = ctx.tail;
                scope.spawn(move || {
                    paths::transcript_path(config, &s.cwd, &s.session_id)
                        .map(|p| transcript::read_tail(&p, tail))
                        .unwrap_or_default()
                })
            })
            .collect();
        let tails: Vec<Vec<u8>> = readers
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect();
        let targets = gh.join().map_err(|_| anyhow!("GitHub fetch panicked"))?;
        Ok::<_, anyhow::Error>((targets, tails))
    })?;
    let targets = targets?;
    ctx.phase("fetch + read");

    let specs: Vec<_> = targets.iter().flat_map(Target::key_specs).collect();
    let keys = KeySet::build(&specs);
    let roots = RootCache::default();
    let ev: Vec<SessionEvidence> = std::thread::scope(|scope| {
        let handles: Vec<_> = sessions
            .iter()
            .zip(&tails)
            .map(|(s, buf)| {
                let (keys, roots, home) = (&keys, &roots, &ctx.home);
                scope.spawn(move || transcript::scan(s, buf, keys, home, roots))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("scan thread"))
            .collect()
    });
    ctx.phase("scan");

    let git = describe_world(&ev);
    ctx.phase("git");
    Ok((targets, World { ev, git, keys }))
}

fn describe_world(ev: &[SessionEvidence]) -> GitView {
    let mut all_roots: Vec<PathBuf> = ev
        .iter()
        .flat_map(|e| {
            e.roots
                .keys()
                .cloned()
                .chain(e.publications.iter().map(|p| p.root.clone()))
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    all_roots.sort();
    let mut worktrees: HashMap<PathBuf, Worktree> = HashMap::new();
    for chunk in all_roots.chunks(16) {
        let described: Vec<(PathBuf, Option<Worktree>)> = std::thread::scope(|scope| {
            let hs: Vec<_> = chunk
                .iter()
                .map(|r| scope.spawn(move || (r.clone(), gitinfo::describe(r))))
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        worktrees.extend(described.into_iter().filter_map(|(r, w)| Some((r, w?))));
    }

    let mut names: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut remotes: HashSet<(PathBuf, PathBuf, String)> = HashSet::new();
    for e in ev {
        for p in &e.publications {
            let Some(w) = worktrees.get(&p.root) else {
                continue;
            };
            names
                .entry(w.common_dir.clone())
                .or_default()
                .extend(p.dest_refs.iter().chain(&p.sources).cloned());
            if let DestRepo::Remote(r) = &p.dest_repo {
                remotes.insert((p.root.clone(), w.git_dir.clone(), r.clone()));
            }
        }
    }
    let tags = names
        .into_iter()
        .map(|(dir, set)| {
            let list: Vec<String> = set.into_iter().collect();
            let check = gitinfo::tags_named(&dir, &list);
            (dir, check)
        })
        .collect();
    let push_slugs = remotes
        .into_iter()
        .map(|(root, git_dir, r)| {
            let slug = gitinfo::push_slug(&root, &r);
            ((git_dir, r), slug)
        })
        .collect();
    GitView {
        worktrees,
        tags,
        push_slugs,
    }
}

pub(crate) fn rank(ctx: &Ctx, world: &World, view: &RouteView, t: &Target) -> Ranking {
    resolve::resolve(
        t,
        &world.ev,
        &world.git,
        view,
        &world.keys,
        Params {
            now: now_ms(),
            max_age: ctx.max_age,
        },
    )
}
