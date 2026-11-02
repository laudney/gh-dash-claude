//! Read-only facts about local worktrees.
//!
//! Every git subprocess runs with `GIT_OPTIONAL_LOCKS=0` (no index refresh
//! or other optional lock that could disturb a running session),
//! `GIT_TERMINAL_PROMPT=0`, a null stdin, and a kill timeout.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::slug::RepoSlug;

const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The latest HEAD move recorded in a worktree's reflog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadMove {
    pub new_oid: String,
    /// Epoch milliseconds of the move second (the true time lies in
    /// `[at_ms, at_ms + 1000)`).
    pub at_ms: i64,
    pub message: String,
}

/// One worktree as git describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub head_oid: Option<String>,
    pub branch: Option<String>,
    /// The current branch's upstream as `(remote repository, branch)`.
    pub upstream: Option<(RepoSlug, String)>,
    /// Remote name → GitHub slug of its (fetch) URL.
    pub remotes: HashMap<String, RepoSlug>,
    pub last_move: Option<HeadMove>,
}

impl Worktree {
    /// Whether this worktree belongs to a PR: one of its remotes is
    /// the base or the head repository.
    pub fn belongs_to(&self, base: &RepoSlug, head: Option<&RepoSlug>) -> bool {
        self.remotes.values().any(|r| r == base || Some(r) == head)
    }
}

/// Result of a tag lookup for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagCheck {
    /// The lookup succeeded; these names are tags.
    Known(HashSet<String>),
    /// The lookup failed; every name must count as a tag (fail closed).
    Failed,
}

impl TagCheck {
    pub fn is_tag(&self, name: &str) -> bool {
        match self {
            TagCheck::Known(set) => set.contains(name),
            TagCheck::Failed => true,
        }
    }
}

