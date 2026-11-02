//! The pure resolver: evidence in, ranking and decision out. No I/O happens here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::gitinfo::{TagCheck, Worktree};
use crate::keys::{KeyId, KeySet};
use crate::slug::RepoSlug;
use crate::target::{PrTarget, Target};
use crate::transcript::{DestRepo, Publication, RootTouch, SessionEvidence, Span};

/// Git facts the resolver needs, gathered beforehand.
#[derive(Debug, Default)]
pub struct GitView {
    pub worktrees: HashMap<PathBuf, Worktree>,
    /// Common git dir → tag check of the publication destination names.
    pub tags: HashMap<PathBuf, TagCheck>,
    /// (worktree git dir, remote) → effective push destination.
    pub push_slugs: HashMap<(PathBuf, String), Option<RepoSlug>>,
}

/// PIDs of the live remembered routes for the target (and, for an issue,
/// its linked PRs).
#[derive(Debug, Default, Clone)]
pub struct RouteView {
    pub pids: Vec<u32>,
    /// A route may exist that cannot be checked (unreadable store, or a
    /// process whose identity cannot be read): never route automatically.
    pub uncertain: bool,
}

/// One piece of evidence, with its points and a human reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub id: &'static str,
    pub points: f64,
    pub reason: String,
}

/// One session's standing for a target.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Index into the evidence slice.
    pub session: usize,
    pub pid: u32,
    pub owner: Vec<Signal>,
    pub hints: Vec<Signal>,
    pub score: f64,
    /// The worktree named by the best owner signal.
    pub worktree: Option<PathBuf>,
    pub latest_work: i64,
}

impl Candidate {
    /// The signals, owner first, best first.
    pub fn signals(&self) -> impl Iterator<Item = &Signal> {
        self.owner.iter().chain(self.hints.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Exactly one owner and no conflicting route: send to this candidate.
    Auto(usize),
    /// Ask the user; the default is always row 0.
    Pick { default: usize },
    /// No eligible session.
    NoCandidates,
}

#[derive(Debug, Clone)]
pub struct Ranking {
    pub candidates: Vec<Candidate>,
    pub decision: Decision,
}

impl Ranking {
    pub fn top(&self) -> Option<&Candidate> {
        self.candidates.first()
    }
}

/// Resolution parameters.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub now: i64,
    pub max_age: i64,
}

const O1: f64 = 100.0;
const O2: f64 = 90.0;
const O3: f64 = 80.0;
const O4: f64 = 70.0;
const W: f64 = 15.0;

#[derive(Default)]
struct PrEval {
    owner: Vec<Signal>,
    hints: Vec<Signal>,
    worktree: Option<(f64, PathBuf)>,
    weak: Option<String>,
}

/// Total mention counts per key over all sessions.
fn totals(ev: &[SessionEvidence], keys: &KeySet) -> Vec<u32> {
    let mut t = vec![0u32; keys.len()];
    for e in ev {
        for (slot, c) in t.iter_mut().zip(&e.mentions) {
            *slot += c;
        }
    }
    t
}

fn sum(ids: &[KeyId], counts: &[u32]) -> u32 {
    ids.iter()
        .map(|&i| counts.get(i).copied().unwrap_or(0))
        .sum()
}

fn share(ids: &[KeyId], mine: &[u32], all: &[u32]) -> f64 {
    let total = sum(ids, all);
    if total == 0 {
        0.0
    } else {
        f64::from(sum(ids, mine)) / f64::from(total)
    }
}

const STOP: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "this", "that", "are", "was", "use", "add", "fix",
    "feat", "pr", "one", "per", "its", "not", "but", "all", "new", "via", "out", "off",
];

fn tokens(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOP.contains(&t.as_str()))
        .collect()
}

