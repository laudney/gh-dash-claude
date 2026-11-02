//! The single-pass evidence scanner over a transcript tail.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::gitinfo::RootCache;
use crate::keys::KeySet;
use crate::registry::Session;
use crate::shellparse::{self, GitCall, MoveVerb};
use crate::slug::RepoSlug;
use crate::time::parse_rfc3339_ms;

/// Result text kept per call, at most.
const MAX_RESULT_TEXT: usize = 64 * 1024;
/// Claude Code shortens `Bash` output near 30 000 characters; text at least
/// this long may be incomplete.
const POSSIBLY_SHORTENED: usize = 29_000;

/// A call's start (its `tool_use` entry) and end (its result entry), in
/// epoch milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: i64,
    pub end: i64,
}

/// A git invocation that may have moved HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveCandidate {
    pub verb: MoveVerb,
    pub span: Span,
    /// The oids (7 hex) in the verb's own report lines.
    pub report_oids: Vec<String>,
}

/// What a session did in one worktree root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootTouch {
    pub work: u32,
    pub reference: u32,
    /// Verified edits and verified HEAD-moving git work.
    pub work_spans: Vec<Span>,
    pub moves: Vec<MoveCandidate>,
}

/// Where a publication went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestRepo {
    /// From the push output's `To <url>` line.
    Url(RepoSlug),
    /// No `To` line: the remote name, resolved later through
    /// `git remote get-url --push`.
    Remote(String),
}

/// A push verified by its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub root: PathBuf,
    pub dest_repo: DestRepo,
    /// Destination refs the output reports as updated (not rejected, not a
    /// new tag).
    pub dest_refs: Vec<String>,
    /// The source ref of each destination, as the output names it; the tag
    /// check covers both names.
    pub sources: Vec<String>,
    pub span: Span,
}

/// All evidence of one session.
#[derive(Debug, Clone)]
pub struct SessionEvidence {
    pub session: Session,
    pub roots: HashMap<PathBuf, RootTouch>,
    pub publications: Vec<Publication>,
    /// Root → paths (relative to the root) changed by verified edits.
    pub edited: HashMap<PathBuf, HashSet<String>>,
    /// Mention counts, indexed by key id.
    pub mentions: Vec<u32>,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
}

impl SessionEvidence {
    /// The best title: the custom one, else the AI one.
    pub fn title(&self) -> Option<&str> {
        self.custom_title.as_deref().or(self.ai_title.as_deref())
    }
}

/// Read the last `tail` bytes of a transcript, dropping a partial first
/// line. A missing file gives an empty buffer.
pub fn read_tail(path: &Path, tail: u64) -> Vec<u8> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(tail);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    if start > 0 {
        match memchr::memchr(b'\n', &buf) {
            Some(i) => {
                buf.drain(..=i);
            }
            None => buf.clear(),
        }
    }
    buf
}

#[derive(Deserialize)]
struct LineHead<'a> {
    #[serde(default, rename = "type", borrow)]
    kind: Option<&'a str>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, borrow)]
    message: Option<&'a RawValue>,
    #[serde(default, rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    content: &'a RawValue,
}

/// A content block of an assistant message. A plain struct, not a tagged
/// enum, so that fields can be raw values.
#[derive(Deserialize)]
struct Block {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ResultBlock<'a> {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default, borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct TextBlock {
    #[serde(default)]
    text: Option<String>,
}

struct Call {
    id: String,
    name: String,
    input: serde_json::Value,
    start: i64,
    git: Vec<GitCall>,
}

struct CallResult {
    ok: bool,
    /// The result only acknowledges a background launch.
    background: bool,
    end: i64,
    text: Option<String>,
    /// Facts about the full text, taken before truncation.
    full: Option<FullText>,
}

/// Facts taken from the complete result text.
struct FullText {
    /// The text may have been shortened by Claude Code.
    maybe_shortened: bool,
    /// A `cd` the call's directory relies on reported a failure.
    cd_failed: bool,
}

const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