/// Run git with the read-only environment and a timeout; return stdout on
/// a zero exit.
///
/// Every inherited `GIT_*` variable is removed first: `GIT_DIR` and its
/// kind would otherwise override `-C` and describe another repository. The
/// child runs in its own process group, which is killed on timeout, and
/// the output is collected under the same deadline.
pub fn run_git(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        // Keep the selectors of the user's own configuration files; drop
        // everything that picks a repository or injects config.
        let keep = matches!(
            name.as_ref(),
            "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_NOSYSTEM"
        );
        if name.starts_with("GIT_") && !keep {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    let out = crate::proc::run_bounded(&mut cmd, GIT_TIMEOUT)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Describe the worktree at `root`, or `None` when git cannot.
pub fn describe(root: &Path) -> Option<Worktree> {
    let r = root.to_str()?;
    let out = run_git(&[
        "-C",
        r,
        "rev-parse",
        "--absolute-git-dir",
        "--git-common-dir",
        "HEAD",
        "--symbolic-full-name",
        "HEAD",
    ])?;
    let mut lines = out.lines();
    let git_dir = PathBuf::from(lines.next()?);
    let common = PathBuf::from(lines.next()?);
    let common_dir = if common.is_absolute() {
        common
    } else {
        root.join(common)
    };
    let head_oid = lines.next().filter(|l| is_oid(l)).map(str::to_string);
    let branch = lines
        .next()
        .and_then(|b| b.strip_prefix("refs/heads/"))
        .filter(|b| !b.is_empty())
        .map(str::to_string);

    let config = run_git(&[
        "-C",
        r,
        "config",
        "-z",
        "--get-regexp",
        r"^(remote\..*\.url|branch\..*\.(remote|merge)|url\..*\.insteadof)$",
    ])
    .unwrap_or_default();
    let (remotes, tracking) = parse_config(&config);
    let upstream = branch.as_ref().and_then(|b| {
        let (remote, merge) = tracking.get(b)?;
        let m = merge.strip_prefix("refs/heads/")?;
        Some((remotes.get(remote)?.clone(), m.to_string()))
    });
    let last_move = read_last_reflog(&git_dir.join("logs/HEAD"));
    Some(Worktree {
        root: root.to_path_buf(),
        git_dir,
        common_dir,
        head_oid,
        branch,
        upstream,
        remotes,
        last_move,
    })
}

fn is_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

type Tracking = HashMap<String, (String, String)>;

/// Apply git's `url.<base>.insteadOf` rewriting: the longest matching
/// prefix wins.
fn rewrite_url(url: &str, rules: &[(String, String)]) -> Option<String> {
    let matching: Vec<&(String, String)> = rules
        .iter()
        .filter(|(from, _)| url.starts_with(from.as_str()))
        .collect();
    let Some(longest) = matching.iter().map(|(from, _)| from.len()).max() else {
        return Some(url.to_string());
    };
    let bases: HashSet<&str> = matching
        .iter()
        .filter(|(from, _)| from.len() == longest)
        .map(|(_, base)| base.as_str())
        .collect();
    // Conflicting rules of equal length: fail closed.
    let [base] = bases.into_iter().collect::<Vec<_>>()[..] else {
        return None;
    };
    Some(format!("{base}{}", &url[longest..]))
}

/// Parse `git config -z --get-regexp` output into remote slugs and branch
/// tracking `(remote, merge ref)`.
///
/// Git fetches from a remote's *first* URL, after `insteadOf` rewriting; a
/// branch with several `merge` values has no single upstream and is
/// dropped (fail closed).
pub fn parse_config(out: &str) -> (HashMap<String, RepoSlug>, Tracking) {
    let mut first_url: HashMap<String, String> = HashMap::new();
    let mut rules: Vec<(String, String)> = Vec::new();
    let mut remote_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut merge_of: HashMap<String, Vec<String>> = HashMap::new();
    // `-z` records: `key\nvalue\0`, so a value with a newline cannot
    // forge another record.
    for record in out.split('\0') {
        let Some((key, value)) = record.split_once('\n') else {
            continue;
        };
        if let Some(rest) = key.strip_prefix("remote.") {
            if let Some(name) = rest.strip_suffix(".url") {
                first_url
                    .entry(name.to_string())
                    .or_insert_with(|| value.to_string());
            }
        } else if let Some(rest) = key.strip_prefix("branch.") {
            if let Some(b) = rest.strip_suffix(".remote") {
                remote_of
                    .entry(b.to_string())
                    .or_default()
                    .push(value.to_string());
            } else if let Some(b) = rest.strip_suffix(".merge") {
                merge_of
                    .entry(b.to_string())
                    .or_default()
                    .push(value.to_string());
            }
        } else if let Some(rest) = key.strip_prefix("url.")
            && let Some(base) = rest.strip_suffix(".insteadof")
        {
            rules.push((value.to_string(), base.to_string()));
        }
    }
    let remotes = first_url
        .into_iter()
        .filter_map(|(name, url)| Some((name, RepoSlug::from_url(&rewrite_url(&url, &rules)?)?)))
        .collect();
    let tracking = remote_of
        .into_iter()
        .filter_map(|(b, rs)| {
            let [r] = rs.as_slice() else { return None };
            let ms = merge_of.get(&b)?;
            let [m] = ms.as_slice() else { return None };
            (r != ".").then(|| (b, (r.clone(), m.clone())))
        })
        .collect();
    (remotes, tracking)
}

/// Read the last complete line of a reflog file.
pub fn read_last_reflog(path: &Path) -> Option<HeadMove> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    // A record still being written has no final newline: fail closed.
    f.seek(SeekFrom::Start(len - 1)).ok()?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last).ok()?;
    if last[0] != b'\n' {
        return None;
    }
    let mut window: u64 = 4096;
    loop {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        Read::by_ref(&mut f)
            .take(len - start)
            .read_to_end(&mut buf)
            .ok()?;
        if buf.last() != Some(&b'\n') {
            return None;
        }
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim_end_matches('\n');
        if let Some(pos) = trimmed.rfind('\n') {
            return parse_reflog_line(&trimmed[pos + 1..]);
        }
        if start == 0 {
            return parse_reflog_line(trimmed);
        }
        if window >= 1 << 20 {
            return None;
        }
        window *= 4;
    }
}