fn title_hint(target_text: &str, e: &SessionEvidence) -> Option<Signal> {
    let a = tokens(target_text);
    let mut btext = e.title().unwrap_or("").to_string();
    if let Some(n) = e.session.explicit_name() {
        btext.push(' ');
        btext.push_str(n);
    }
    let b = tokens(&btext);
    let inter = a.intersection(&b).count();
    // One shared word is coincidence (seen on real data); require two.
    if inter < 2 {
        return None;
    }
    let denom = a.len().min(b.len()).max(1);
    let points = (10.0 * inter as f64 / denom as f64).min(10.0);
    (points > 0.0).then(|| Signal {
        id: "T",
        points,
        reason: format!("title shares {inter} word(s)"),
    })
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(8)]
}

fn branch_refs(u: &Publication, git: &GitView) -> Vec<String> {
    let Some(w) = git.worktrees.get(&u.root) else {
        return Vec::new();
    };
    let check = git.tags.get(&w.common_dir).unwrap_or(&TagCheck::Failed);
    // A destination counts as a branch only when neither it nor its source
    // is a tag (a source missing from the record fails closed).
    u.dest_refs
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            let src = u.sources.get(*i);
            !check.is_tag(r) && src.is_some_and(|s| !check.is_tag(s))
        })
        .map(|(_, r)| r.clone())
        .collect()
}

fn dest_slug(u: &Publication, git: &GitView) -> Option<RepoSlug> {
    match &u.dest_repo {
        DestRepo::Url(s) => Some(s.clone()),
        DestRepo::Remote(r) => {
            let w = git.worktrees.get(&u.root)?;
            git.push_slugs.get(&(w.git_dir.clone(), r.clone()))?.clone()
        }
    }
}