static ABS_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[\s"'=:(])(/[^\s"'`;|&<>()\\]+)"#).unwrap());
static TILDE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[\s"'=:(])(~/[^\s"'`;|&<>()\\]+)"#).unwrap());
static RE_TO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*To (\S+)\s*$").unwrap());
static RE_PUSH_OK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*(?:[+*=-]\s+)?(?:[0-9a-f]{7,}\.\.\.?[0-9a-f]{7,}|\[new branch\]|\[new reference\])\s+(\S+)\s+->\s+(\S+)",
    )
    .unwrap()
});
/// `git push --porcelain` records: `<flag>\t<from>:<to>\t<summary>`.
static RE_PORCELAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^([ +*!=-])\t([^\t:]*):([^\t\n]+)\t").unwrap());
static RE_NEW_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*\*\s+\[new tag\]\s+\S+\s+->\s+(\S+)").unwrap());
static RE_REJECTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*!\s+\[(?:remote )?rejected\]\s+\S+\s+->\s+(\S+)").unwrap()
});
static RE_COMMIT_REPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\[[^\]\n]*? ([0-9a-f]{7,})\]").unwrap());
static RE_HEAD_NOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^HEAD is now at ([0-9a-f]{7,})").unwrap());
static RE_UPDATING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Updating [0-9a-f]{7,}\.\.([0-9a-f]{7,})").unwrap());

/// The `To` line of a push result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToLine {
    Absent,
    GitHub(RepoSlug),
    Other,
}

/// What a push result says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub to: ToLine,
    /// Updated destination → its source.
    pub ok: std::collections::HashMap<String, String>,
    pub rejected: HashSet<String>,
    pub new_tags: HashSet<String>,
}

fn short_ref(r: &str) -> String {
    r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
}

/// Parse a push result text.
pub fn push_outcome(text: &str) -> PushOutcome {
    let tos: HashSet<String> = RE_TO
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect();
    let to = match tos.len() {
        0 => ToLine::Absent,
        1 => match RepoSlug::from_url(tos.iter().next().unwrap()) {
            Some(s) => ToLine::GitHub(s),
            None => ToLine::Other,
        },
        // Several destinations: ref outcomes cannot be tied to one.
        _ => ToLine::Other,
    };
    let collect = |re: &Regex| -> HashSet<String> {
        re.captures_iter(text).map(|c| short_ref(&c[1])).collect()
    };
    let mut out = PushOutcome {
        to,
        ok: std::collections::HashMap::new(),
        rejected: collect(&RE_REJECTED),
        new_tags: collect(&RE_NEW_TAG),
    };
    let is_tag = |r: &str| r.starts_with("refs/tags/");
    for c in RE_PUSH_OK.captures_iter(text) {
        let (src, dst) = (&c[1], &c[2]);
        if is_tag(src) || is_tag(dst) {
            out.new_tags.insert(short_ref(dst));
        } else {
            out.ok.insert(short_ref(dst), short_ref(src));
        }
    }
    for c in RE_PORCELAIN.captures_iter(text) {
        let (flag, src, dst) = (&c[1], &c[2], &c[3]);
        match flag {
            "!" => {
                out.rejected.insert(short_ref(dst));
            }
            "-" | "=" => {}
            _ if is_tag(src) || is_tag(dst) => {
                out.new_tags.insert(short_ref(dst));
            }
            _ if dst.starts_with("refs/") && !dst.starts_with("refs/heads/") => {}
            _ => {
                out.ok.insert(short_ref(dst), short_ref(src));
            }
        }
    }
    out
}

/// The oids a move verb reports in its own output.
pub fn report_oids(verb: MoveVerb, text: &str) -> Vec<String> {
    let re: &Regex = match verb {
        MoveVerb::Commit | MoveVerb::CherryPick => &RE_COMMIT_REPORT,
        MoveVerb::Reset | MoveVerb::Checkout => &RE_HEAD_NOW,
        MoveVerb::Merge => &RE_UPDATING,
        MoveVerb::Rebase | MoveVerb::Am => return Vec::new(),
    };
    re.captures_iter(text)
        .map(|c| c[1][..7].to_string())
        .collect()
}

/// Git subcommands that never print a HEAD-move report line.
const QUIET_GIT: &[&str] = &[
    "log",
    "status",
    "diff",
    "show",
    "rev-parse",
    "rev-list",
    "branch",
    "push",
    "fetch",
    "remote",
    "config",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "add",
    "rm",
    "mv",
    "describe",
    "blame",
    "grep",
    "shortlog",
    "for-each-ref",
    "cat-file",
    "merge-base",
    "cherry",
    "range-diff",
    "symbolic-ref",
    "name-rev",
    "var",
    "version",
    "help",
    "count-objects",
    "check-ignore",
    "check-attr",
    "diff-tree",
    "diff-index",
    "diff-files",
    "show-ref",
];