/// Parse `<old> <new> <name> <<email>> <epoch> <tz>\t<message>`.
pub fn parse_reflog_line(line: &str) -> Option<HeadMove> {
    let (head, message) = line.split_once('\t').unwrap_or((line, ""));
    let mut fields = head.split(' ');
    let _old = fields.next()?;
    let new_oid = fields.next()?;
    if !is_oid(new_oid) {
        return None;
    }
    let after_email = &head[head.rfind("> ")? + 2..];
    let epoch: i64 = after_email.split(' ').next()?.parse().ok()?;
    Some(HeadMove {
        new_oid: new_oid.to_string(),
        at_ms: epoch.checked_mul(1000)?,
        message: message.to_string(),
    })
}

/// Which of `names` are tags in the repository at `common_dir`.
pub fn tags_named(common_dir: &Path, names: &[String]) -> TagCheck {
    if names.is_empty() {
        return TagCheck::Known(HashSet::new());
    }
    let Some(gd) = common_dir.to_str() else {
        return TagCheck::Failed;
    };
    let refs: Vec<String> = names.iter().map(|n| format!("refs/tags/{n}")).collect();
    let mut args = vec![
        "--git-dir",
        gd,
        "for-each-ref",
        "--format=%(refname:strip=2)",
    ];
    args.extend(refs.iter().map(String::as_str));
    match run_git(&args) {
        Some(out) => TagCheck::Known(out.lines().map(str::to_string).collect()),
        None => TagCheck::Failed,
    }
}

/// The effective GitHub push destination of a remote in this worktree
/// (`pushurl` and URL rewrites applied); `None` when it is missing,
/// ambiguous, or not on GitHub (fail closed).
pub fn push_slug(root: &Path, remote: &str) -> Option<RepoSlug> {
    let out = run_git(&[
        "-C",
        root.to_str()?,
        "remote",
        "get-url",
        "--push",
        "--all",
        remote,
    ])?;
    let urls: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    match urls.as_slice() {
        [one] => RepoSlug::from_url(one),
        _ => None,
    }
}

/// Resolve symlinks in `path`, or in its nearest existing ancestor when the
/// path itself no longer exists (a deleted file), keeping the rest.
fn real_path(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = path;
    loop {
        match std::fs::canonicalize(cur) {
            Ok(real) => {
                let mut out = real;
                for part in missing.iter().rev() {
                    out.push(part);
                }
                return Some(out);
            }
            // Only a missing component may be skipped; any other error
            // (permissions, I/O) leaves the real path unknown.
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return None,
            Err(_) => {}
        }
        // A component that exists but does not resolve is a dangling
        // symlink: its real repository is unknown, so fail closed.
        if std::fs::symlink_metadata(cur).is_ok() {
            return None;
        }
        missing.push(cur.file_name()?.to_os_string());
        cur = cur.parent()?;
    }
}

/// Map paths to their worktree roots (the nearest ancestor holding `.git`
/// on the *resolved* path, so that a symlink into another repository is
/// attributed to that repository), caching every visited directory.
#[derive(Default)]
pub struct RootCache {
    map: Mutex<HashMap<PathBuf, Option<PathBuf>>>,
}

impl RootCache {
    /// The worktree root of `path` and the resolved path itself.
    pub fn locate(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        if !path.is_absolute()
            || ["/proc", "/dev", "/sys"]
                .iter()
                .any(|p| path.starts_with(p))
        {
            return None;
        }
        let real = real_path(path)?;
        let start = if real.is_dir() {
            real.as_path()
        } else {
            real.parent()?
        };
        let mut visited = Vec::new();
        let mut cur = Some(start);
        let found = loop {
            let Some(dir) = cur else { break None };
            if let Some(hit) = self.map.lock().unwrap().get(dir) {
                break hit.clone();
            }
            visited.push(dir.to_path_buf());
            if dir.join(".git").exists() {
                break Some(dir.to_path_buf());
            }
            cur = dir.parent();
        };
        let mut map = self.map.lock().unwrap();
        for v in visited {
            map.insert(v, found.clone());
        }
        drop(map);
        found.map(|root| (root, real))
    }