fn eval_pr(
    p: &PrTarget,
    e: &SessionEvidence,
    git: &GitView,
    keys: &KeySet,
    all_counts: &[u32],
    params: Params,
) -> PrEval {
    let mut out = PrEval::default();
    let head = p.head_repo.as_ref();
    let cutoff = params.now - params.max_age;

    // O1 / O2, over every root the session touched or pushed from.
    let empty = RootTouch::default();
    let mut roots: Vec<&PathBuf> = e.roots.keys().collect();
    for u in &e.publications {
        if !e.roots.contains_key(&u.root) && !roots.contains(&&u.root) {
            roots.push(&u.root);
        }
    }
    for root in roots {
        let touch = e.roots.get(root).unwrap_or(&empty);
        let Some(w) = git.worktrees.get(root) else {
            continue;
        };
        if !w.belongs_to(&p.base, head) {
            continue;
        }
        let state_head = w.head_oid.as_deref() == Some(p.head_oid.as_str());
        let state_track = head.is_some_and(|h| {
            w.upstream
                .as_ref()
                .is_some_and(|(s, b)| s == h && *b == p.head_ref)
        });
        if !state_head && !state_track {
            continue;
        }
        let mut spans: Vec<Span> = touch.work_spans.clone();
        // A push is work on this worktree only when it publishes the
        // worktree's own branch, its upstream branch, or the PR head; a push
        // of HEAD to some other name (possibly a remote-only tag) is not.
        let own: Vec<&str> = [
            w.branch.as_deref(),
            w.upstream.as_ref().map(|(_, b)| b.as_str()),
            Some(p.head_ref.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        spans.extend(
            e.publications
                .iter()
                .filter(|u| {
                    u.root == *root
                        && branch_refs(u, git)
                            .iter()
                            .any(|r| own.contains(&r.as_str()))
                })
                .map(|u| u.span),
        );
        let newest_support = w.last_move.as_ref().and_then(|m| {
            let after = spans
                .iter()
                .filter(|s| s.start >= m.at_ms + 1000)
                .map(|s| s.end)
                .max();
            let made = touch
                .moves
                .iter()
                .filter(|c| {
                    c.span.start <= m.at_ms + 1000
                        && c.span.end >= m.at_ms
                        && c.verb.matches_reflog(&m.message)
                        && c.report_oids
                            .iter()
                            .any(|o| m.new_oid.starts_with(o.as_str()))
                })
                .map(|c| c.span.end)
                .max();
            after.max(made)
        });
        let what = if state_head {
            format!("HEAD {} in {}", short(&p.head_oid), root.display())
        } else {
            format!(
                "tracks {}:{} in {}",
                head.map(RepoSlug::as_str).unwrap_or("?"),
                p.head_ref,
                root.display()
            )
        };
        match newest_support {
            Some(t) if t >= cutoff => {
                let (id, points) = if state_head { ("O1", O1) } else { ("O2", O2) };
                if out.worktree.as_ref().is_none_or(|(best, _)| points > *best) {
                    out.worktree = Some((points, root.clone()));
                }
                out.owner.push(Signal {
                    id,
                    points,
                    reason: what,
                });
            }
            Some(_) => out.weak = Some(format!("{what} (work expired)")),
            None => out.weak = Some(format!("{what} (no work after the last HEAD move)")),
        }
    }

    // O3.
    for u in &e.publications {
        let Some(w) = git.worktrees.get(&u.root) else {
            continue;
        };
        if !w.belongs_to(&p.base, head) {
            continue;
        }
        let qualified = head.is_some_and(|h| dest_slug(u, git).as_ref() == Some(h))
            && branch_refs(u, git).contains(&p.head_ref);
        if !qualified {
            continue;
        }
        let what = format!(
            "pushed {}:{} from {}",
            head.map(RepoSlug::as_str).unwrap_or("?"),
            p.head_ref,
            u.root.display()
        );
        if u.span.end >= cutoff {
            if out.worktree.as_ref().is_none_or(|(best, _)| O3 > *best) {
                out.worktree = Some((O3, u.root.clone()));
            }
            out.owner.push(Signal {
                id: "O3",
                points: O3,
                reason: what,
            });
        } else {
            out.weak = Some(format!("{what} (expired)"));
        }
    }

    // Hints.
    if let Some(tk) = keys.target(&p.key()) {
        let mut url_ids = tk.url.clone();
        url_ids.extend(&tk.short);
        let m = 20.0 * share(&url_ids, &e.mentions, all_counts)
            + 20.0 * share(&tk.refs, &e.mentions, all_counts);
        if m > 0.0 {
            out.hints.push(Signal {
                id: "M",
                points: m,
                reason: format!(
                    "mentions: {} of {} PR refs, {} of {} branch refs",
                    sum(&url_ids, &e.mentions),
                    sum(&url_ids, all_counts),
                    sum(&tk.refs, &e.mentions),
                    sum(&tk.refs, all_counts)
                ),
            });
        }
    }
    if !p.files.is_empty() {
        let files: HashSet<&str> = p.files.iter().map(String::as_str).collect();
        let edited: HashSet<&str> = e
            .edited
            .iter()
            .filter(|(root, _)| {
                git.worktrees
                    .get(*root)
                    .is_some_and(|w| w.belongs_to(&p.base, head))
            })
            .flat_map(|(_, set)| set.iter().map(String::as_str))
            .collect();
        let overlap = files.intersection(&edited).count();
        if overlap > 0 {
            out.hints.push(Signal {
                id: "F",
                points: 15.0 * overlap as f64 / files.len() as f64,
                reason: format!("edited {overlap} of {} PR files", files.len()),
            });
        }
    }
    if let Some(t) = title_hint(&format!("{} {}", p.title, p.head_ref), e) {
        out.hints.push(t);
    }
    out
}

fn latest_work(e: &SessionEvidence, git: &GitView) -> i64 {
    let spans = e
        .roots
        .values()
        .flat_map(|t| t.work_spans.iter().map(|s| s.end));
    let moves = e.roots.values().flat_map(|t| {
        t.moves
            .iter()
            .filter(|m| !m.report_oids.is_empty())
            .map(|m| m.span.end)
    });
    let pubs = e
        .publications
        .iter()
        .filter(|u| !branch_refs(u, git).is_empty())
        .map(|u| u.span.end);
    spans.chain(moves).chain(pubs).max().unwrap_or(0)
}

/// Rank every session for a target and decide.
pub fn resolve(
    target: &Target,
    ev: &[SessionEvidence],
    git: &GitView,
    routes: &RouteView,
    keys: &KeySet,
    params: Params,
) -> Ranking {
    if ev.is_empty() {
        return Ranking {
            candidates: Vec::new(),
            decision: Decision::NoCandidates,
        };
    }
    let all_counts = totals(ev, keys);
    let mut candidates: Vec<Candidate> = ev
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut owner = Vec::new();
            let mut hints = Vec::new();
            let mut worktree: Option<(f64, PathBuf)> = None;
            let mut weak: Option<String> = None;
            let mut absorb = |r: PrEval, prefix: Option<u64>| {
                let tag = |s: String| match prefix {
                    Some(n) => format!("#{n}: {s}"),
                    None => s,
                };
                owner.extend(r.owner.into_iter().map(|mut s| {
                    s.reason = tag(s.reason);
                    s
                }));
                hints.extend(r.hints.into_iter().map(|mut s| {
                    s.reason = tag(s.reason);
                    s
                }));
                if let Some((pts, root)) = r.worktree
                    && worktree.as_ref().is_none_or(|(b, _)| pts > *b)
                {
                    worktree = Some((pts, root));
                }
                if weak.is_none() {
                    weak = r.weak.map(tag);
                }
            };
            match target {
                Target::Pr(p) => absorb(eval_pr(p, e, git, keys, &all_counts, params), None),
                Target::Issue(issue) => {
                    for p in &issue.linked_prs {
                        absorb(
                            eval_pr(p, e, git, keys, &all_counts, params),
                            Some(p.number),
                        );
                    }
                    if let Some(tk) = keys.target(&target.key()) {
                        let mut ids = tk.url.clone();
                        ids.extend(&tk.short);
                        let m = 20.0 * share(&ids, &e.mentions, &all_counts);
                        if m > 0.0 {
                            hints.push(Signal {
                                id: "M",
                                points: m,
                                reason: format!(
                                    "mentions: {} of {} issue refs",
                                    sum(&ids, &e.mentions),
                                    sum(&ids, &all_counts)
                                ),
                            });
                        }
                    }
                    if let Some(t) = title_hint(&issue.title, e) {
                        hints.push(t);
                    }
                }
            }
            if routes.pids.contains(&e.session.pid) {
                owner.push(Signal {
                    id: "O4",
                    points: O4,
                    reason: "remembered route".into(),
                });
            }
            if let Some(w) = weak {
                hints.push(Signal {
                    id: "W",
                    points: W,
                    reason: w,
                });
            }
            owner.sort_by(|a, b| b.points.total_cmp(&a.points));
            hints.sort_by(|a, b| b.points.total_cmp(&a.points));
            let score = owner.first().map_or(0.0, |s| s.points)
                + hints.iter().map(|s| s.points).sum::<f64>();
            Candidate {
                session: i,
                pid: e.session.pid,
                owner,
                hints,
                score,
                worktree: worktree.map(|(_, r)| r),
                latest_work: latest_work(e, git),
            }
        })
        .collect();

    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.latest_work.cmp(&a.latest_work))
            .then(a.pid.cmp(&b.pid))
    });

    let owners: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.owner.is_empty())
        .map(|(i, _)| i)
        .collect();
    let decision = match owners.as_slice() {
        [only] if !routes.uncertain && routes.pids.iter().all(|&p| p == candidates[*only].pid) => {
            Decision::Auto(*only)
        }
        _ => {
            let distinct: HashSet<u32> = routes.pids.iter().copied().collect();
            if distinct.len() == 1 {
                let pid = *distinct.iter().next().unwrap();
                if let Some(pos) = candidates.iter().position(|c| c.pid == pid) {
                    let c = candidates.remove(pos);
                    candidates.insert(0, c);
                }
            }
            Decision::Pick { default: 0 }
        }
    };
    if let Decision::Auto(i) = decision
        && i != 0
    {
        // Owners outrank everyone with at least 70 points, but hints can
        // stack; keep the auto choice on the first row for display.
        let c = candidates.remove(i);
        candidates.insert(0, c);
        return Ranking {
            candidates,
            decision: Decision::Auto(0),
        };
    }
    Ranking {
        candidates,
        decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitinfo::HeadMove;
    use crate::registry::Session;
    use crate::shellparse::MoveVerb;
    use crate::target::IssueTarget;
    use crate::transcript::{MoveCandidate, RootTouch};

    const NOW: i64 = 1_800_000_000_000;
    const HOUR: i64 = 3_600_000;
    const OID: &str = "abc12344e621c24e81a87a191c2b2fab51bdd1f0";

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::parse(s).unwrap()
    }

    fn pr(n: u64, head_ref: &str) -> PrTarget {
        PrTarget {
            base: slug("upstream/project"),
            number: n,
            title: format!("PR {n}"),
            url: format!("https://github.com/upstream/project/pull/{n}"),
            head_repo: Some(slug("alice/project")),
            head_ref: head_ref.into(),
            head_oid: OID.into(),
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
        }
    }

    fn session(pid: u32) -> Session {
        Session {
            pid,
            session_id: format!("s{pid}"),
            cwd: "/main".into(),
            name: format!("tf-{pid}"),
            name_source: "derived".into(),
            kind: "interactive".into(),
            status: "idle".into(),
            proc_start: "1".into(),
            socket: Some("/tmp/x.sock".into()),
            peer_protocol: 1,
        }
    }

    fn ev(pid: u32, keys: &KeySet) -> SessionEvidence {
        SessionEvidence {
            session: session(pid),
            roots: HashMap::new(),
            publications: Vec::new(),
            edited: HashMap::new(),
            mentions: vec![0; keys.len()],
            custom_title: None,
            ai_title: None,
        }
    }

    fn worktree(root: &str, head: &str, moved_at: i64) -> Worktree {
        let mut remotes = HashMap::new();
        remotes.insert("upstream".to_string(), slug("upstream/project"));
        remotes.insert("fork".to_string(), slug("alice/project"));
        Worktree {
            root: root.into(),
            git_dir: format!("/main/.git/worktrees/{root}").into(),
            common_dir: "/main/.git".into(),
            head_oid: Some(head.into()),
            branch: Some("local".into()),
            upstream: None,
            remotes,
            last_move: Some(HeadMove {
                new_oid: head.into(),
                at_ms: moved_at,
                message: "commit: x".into(),
            }),
        }
    }

    fn work(start: i64) -> RootTouch {
        RootTouch {
            work: 1,
            reference: 0,
            work_spans: vec![Span {
                start,
                end: start + 1000,
            }],
            moves: Vec::new(),
        }
    }

    fn git_with(wts: Vec<Worktree>) -> GitView {
        GitView {
            worktrees: wts.into_iter().map(|w| (w.root.clone(), w)).collect(),
            tags: HashMap::from([(PathBuf::from("/main/.git"), TagCheck::Known(HashSet::new()))]),
            push_slugs: HashMap::new(),
        }
    }

    fn params() -> Params {
        Params {
            now: NOW,
            max_age: 7 * 24 * HOUR,
        }
    }

    fn run(
        t: &Target,
        ev: &[SessionEvidence],
        git: &GitView,
        keys: &KeySet,
        routes: &[u32],
    ) -> Ranking {
        resolve(
            t,
            ev,
            git,
            &RouteView {
                pids: routes.to_vec(),
                uncertain: false,
            },
            keys,
            params(),
        )
    }

    #[test]
    fn worktree_head_after_move_is_decisive() {
        let p = pr(1234, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let b = ev(2, &keys);
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(&Target::Pr(p), &[a, b], &git, &keys, &[]);
        assert_eq!(r.decision, Decision::Auto(0));
        assert_eq!(r.candidates[0].pid, 1);
        assert_eq!(r.candidates[0].owner[0].id, "O1");
        assert_eq!(
            r.candidates[0].worktree.as_deref(),
            Some(std::path::Path::new("/wt"))
        );
    }

    #[test]
    fn mentions_alone_never_route() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut status = ev(1, &keys);
        let tk = keys.target(&p.key()).unwrap().clone();
        status.mentions[tk.url[0]] = 1;
        status.mentions[tk.refs[0]] = 1;
        let r = run(
            &Target::Pr(p),
            &[status, ev(2, &keys)],
            &GitView::default(),
            &keys,
            &[],
        );
        assert_eq!(r.decision, Decision::Pick { default: 0 });
        assert_eq!(r.candidates[0].pid, 1, "mentions still rank");
        assert!(r.candidates[0].owner.is_empty());
    }

    #[test]
    fn reference_touch_only_is_weak() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert(
            "/wt".into(),
            RootTouch {
                reference: 5,
                ..Default::default()
            },
        );
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(&Target::Pr(p), &[a], &git, &keys, &[]);
        assert!(matches!(r.decision, Decision::Pick { .. }));
        assert_eq!(r.candidates[0].hints[0].id, "W");
    }

    #[test]
    fn work_before_another_actors_move_is_weak() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 5 * HOUR));
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(&Target::Pr(p), &[a], &git, &keys, &[]);
        assert!(r.candidates[0].owner.is_empty());
    }

    #[test]
    fn same_second_work_is_not_after_the_move() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        let m = NOW - 3 * HOUR;
        a.roots.insert("/wt".into(), work(m + 400));
        let git = git_with(vec![worktree("/wt", OID, m)]);
        let r = run(&Target::Pr(p.clone()), &[a], &git, &keys, &[]);
        assert!(r.candidates[0].owner.is_empty());
        // … unless the session itself made the move, proven by its report.
        let mut b = ev(1, &keys);
        b.roots.insert(
            "/wt".into(),
            RootTouch {
                moves: vec![MoveCandidate {
                    verb: MoveVerb::Commit,
                    span: Span {
                        start: m - 2000,
                        end: m + 3000,
                    },
                    report_oids: vec!["abc1234".into()],
                }],
                ..Default::default()
            },
        );
        let r = run(&Target::Pr(p), &[b], &git, &keys, &[]);
        assert_eq!(r.decision, Decision::Auto(0));
    }

    #[test]
    fn report_oid_must_match() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let m = NOW - HOUR;
        let mut b = ev(1, &keys);
        b.roots.insert(
            "/wt".into(),
            RootTouch {
                moves: vec![MoveCandidate {
                    verb: MoveVerb::Commit,
                    span: Span {
                        start: m - 2000,
                        end: m + 3000,
                    },
                    report_oids: vec!["1111111".into()],
                }],
                ..Default::default()
            },
        );
        let git = git_with(vec![worktree("/wt", OID, m)]);
        let r = run(&Target::Pr(p), &[b], &git, &keys, &[]);
        assert!(r.candidates[0].owner.is_empty());
    }

    #[test]
    fn missing_reflog_fails_closed() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - HOUR));
        let mut w = worktree("/wt", OID, 0);
        w.last_move = None;
        let r = run(&Target::Pr(p), &[a], &git_with(vec![w]), &keys, &[]);
        assert!(r.candidates[0].owner.is_empty());
    }

    #[test]
    fn expired_work_is_weak() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 9 * 24 * HOUR));
        let git = git_with(vec![worktree("/wt", OID, NOW - 10 * 24 * HOUR)]);
        let r = run(&Target::Pr(p), &[a], &git, &keys, &[]);
        assert!(r.candidates[0].owner.is_empty());
        assert!(r.candidates[0].hints.iter().any(|h| h.id == "W"));
    }

    #[test]
    fn two_owners_need_the_picker() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let mut b = ev(2, &keys);
        b.roots.insert("/wt".into(), work(NOW - HOUR));
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(&Target::Pr(p), &[a, b], &git, &keys, &[]);
        assert_eq!(r.decision, Decision::Pick { default: 0 });
        assert_eq!(r.candidates[0].pid, 2, "more recent work ranks first");
    }

    #[test]
    fn conflicting_route_blocks_auto_and_leads_the_picker() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let b = ev(2, &keys);
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(
            &Target::Pr(p.clone()),
            &[a.clone(), b.clone()],
            &git,
            &keys,
            &[2],
        );
        assert_eq!(r.decision, Decision::Pick { default: 0 });
        assert_eq!(r.candidates[0].pid, 2, "the remembered route is row 1");
        // A route to an ineligible session also blocks auto.
        let r = run(&Target::Pr(p.clone()), &[a.clone(), b], &git, &keys, &[99]);
        assert!(matches!(r.decision, Decision::Pick { .. }));
        assert_eq!(r.candidates[0].pid, 1);
        // A matching route keeps auto.
        let r = run(&Target::Pr(p), &[a], &git, &keys, &[1]);
        assert_eq!(r.decision, Decision::Auto(0));
    }

    #[test]
    fn uncertain_routes_block_auto() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let view = RouteView {
            pids: vec![],
            uncertain: true,
        };
        let r = resolve(&Target::Pr(p), &[a], &git, &view, &keys, params());
        assert!(matches!(r.decision, Decision::Pick { .. }));
    }

    #[test]
    fn route_alone_is_decisive() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let r = run(
            &Target::Pr(p),
            &[ev(1, &keys), ev(2, &keys)],
            &GitView::default(),
            &keys,
            &[2],
        );
        assert_eq!(r.decision, Decision::Auto(0));
        assert_eq!(r.candidates[0].pid, 2);
    }

    #[test]
    fn tracking_needs_the_fork_identity() {
        let p = pr(1, "pushed-name");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let mut w = worktree(
            "/wt",
            "ffffffffffffffffffffffffffffffffffffffff",
            NOW - 3 * HOUR,
        );
        w.upstream = Some((slug("someone/project"), "pushed-name".into()));
        let r = run(
            &Target::Pr(p.clone()),
            &[a.clone()],
            &git_with(vec![w.clone()]),
            &keys,
            &[],
        );
        assert!(
            r.candidates[0].owner.is_empty(),
            "another fork's branch of the same name"
        );
        w.upstream = Some((slug("alice/project"), "pushed-name".into()));
        let r = run(&Target::Pr(p), &[a], &git_with(vec![w]), &keys, &[]);
        assert_eq!(r.candidates[0].owner[0].id, "O2");
    }

    #[test]
    fn publication_links_a_rebased_push_branch() {
        let p = pr(1, "pr-push-copy");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.publications.push(Publication {
            root: "/wt".into(),
            dest_repo: DestRepo::Url(slug("alice/project")),
            dest_refs: vec!["pr-push-copy".into()],
            sources: vec!["HEAD".into()],
            span: Span {
                start: NOW - HOUR,
                end: NOW - HOUR + 5000,
            },
        });
        // The worktree is on another branch and another commit.
        let w = worktree(
            "/wt",
            "ffffffffffffffffffffffffffffffffffffffff",
            NOW - 2 * HOUR,
        );
        let r = run(
            &Target::Pr(p.clone()),
            &[a.clone()],
            &git_with(vec![w.clone()]),
            &keys,
            &[],
        );
        assert_eq!(r.decision, Decision::Auto(0));
        assert_eq!(r.candidates[0].owner[0].id, "O3");
        // A tag of the same name disqualifies it; so does a failed check.
        let mut g = git_with(vec![w.clone()]);
        g.tags.insert(
            "/main/.git".into(),
            TagCheck::Known(HashSet::from(["pr-push-copy".to_string()])),
        );
        assert!(
            run(&Target::Pr(p.clone()), &[a.clone()], &g, &keys, &[]).candidates[0]
                .owner
                .is_empty()
        );
        g.tags.insert("/main/.git".into(), TagCheck::Failed);
        assert!(
            run(&Target::Pr(p.clone()), &[a.clone()], &g, &keys, &[]).candidates[0]
                .owner
                .is_empty()
        );
        // A tag as the source disqualifies it.
        let mut g2 = git_with(vec![w.clone()]);
        g2.tags.insert(
            "/main/.git".into(),
            TagCheck::Known(HashSet::from(["HEAD".to_string()])),
        );
        assert!(
            run(&Target::Pr(p.clone()), &[a.clone()], &g2, &keys, &[]).candidates[0]
                .owner
                .is_empty()
        );
        // A push to another fork does not count.
        let mut other = a.clone();
        other.publications[0].dest_repo = DestRepo::Url(slug("someone/project"));
        assert!(
            run(&Target::Pr(p), &[other], &git_with(vec![w]), &keys, &[]).candidates[0]
                .owner
                .is_empty()
        );
    }

    #[test]
    fn issue_owners_are_unioned() {
        let p1 = pr(10, "pr-a");
        let p2 = pr(11, "pr-b");
        let mut w2 = worktree(
            "/wt2",
            "2222222222222222222222222222222222222222",
            NOW - 3 * HOUR,
        );
        w2.root = "/wt2".into();
        let issue = |prs: Vec<PrTarget>| {
            Target::Issue(IssueTarget {
                base: slug("upstream/project"),
                number: 5,
                title: "bug".into(),
                url: "u".into(),
                linked_prs: prs,
            })
        };
        let mut p2b = p2.clone();
        p2b.head_oid = "2222222222222222222222222222222222222222".into();
        let t = issue(vec![p1.clone(), p2b.clone()]);
        let keys = KeySet::build(&t.key_specs());
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let mut b = ev(2, &keys);
        b.roots.insert("/wt2".into(), work(NOW - 2 * HOUR));
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR), w2]);
        let r = run(&t, &[a.clone(), b.clone()], &git, &keys, &[]);
        assert!(matches!(r.decision, Decision::Pick { .. }), "two owners");
        let t1 = issue(vec![p1]);
        let keys1 = KeySet::build(&t1.key_specs());
        let r = run(&t1, &[a, b], &git, &keys1, &[]);
        assert_eq!(r.decision, Decision::Auto(0));
        assert!(r.candidates[0].owner[0].reason.starts_with("#10:"));
        let t0 = issue(vec![]);
        let k0 = KeySet::build(&t0.key_specs());
        let r = run(
            &t0,
            &[ev(1, &k0), ev(2, &k0)],
            &GitView::default(),
            &k0,
            &[2],
        );
        assert_eq!(r.decision, Decision::Auto(0));
    }

    #[test]
    fn publication_only_root_counts_as_owner_work() {
        // Session 1 has HEAD-matching work; session 2 only pushed from the
        // same worktree after the move: two owners, so no auto.
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let mut a = ev(1, &keys);
        a.roots.insert("/wt".into(), work(NOW - 2 * HOUR));
        let mut b = ev(2, &keys);
        b.publications.push(Publication {
            root: "/wt".into(),
            dest_repo: DestRepo::Url(slug("alice/project")),
            dest_refs: vec!["local".into()],
            sources: vec!["HEAD".into()],
            span: Span {
                start: NOW - HOUR,
                end: NOW - HOUR + 1000,
            },
        });
        let git = git_with(vec![worktree("/wt", OID, NOW - 3 * HOUR)]);
        let r = run(&Target::Pr(p), &[a, b], &git, &keys, &[]);
        assert!(matches!(r.decision, Decision::Pick { .. }));
    }

    #[test]
    fn push_to_an_unrelated_name_is_not_worktree_work() {
        let p = pr(1, "pr-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let m = NOW - 3 * HOUR;
        let mut a = ev(1, &keys);
        a.publications.push(Publication {
            root: "/wt".into(),
            dest_repo: DestRepo::Url(slug("alice/project")),
            dest_refs: vec!["release".into()],
            sources: vec!["HEAD".into()],
            span: Span {
                start: NOW - HOUR,
                end: NOW - HOUR + 1000,
            },
        });
        let git = git_with(vec![worktree("/wt", OID, m)]);
        let r = run(&Target::Pr(p.clone()), &[a.clone()], &git, &keys, &[]);
        assert!(
            r.candidates[0].owner.is_empty(),
            "HEAD:release is not work on the PR worktree"
        );
        a.publications[0].dest_refs = vec!["local".into()];
        let r = run(&Target::Pr(p), &[a], &git, &keys, &[]);
        assert_eq!(
            r.candidates[0].owner[0].id, "O1",
            "pushing the worktree's branch is work"
        );
    }

    #[test]
    fn no_sessions() {
        let p = pr(1, "x-topic");
        let keys = KeySet::build(&[p.key_spec()]);
        let r = run(&Target::Pr(p), &[], &GitView::default(), &keys, &[]);
        assert_eq!(r.decision, Decision::NoCandidates);
    }
}