/// Whether invocation `g` may print the report line of `verb`.
/// Unknown subcommands (aliases included) may print anything.
fn may_report(g: &GitCall, verb: MoveVerb) -> bool {
    // A custom format can print any text, including a report line.
    let custom_format = g.args.iter().any(|a| {
        a.starts_with("--format") || a.starts_with("--pretty") || a.starts_with("format:")
    });
    if QUIET_GIT.contains(&g.sub.as_str()) && !custom_format {
        return false;
    }
    let known: &[&str] = match verb {
        MoveVerb::Commit | MoveVerb::CherryPick => &[
            "commit",
            "cherry-pick",
            "revert",
            "merge",
            "am",
            "rebase",
            "pull",
        ],
        MoveVerb::Reset | MoveVerb::Checkout => &[
            "reset", "checkout", "switch", "worktree", "stash", "rebase", "pull", "bisect",
        ],
        MoveVerb::Merge => &["merge", "pull"],
        MoveVerb::Rebase | MoveVerb::Am => return true,
    };
    const ALL_KNOWN: &[&str] = &[
        "commit",
        "cherry-pick",
        "revert",
        "merge",
        "am",
        "rebase",
        "pull",
        "reset",
        "checkout",
        "switch",
        "worktree",
        "stash",
        "bisect",
        "tag",
        "notes",
        "gc",
        "clone",
        "init",
        "apply",
        "restore",
        "clean",
        "submodule",
        "sparse-checkout",
        "format-patch",
        "bundle",
        "archive",
        "maintenance",
        "prune",
        "repack",
        "fsck",
        "reflog",
        "update-ref",
        "update-index",
        "read-tree",
        "write-tree",
        "commit-tree",
        "hash-object",
        "mktree",
    ];
    known.contains(&g.sub.as_str()) || !ALL_KNOWN.contains(&g.sub.as_str())
}