    /// The worktree root of `path`.
    pub fn root_of(&self, path: &Path) -> Option<PathBuf> {
        self.locate(path).map(|(root, _)| root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Convert `key value` lines to `git config -z` records.
    fn z(lines: &str) -> String {
        lines
            .lines()
            .filter_map(|l| l.trim().split_once(' '))
            .map(|(k, v)| format!("{k}\n{v}\0"))
            .collect()
    }

    #[test]
    fn newline_in_a_value_cannot_forge_a_record() {
        let out =
            "remote.a.url\nhttps://github.com/x/y.git\nremote.b.url https://github.com/evil/z\0";
        let (remotes, _) = parse_config(out);
        assert!(!remotes.contains_key("b"));
    }

    #[test]
    fn reflog_line() {
        let l = "0a1b2c3da68a131e7cea4062647b4d32fd39290b abc12344e621c24e81a87a191c2b2fab51bdd1f0 alice <r@ecopoiesis.org> 1700000123 +0000\tcommit: fix(parser): x";
        let m = parse_reflog_line(l).unwrap();
        assert_eq!(m.new_oid, "abc12344e621c24e81a87a191c2b2fab51bdd1f0");
        assert_eq!(m.at_ms, 1_700_000_123_000);
        assert_eq!(m.message, "commit: fix(parser): x");
        assert!(parse_reflog_line("garbage").is_none());
    }

    #[test]
    fn config_parsing() {
        let out = "remote.upstream.url https://github.com/upstream/project.git\n\
                   remote.fork.url git@github.com:alice/project.git\n\
                   remote.local.url /srv/x.git\n\
                   branch.pr-local-name.remote fork\n\
                   branch.pr-local-name.merge refs/heads/pr-pushed-name\n\
                   branch.loc.remote .\n\
                   branch.loc.merge refs/heads/main\n";
        let (remotes, tracking) = parse_config(&z(out));
        assert_eq!(remotes["fork"].as_str(), "alice/project");
        assert!(!remotes.contains_key("local"));
        assert_eq!(
            tracking["pr-local-name"],
            ("fork".to_string(), "refs/heads/pr-pushed-name".to_string())
        );
        assert!(!tracking.contains_key("loc"));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn describes_worktree_with_fork_tracking() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "root"]);
        git(
            &main,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/upstream/project.git",
            ],
        );
        git(
            &main,
            &[
                "remote",
                "add",
                "fork",
                "git@github.com:alice/project.git",
            ],
        );
        git(&main, &["tag", "v1"]);
        let wt = tmp.path().join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "local-name",
                wt.to_str().unwrap(),
            ],
        );
        git(&wt, &["config", "branch.local-name.remote", "fork"]);
        git(
            &wt,
            &[
                "config",
                "branch.local-name.merge",
                "refs/heads/pushed-name",
            ],
        );
        git(&wt, &["commit", "-q", "--allow-empty", "-m", "work"]);

        let w = describe(&wt).unwrap();
        assert_eq!(w.branch.as_deref(), Some("local-name"));
        assert!(w.git_dir.ends_with("worktrees/wt"));
        assert_eq!(
            w.common_dir,
            main.join(".git")
                .canonicalize()
                .unwrap_or(main.join(".git"))
        );
        let (slug, b) = w.upstream.clone().unwrap();
        assert_eq!(
            (slug.as_str(), b.as_str()),
            ("alice/project", "pushed-name")
        );
        let mv = w.last_move.clone().unwrap();
        assert_eq!(Some(mv.new_oid.clone()), w.head_oid);
        assert!(mv.message.starts_with("commit"));
        let base = RepoSlug::parse("upstream/project").unwrap();
        assert!(w.belongs_to(&base, None));
        assert!(!w.belongs_to(&RepoSlug::parse("x/y").unwrap(), None));

        let tags = tags_named(&w.common_dir, &["v1".into(), "pushed-name".into()]);
        assert!(tags.is_tag("v1") && !tags.is_tag("pushed-name"));
        assert_eq!(
            push_slug(&wt, "fork").unwrap().as_str(),
            "alice/project"
        );
        assert!(push_slug(&wt, "missing").is_none());

        let cache = RootCache::default();
        std::fs::create_dir_all(wt.join("src/deep")).unwrap();
        assert_eq!(
            cache.root_of(&wt.join("src/deep/file.rs")),
            Some(wt.clone())
        );
        assert_eq!(cache.root_of(&wt.join("src/deep")), Some(wt.clone()));
        assert_eq!(cache.root_of(&main.join("x")), Some(main.clone()));
        assert_eq!(cache.root_of(Path::new("/proc/self")), None);
    }

    #[test]
    fn config_first_url_rewrites_and_ambiguous_merge() {
        let out = "remote.r.url gh:alice/project\n\
                   remote.r.url https://github.com/other/x.git\n\
                   url.https://github.com/.insteadof gh:\n\
                   branch.b.remote r\n\
                   branch.b.merge refs/heads/one\n\
                   branch.b.merge refs/heads/two\n";
        let (remotes, tracking) = parse_config(&z(out));
        assert_eq!(remotes["r"].as_str(), "alice/project");
        assert!(!tracking.contains_key("b"), "two merge values: no upstream");
    }

    #[test]
    fn partial_reflog_record_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("HEAD");
        let full = "0000000000000000000000000000000000000000 abc12344e621c24e81a87a191c2b2fab51bdd1f0 a <a@b> 1700000000 +0000\tcommit: x\n";
        std::fs::write(&p, format!("{full}0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 a <a@b> 1800000000")).unwrap();
        assert!(read_last_reflog(&p).is_none());
        std::fs::write(&p, full).unwrap();
        assert_eq!(read_last_reflog(&p).unwrap().at_ms, 1_700_000_000_000);
    }

    #[test]
    fn symlink_into_another_repo_is_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(b.join("src")).unwrap();
        std::fs::create_dir_all(b.join(".git")).unwrap();
        std::os::unix::fs::symlink(b.join("src"), a.join("vendor")).unwrap();
        let cache = RootCache::default();
        let (root, real) = cache.locate(&a.join("vendor/gone.rs")).unwrap();
        assert_eq!(root, b.canonicalize().unwrap());
        assert_eq!(real, b.canonicalize().unwrap().join("src/gone.rs"));
    }

    #[test]
    fn dangling_symlink_and_rewrite_ties_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), a.join("vendor")).unwrap();
        assert_eq!(RootCache::default().locate(&a.join("vendor/f.rs")), None);
        let out = "remote.r.url gh:x/y\n\
                   url.https://github.com/.insteadof gh:\n\
                   url.https://example.com/.insteadof gh:\n";
        assert!(parse_config(&z(out)).0.is_empty());
    }

    #[test]
    fn run_git_reports_exit_status_and_output() {
        assert!(run_git(&["--version"]).unwrap().starts_with("git version"));
        assert!(run_git(&["no-such-subcommand-xyz"]).is_none());
    }

    #[test]
    fn missing_reflog_and_failed_tags() {
        assert!(read_last_reflog(Path::new("/nonexistent/logs/HEAD")).is_none());
        assert_eq!(
            tags_named(Path::new("/nonexistent"), &["a".into()]),
            TagCheck::Failed
        );
        assert!(TagCheck::Failed.is_tag("anything"));
    }
}