/// Text of a `tool_result` content value (a string, or a list of text
/// blocks joined with newlines), truncated, plus the facts taken from the
/// complete text.
fn result_text(raw: &RawValue, cd_ops: &[String]) -> Option<(String, FullText)> {
    let mut text = if let Ok(s) = serde_json::from_str::<String>(raw.get()) {
        s
    } else {
        let blocks: Vec<TextBlock> = serde_json::from_str(raw.get()).ok()?;
        blocks
            .into_iter()
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let full = FullText {
        maybe_shortened: text.chars().count() >= POSSIBLY_SHORTENED,
        // Match each operand literally: one that contains `: ` still matches.
        cd_failed: cd_ops
            .iter()
            .any(|op| text.contains(&format!("cd: {op}: "))),
    };
    if text.len() > MAX_RESULT_TEXT {
        let mut cut = MAX_RESULT_TEXT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Some((text, full))
}

/// Scan one session's transcript tail.
pub fn scan(
    session: &Session,
    buf: &[u8],
    keys: &KeySet,
    home: &Path,
    roots: &RootCache,
) -> SessionEvidence {
    let mut ev = SessionEvidence {
        session: session.clone(),
        roots: HashMap::new(),
        publications: Vec::new(),
        edited: HashMap::new(),
        mentions: keys.count(buf),
        custom_title: None,
        ai_title: None,
    };

    let mut calls: Vec<Call> = Vec::new();
    let mut want_text: HashSet<String> = HashSet::new();
    // Call id → operands of the sticky `cd`s its git invocations rely on.
    let mut cd_ops: HashMap<String, Vec<String>> = HashMap::new();
    let mut results: HashMap<String, CallResult> = HashMap::new();

    for line in buf.split(|&b| b == b'\n') {
        let is_use = memchr::memmem::find(line, b"\"tool_use\"").is_some();
        let is_result = memchr::memmem::find(line, b"\"tool_result\"").is_some();
        let is_title = memchr::memmem::find(line, b"-title\"").is_some();
        if !is_use && !is_result && !is_title {
            continue;
        }
        let Ok(head) = serde_json::from_slice::<LineHead>(line) else {
            continue;
        };
        match head.kind {
            Some("custom-title") => {
                if let Some(t) = head.custom_title.or(head.title) {
                    ev.custom_title = Some(t);
                }
                continue;
            }
            Some("ai-title") => {
                if let Some(t) = head.ai_title.or(head.title) {
                    ev.ai_title = Some(t);
                }
                continue;
            }
            _ => {}
        }
        let Some(ts) = head.timestamp.as_deref().and_then(parse_rfc3339_ms) else {
            continue;
        };
        let Some(msg) = head.message else { continue };
        let Ok(msg) = serde_json::from_str::<Message>(msg.get()) else {
            continue;
        };
        if head.kind == Some("assistant") && is_use {
            let Ok(blocks) = serde_json::from_str::<Vec<Block>>(msg.content.get()) else {
                continue;
            };
            for b in blocks {
                if b.kind != "tool_use" {
                    continue;
                }
                let (Some(id), Some(name), Some(input)) = (b.id, b.name, b.input) else {
                    continue;
                };
                let git = if name == "Bash" {
                    input
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| shellparse::git_invocations(c, home))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if git
                    .iter()
                    .any(|g| g.sub == "push" || g.move_verb().is_some() || !g.assumes_cd.is_empty())
                {
                    want_text.insert(id.clone());
                    let ops: Vec<String> = git
                        .iter()
                        .flat_map(|g| g.assumes_cd.iter().cloned())
                        .collect();
                    if !ops.is_empty() {
                        cd_ops.insert(id.clone(), ops);
                    }
                }
                calls.push(Call {
                    id,
                    name,
                    input,
                    start: ts,
                    git,
                });
            }
        } else if is_result {
            // Claude Code marks a launch (explicit or automatic) with a
            // `backgroundTaskId`; such a result is not a completion.
            let is_background = memchr::memmem::find(line, b"\"backgroundTaskId\"").is_some();
            let Ok(blocks) = serde_json::from_str::<Vec<ResultBlock>>(msg.content.get()) else {
                continue;
            };
            for b in blocks {
                if b.kind != "tool_result" {
                    continue;
                }
                let Some(id) = b.tool_use_id else { continue };
                let (text, full) = match want_text.contains(&id) {
                    true => match b.content.and_then(|c| {
                        result_text(c, cd_ops.get(&id).map_or(&[][..], Vec::as_slice))
                    }) {
                        Some((t, f)) => (Some(t), Some(f)),
                        None => (None, None),
                    },
                    false => (None, None),
                };
                results.insert(
                    id,
                    CallResult {
                        ok: !b.is_error,
                        background: is_background,
                        end: ts,
                        text,
                        full,
                    },
                );
            }
        }
    }

    for call in &calls {
        classify(&mut ev, call, results.get(&call.id), home, roots);
    }
    if let Some(root) = roots.root_of(&session.cwd) {
        ev.roots.entry(root).or_default().reference += 1;
    }
    ev
}

/// Whether a sticky `cd` this invocation relies on may have failed: bash
/// prints `cd: <dir>: <reason>` for every failed `cd`, and the `Bash` tool
/// captures stderr. Without the output, fail closed.
fn sticky_cd_failed(g: &GitCall, res: Option<&CallResult>) -> bool {
    if g.assumes_cd.is_empty() {
        return false;
    }
    match res.and_then(|r| r.full.as_ref()) {
        None => true,
        Some(full) => full.maybe_shortened || full.cd_failed,
    }
}

fn reference(ev: &mut SessionEvidence, root: Option<PathBuf>) {
    if let Some(r) = root {
        ev.roots.entry(r).or_default().reference += 1;
    }
}

fn classify(
    ev: &mut SessionEvidence,
    call: &Call,
    res: Option<&CallResult>,
    home: &Path,
    roots: &RootCache,
) {
    // A background launch returns success before the command has run: its
    // result proves nothing about the command.
    let background = call
        .input
        .get("run_in_background")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let res = if background || res.is_some_and(|r| r.background) {
        None
    } else {
        res
    };
    let ok = res.is_some_and(|r| r.ok);
    let span = res.map(|r| Span {
        start: call.start,
        end: r.end,
    });
    let mut evidence_paths: HashSet<PathBuf> = HashSet::new();

    if EDIT_TOOLS.contains(&call.name.as_str()) {
        let path = call
            .input
            .get("file_path")
            .or_else(|| call.input.get("notebook_path"))
            .and_then(|p| p.as_str())
            .map(PathBuf::from);
        if let Some(path) = path.filter(|p| p.is_absolute()) {
            evidence_paths.insert(path.clone());
            if let Some((root, real)) = roots.locate(&path) {
                if let (true, Some(span)) = (ok, span) {
                    let t = ev.roots.entry(root.clone()).or_default();
                    t.work += 1;
                    t.work_spans.push(span);
                    if let Ok(rel) = real.strip_prefix(&root) {
                        ev.edited
                            .entry(root)
                            .or_default()
                            .insert(rel.to_string_lossy().into_owned());
                    }
                } else {
                    reference(ev, Some(root));
                }
            }
        }
    }

    let push_count = call.git.iter().filter(|g| g.sub == "push").count();
    for g in &call.git {
        let root = g.dir.as_deref().and_then(|d| roots.root_of(d));
        if let Some(d) = &g.dir {
            evidence_paths.insert(d.clone());
        }
        if g.opaque || root.is_none() || sticky_cd_failed(g, res) {
            reference(ev, root);
            continue;
        }
        let root = root.unwrap();
        let mut credited = false;
        if let (Some(_), true, true, Some(span)) = (g.work_verb(), ok, g.chain_verified, span) {
            let t = ev.roots.entry(root.clone()).or_default();
            t.work += 1;
            t.work_spans.push(span);
            credited = true;
        }
        if let (Some(verb), Some(r), Some(span)) = (g.move_verb(), res, span) {
            let producers = call.git.iter().filter(|o| may_report(o, verb)).count();
            let oids = match (producers, r.text.as_deref()) {
                (1, Some(t)) => report_oids(verb, t),
                _ => Vec::new(),
            };
            ev.roots
                .entry(root.clone())
                .or_default()
                .moves
                .push(MoveCandidate {
                    verb,
                    span,
                    report_oids: oids,
                });
            // A candidate alone earns nothing; the call still gives a reference.
        }
        if let (Some(p), 1, Some(r), Some(span)) = (g.push(), push_count, res, span)
            && !p.dry_run
            && !p.delete
            && !p.bulk
            && !p.unsupported
            && !p.dests.is_empty()
            && let Some(remote) = p.remote.clone()
            && let Some(text) = r.text.as_deref()
        {
            let out = push_outcome(text);
            let (dest_refs, sources): (Vec<String>, Vec<String>) = p
                .dests
                .iter()
                .filter(|d| !out.rejected.contains(*d) && !out.new_tags.contains(*d))
                .filter_map(|d| Some((d.clone(), out.ok.get(d)?.clone())))
                .unzip();
            let dest_repo = match out.to {
                ToLine::GitHub(s) => Some(DestRepo::Url(s)),
                ToLine::Absent => Some(DestRepo::Remote(remote)),
                ToLine::Other => None,
            };
            if let (Some(dest_repo), false) = (dest_repo, dest_refs.is_empty()) {
                ev.publications.push(Publication {
                    root: root.clone(),
                    dest_repo,
                    dest_refs,
                    sources,
                    span,
                });
                credited = true;
            }
        }
        if !credited {
            reference(ev, Some(root));
        }
    }

    // Every other path mentioned by the call is a reference.
    let input_text = call.input.to_string();
    let mut others: Vec<PathBuf> = ABS_PATH
        .captures_iter(&input_text)
        .map(|c| PathBuf::from(&c[1]))
        .collect();
    if call.name == "Bash"
        && let Some(cmd) = call.input.get("command").and_then(|c| c.as_str())
    {
        others.extend(
            TILDE_PATH
                .captures_iter(cmd)
                .filter_map(|c| crate::paths::expand_tilde(&c[1], home)),
        );
    }
    let mut seen_roots: HashSet<PathBuf> = HashSet::new();
    for p in others {
        if evidence_paths.contains(&p) {
            continue;
        }
        if let Some(root) = roots.root_of(&p)
            && seen_roots.insert(root.clone())
        {
            reference(ev, Some(root));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeySpec;

    fn session(cwd: &Path) -> Session {
        Session {
            pid: 42,
            session_id: "s".into(),
            cwd: cwd.to_path_buf(),
            name: "n".into(),
            name_source: "derived".into(),
            kind: "interactive".into(),
            status: "idle".into(),
            proc_start: "1".into(),
            socket: None,
            peer_protocol: 1,
        }
    }

    fn use_line(ts: &str, id: &str, name: &str, input: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": ts,
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "x"},
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]}
        })
        .to_string()
    }

    fn result_line(
        ts: &str,
        id: &str,
        is_error: Option<bool>,
        content: serde_json::Value,
    ) -> String {
        let mut b =
            serde_json::json!({"type": "tool_result", "tool_use_id": id, "content": content});
        if let Some(e) = is_error {
            b["is_error"] = e.into();
        }
        serde_json::json!({
            "type": "user", "timestamp": ts,
            "message": {"role": "user", "content": [b]},
            "toolUseResult": {"stdout": "huge"}
        })
        .to_string()
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        main: PathBuf,
        wt: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt");
        for d in [&main, &wt] {
            std::fs::create_dir_all(d.join("src")).unwrap();
            std::fs::write(d.join(".git"), "gitdir: x").unwrap();
        }
        Fixture {
            _tmp: tmp,
            main,
            wt,
        }
    }

    #[test]
    fn verified_edit_failed_edit_and_titles() {
        let f = fixture();
        let file = f.wt.join("src/a.rs");
        let lines = [
            "{\"partial line".to_string(),
            serde_json::json!({"type": "ai-title", "aiTitle": "First"}).to_string(),
            use_line(
                "2026-11-02T10:00:00Z",
                "t1",
                "Edit",
                serde_json::json!({"file_path": file}),
            ),
            result_line("2026-11-02T10:00:01Z", "t1", None, "ok".into()),
            use_line(
                "2026-11-02T10:01:00Z",
                "t2",
                "Edit",
                serde_json::json!({"file_path": f.wt.join("src/b.rs")}),
            ),
            result_line("2026-11-02T10:01:01Z", "t2", Some(true), "error".into()),
            use_line(
                "2026-11-02T10:02:00Z",
                "t3",
                "Edit",
                serde_json::json!({"file_path": f.wt.join("src/c.rs")}),
            ),
            serde_json::json!({"type": "custom-title", "customTitle": "Mine"}).to_string(),
            serde_json::json!({"type": "ai-title", "aiTitle": "Later"}).to_string(),
        ];
        let buf = lines.join("\n");
        let ev = scan(
            &session(&f.main),
            buf.as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        let t = &ev.roots[&f.wt];
        assert_eq!(t.work, 1);
        assert_eq!(
            t.reference, 2,
            "failed and missing-result edits are references"
        );
        assert_eq!(t.work_spans[0].end - t.work_spans[0].start, 1000);
        assert_eq!(ev.edited[&f.wt], HashSet::from(["src/a.rs".to_string()]));
        assert_eq!(ev.title(), Some("Mine"));
        assert_eq!(ev.ai_title.as_deref(), Some("Later"));
        assert_eq!(ev.roots[&f.main].reference, 1, "session cwd");
    }

    #[test]
    fn piped_push_is_verified_by_output() {
        let f = fixture();
        let cmd = format!(
            "cd {} && git push fork pr-topic 2>&1 | tail -3",
            f.wt.display()
        );
        let out = "To github.com:alice/project.git\n   0a1b2c3da..abc12344e  pr-topic -> pr-topic";
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "p1",
                "Bash",
                serde_json::json!({"command": cmd}),
            ),
            result_line("2026-11-02T10:00:05Z", "p1", Some(false), out.into()),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        assert_eq!(ev.publications.len(), 1);
        let p = &ev.publications[0];
        assert_eq!(p.root, f.wt);
        assert_eq!(p.dest_refs, vec!["pr-topic"]);
        assert_eq!(
            p.dest_repo,
            DestRepo::Url(RepoSlug::parse("alice/project").unwrap())
        );
    }

    #[test]
    fn push_without_to_line_two_pushes_and_rejections() {
        let f = fixture();
        let w = f.wt.display().to_string();
        let one = format!("cd {w} && git push fork topic 2>&1 | tail -1");
        let two = format!("cd {w} && git push fork a; git push fork b");
        let rej = format!("cd {w} && git push fork topic");
        let list_text =
            serde_json::json!([{"type": "text", "text": "   1111111..2222222  topic -> topic"}]);
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "a",
                "Bash",
                serde_json::json!({"command": one}),
            ),
            result_line("2026-11-02T10:00:01Z", "a", None, list_text),
            use_line(
                "2026-11-02T10:01:00Z",
                "b",
                "Bash",
                serde_json::json!({"command": two}),
            ),
            result_line(
                "2026-11-02T10:01:01Z",
                "b",
                None,
                "   1111111..2222222  a -> a\n   1111111..2222222  b -> b".into(),
            ),
            use_line(
                "2026-11-02T10:02:00Z",
                "c",
                "Bash",
                serde_json::json!({"command": rej}),
            ),
            result_line(
                "2026-11-02T10:02:01Z",
                "c",
                Some(true),
                "To github.com:x/y.git\n ! [rejected]        topic -> topic (non-fast-forward)"
                    .into(),
            ),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        assert_eq!(ev.publications.len(), 1, "{:?}", ev.publications);
        assert_eq!(
            ev.publications[0].dest_repo,
            DestRepo::Remote("fork".into())
        );
    }

    #[test]
    fn commit_move_candidate_with_report_oid() {
        let f = fixture();
        let cmd = format!(
            "cd {} && git commit -q -m \"$(cat <<'EOF'\nfix: x\nEOF\n)\" 2>&1 | tail -1 && git log --oneline -1",
            f.wt.display()
        );
        let out = "[pr-topic abc1234] fix: x\n 1 file changed\n9999999 other";
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "c1",
                "Bash",
                serde_json::json!({"command": cmd}),
            ),
            result_line("2026-11-02T10:00:09Z", "c1", None, out.into()),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        let t = &ev.roots[&f.wt];
        assert!(
            t.work_spans.is_empty(),
            "piped commit is not chain-verified"
        );
        assert_eq!(t.moves.len(), 1);
        assert_eq!(t.moves[0].report_oids, vec!["abc1234"]);
    }

    #[test]
    fn failed_sticky_cd_in_output_makes_the_commit_opaque() {
        let f = fixture();
        let cmd = format!(
            "cd {m}; cd {w}; git commit -m x 2>&1 | tail -1",
            m = f.main.display(),
            w = f.wt.display()
        );
        let ok_out = "[topic abc1234] x";
        let bad_out = format!(
            "bash: line 1: cd: {}: No such file or directory\n[main abc1234] x",
            f.wt.display()
        );
        for (out, want_wt_move) in [(ok_out.to_string(), true), (bad_out, false)] {
            let lines = [
                use_line(
                    "2026-11-02T10:00:00Z",
                    "c",
                    "Bash",
                    serde_json::json!({"command": cmd}),
                ),
                result_line("2026-11-02T10:00:01Z", "c", None, out.clone().into()),
            ];
            let ev = scan(
                &session(&f.main),
                lines.join("\n").as_bytes(),
                &KeySet::empty(),
                Path::new("/h"),
                &RootCache::default(),
            );
            let moves = ev.roots.get(&f.wt).map_or(0, |t| t.moves.len());
            assert_eq!(moves == 1, want_wt_move, "{out}");
        }
    }

    #[test]
    fn long_output_fails_closed_for_sticky_cd() {
        let f = fixture();
        let cmd = format!("cd {w}; git commit -m x 2>&1 | tail -1", w = f.wt.display());
        let out = format!("{}\n[t abc1234] x", "x".repeat(30_000));
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "c",
                "Bash",
                serde_json::json!({"command": cmd}),
            ),
            result_line("2026-11-02T10:00:01Z", "c", None, out.into()),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        assert_eq!(ev.roots.get(&f.wt).map_or(0, |t| t.moves.len()), 0);
    }

    #[test]
    fn background_launch_and_ambiguous_reports_earn_nothing() {
        let f = fixture();
        let w = f.wt.display().to_string();
        let bg = format!("cd {w} && git commit -m x");
        let two = format!("git -C {w} reset --hard bad; git -C {w} reset --hard HEAD");
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "b",
                "Bash",
                serde_json::json!({"command": bg, "run_in_background": true}),
            ),
            result_line(
                "2026-11-02T10:00:01Z",
                "b",
                Some(false),
                "Command running in background with ID: x".into(),
            ),
            use_line(
                "2026-11-02T10:01:00Z",
                "t",
                "Bash",
                serde_json::json!({"command": two}),
            ),
            result_line(
                "2026-11-02T10:01:01Z",
                "t",
                None,
                "HEAD is now at abc1234 x".into(),
            ),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        let t = &ev.roots[&f.wt];
        let bg_start = crate::time::parse_rfc3339_ms("2026-11-02T10:00:00Z").unwrap();
        assert!(
            t.work_spans.iter().all(|sp| sp.start != bg_start),
            "background commit is not verified"
        );
        assert!(
            t.moves.iter().all(|m| m.report_oids.is_empty()),
            "two resets: ambiguous"
        );
        assert!(t.reference >= 2);
    }

    #[test]
    fn automatic_background_and_other_report_producers() {
        let f = fixture();
        let w = f.wt.display().to_string();
        let cmd = format!("cd {w} && git commit -m x");
        let mut auto_bg: serde_json::Value = serde_json::from_str(&result_line(
            "2026-11-02T10:00:01Z",
            "a",
            Some(false),
            "Command was moved to the background".into(),
        ))
        .unwrap();
        auto_bg["toolUseResult"]["backgroundTaskId"] = "bg1".into();
        let mixed = format!("git -C {w} worktree add /tmp/x b; git -C {w} reset --hard bad");
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "a",
                "Bash",
                serde_json::json!({"command": cmd}),
            ),
            auto_bg.to_string(),
            use_line(
                "2026-11-02T10:01:00Z",
                "m",
                "Bash",
                serde_json::json!({"command": mixed}),
            ),
            result_line(
                "2026-11-02T10:01:01Z",
                "m",
                None,
                "HEAD is now at abc1234 x".into(),
            ),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        let t = &ev.roots[&f.wt];
        let bg = crate::time::parse_rfc3339_ms("2026-11-02T10:00:00Z").unwrap();
        assert!(
            t.work_spans.iter().all(|sp| sp.start != bg),
            "auto-background is not a completion"
        );
        assert!(t.moves.iter().all(|m| m.report_oids.is_empty()));
    }

    #[test]
    fn custom_log_format_counts_as_a_report_producer() {
        let f = fixture();
        let w = f.wt.display().to_string();
        let cmd =
            format!("git -C {w} log -1 --format='HEAD is now at %h'; git -C {w} reset --hard bad");
        let lines = [
            use_line(
                "2026-11-02T10:00:00Z",
                "l",
                "Bash",
                serde_json::json!({"command": cmd}),
            ),
            result_line(
                "2026-11-02T10:00:01Z",
                "l",
                None,
                "HEAD is now at abc1234".into(),
            ),
        ];
        let ev = scan(
            &session(&f.main),
            lines.join("\n").as_bytes(),
            &KeySet::empty(),
            Path::new("/h"),
            &RootCache::default(),
        );
        assert!(
            ev.roots[&f.wt]
                .moves
                .iter()
                .all(|m| m.report_oids.is_empty())
        );
    }

    #[test]
    fn mentions_are_counted() {
        let f = fixture();
        let keys = KeySet::build(&[KeySpec {
            target: "a/b#7".into(),
            base: RepoSlug::parse("a/b").unwrap(),
            number: 7,
            is_pr: true,
            head_ref: Some("topic-x".into()),
        }]);
        let buf = b"see https://github.com/a/b/pull/7 and topic-x";
        let ev = scan(
            &session(&f.main),
            buf,
            &keys,
            Path::new("/h"),
            &RootCache::default(),
        );
        let t = keys.target("a/b#7").unwrap();
        assert_eq!(ev.mentions[t.url[0]], 1);
        assert_eq!(ev.mentions[t.refs[0]], 1);
    }

    #[test]
    fn report_line_patterns() {
        assert_eq!(
            report_oids(MoveVerb::Commit, "[main (root-commit) abcdef1] x"),
            vec!["abcdef1"]
        );
        assert!(report_oids(MoveVerb::Commit, "abcdef1 x\n  [x abcdef1] indented").is_empty());
        assert_eq!(
            report_oids(MoveVerb::Reset, "HEAD is now at 1234567890 msg"),
            vec!["1234567"]
        );
        assert!(report_oids(MoveVerb::Reset, "error: HEAD is now at 1234567").is_empty());
        assert_eq!(
            report_oids(MoveVerb::Merge, "Updating 1111111..2222222\nFast-forward"),
            vec!["2222222"]
        );
        assert!(report_oids(MoveVerb::Rebase, "[x 1234567] y").is_empty());
    }

    #[test]
    fn push_outcome_parsing() {
        let o = push_outcome(
            "To github.com:a/b.git\n * [new branch]      HEAD -> topic\n * [new tag]         v1 -> v1\n + 1111111...2222222 f -> f (forced update)",
        );
        assert_eq!(o.to, ToLine::GitHub(RepoSlug::parse("a/b").unwrap()));
        assert!(o.ok.contains_key("topic") && o.ok.contains_key("f"));
        let p = push_outcome(
            "To github.com:a/b.git\n*\tHEAD:refs/heads/topic\t[new branch]\n!\trefs/heads/x:refs/heads/y\t[rejected] (fetch first)\n+\trefs/tags/v1:refs/heads/release\t1111111...2222222 (forced update)\nDone\n",
        );
        assert_eq!(p.ok.get("topic").map(String::as_str), Some("HEAD"));
        assert!(p.rejected.contains("y"));
        assert!(!p.ok.contains_key("release") && p.new_tags.contains("release"));
        let h = push_outcome(" + 1111111...2222222 refs/tags/v1 -> release (forced update)");
        assert!(!h.ok.contains_key("release"));
        assert!(o.new_tags.contains("v1"));
        let two = push_outcome("To github.com:a/b.git\n x\nTo github.com:c/d.git\n");
        assert_eq!(two.to, ToLine::Other);
        assert_eq!(push_outcome("").to, ToLine::Absent);
        assert_eq!(push_outcome("To /srv/x.git\n").to, ToLine::Other);
    }

    #[test]
    fn tail_reader_drops_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\ncccc\n").unwrap();
        assert_eq!(read_tail(&p, 7), b"cccc\n");
        assert_eq!(read_tail(&p, 100), b"aaaa\nbbbb\ncccc\n");
        assert!(read_tail(&dir.path().join("none"), 10).is_empty());
    }
}
