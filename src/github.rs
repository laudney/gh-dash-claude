//! Target fetches through the `gh` CLI.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::review::{Author, AuthorKind, Feedback, Item, ItemKind, PrFeedback, RateLimit};
use crate::slug::RepoSlug;
use crate::target::{IssueTarget, PrTarget, Target};
use crate::time::parse_rfc3339_ms;

const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// Run `gh` with a timeout; return stdout on success, or an error that
/// carries its stderr.
fn run_gh(args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    let sub = args.first().copied().unwrap_or("");
    let out = crate::proc::run_bounded(&mut cmd, GH_TIMEOUT).ok_or_else(|| {
        anyhow!(
            "gh {sub} did not finish within {}s (is the GitHub CLI installed?)",
            GH_TIMEOUT.as_secs()
        )
    })?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh {sub} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

#[derive(Deserialize)]
struct NameOnly {
    name: String,
}

#[derive(Deserialize)]
struct FilePath {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrJson {
    number: u64,
    title: String,
    url: String,
    head_ref_name: String,
    head_ref_oid: String,
    #[serde(default)]
    head_repository: Option<NameOnly>,
    #[serde(default)]
    head_repository_owner: Option<Owner>,
    #[serde(default)]
    files: Vec<FilePath>,
}

fn pr_from(base: &RepoSlug, j: PrJson) -> PrTarget {
    let head_repo = match (&j.head_repository_owner, &j.head_repository) {
        (Some(o), Some(r)) => RepoSlug::from_parts(&o.login, &r.name),
        _ => None,
    };
    PrTarget {
        base: base.clone(),
        number: j.number,
        title: j.title,
        url: j.url,
        head_repo,
        head_ref: j.head_ref_name,
        head_oid: j.head_ref_oid,
        files: j.files.into_iter().take(100).map(|f| f.path).collect(),
    }
}

const PR_FIELDS: &str =
    "number,title,url,headRefName,headRefOid,headRepository,headRepositoryOwner,files";

/// Fetch one PR.
pub fn fetch_pr(base: &RepoSlug, number: u64) -> Result<PrTarget> {
    let n = number.to_string();
    let out = run_gh(&[
        "pr",
        "view",
        &n,
        "--repo",
        base.as_str(),
        "--json",
        PR_FIELDS,
    ])?;
    let j: PrJson = serde_json::from_slice(&out).context("parsing gh pr view output")?;
    Ok(pr_from(base, j))
}

#[derive(Deserialize)]
struct LinkedRef {
    number: u64,
    #[serde(default)]
    repository: Option<LinkedRepo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkedRepo {
    #[serde(default)]
    name_with_owner: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owner: Option<Owner>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueJson {
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    closed_by_pull_requests_references: Vec<LinkedRef>,
}

/// GraphQL PR fields, as used by the batch and linked-PR queries.
const GQL_PR: &str = "number title url state headRefName headRefOid \
    headRepository { name owner { login } } repository { nameWithOwner } \
    files(first: 100) { nodes { path } }";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPr {
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    state: String,
    head_ref_name: String,
    head_ref_oid: String,
    #[serde(default)]
    head_repository: Option<GqlRepo>,
    repository: GqlBase,
    #[serde(default)]
    files: Option<GqlFiles>,
}

#[derive(Deserialize)]
struct GqlRepo {
    name: String,
    owner: Owner,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlBase {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GqlFiles {
    nodes: Vec<FilePath>,
}

fn pr_from_gql(g: GqlPr) -> Option<PrTarget> {
    let base = RepoSlug::parse(&g.repository.name_with_owner)?;
    Some(PrTarget {
        base,
        number: g.number,
        title: g.title,
        url: g.url,
        head_repo: g
            .head_repository
            .and_then(|r| RepoSlug::from_parts(&r.owner.login, &r.name)),
        head_ref: g.head_ref_name,
        head_oid: g.head_ref_oid,
        files: g
            .files
            .map(|f| f.nodes.into_iter().map(|n| n.path).collect())
            .unwrap_or_default(),
    })
}

/// Fetch one issue with its open linked PRs.
pub fn fetch_issue(base: &RepoSlug, number: u64) -> Result<IssueTarget> {
    let n = number.to_string();
    let out = run_gh(&[
        "issue",
        "view",
        &n,
        "--repo",
        base.as_str(),
        "--json",
        "number,title,url,closedByPullRequestsReferences",
    ])?;
    let j: IssueJson = serde_json::from_slice(&out).context("parsing gh issue view output")?;
    let mut linked = Vec::new();
    let refs: Vec<(String, String, u64)> = j
        .closed_by_pull_requests_references
        .iter()
        .filter_map(|r| {
            let slug = match &r.repository {
                Some(LinkedRepo {
                    name_with_owner: Some(nwo),
                    ..
                }) => RepoSlug::parse(nwo),
                Some(LinkedRepo {
                    name: Some(nm),
                    owner: Some(o),
                    ..
                }) => RepoSlug::from_parts(&o.login, nm),
                _ => Some(base.clone()),
            }?;
            let (o, nm) = slug.as_str().split_once('/')?;
            Some((o.to_string(), nm.to_string(), r.number))
        })
        .collect();
    if !refs.is_empty() {
        let mut q = String::from("query {");
        for (i, (o, nm, num)) in refs.iter().enumerate() {
            q.push_str(&format!(
                " p{i}: repository(owner: {o:?}, name: {nm:?}) {{ pullRequest(number: {num}) {{ {GQL_PR} }} }}"
            ));
        }
        q.push_str(" }");
        let out = run_gh(&["api", "graphql", "-f", &format!("query={q}")])?;
        let v: serde_json::Value = serde_json::from_slice(&out).context("parsing linked PRs")?;
        for i in 0..refs.len() {
            let node = &v["data"][format!("p{i}")]["pullRequest"];
            if let Ok(g) = serde_json::from_value::<GqlPr>(node.clone())
                && g.state == "OPEN"
                && let Some(p) = pr_from_gql(g)
            {
                linked.push(p);
            }
        }
    }
    Ok(IssueTarget {
        base: base.clone(),
        number: j.number,
        title: j.title,
        url: j.url,
        linked_prs: linked,
    })
}

/// Fetch one target.
pub fn fetch(base: &RepoSlug, number: u64, issue: bool) -> Result<Target> {
    if issue {
        fetch_issue(base, number).map(Target::Issue)
    } else {
        fetch_pr(base, number).map(Target::Pr)
    }
}

/// Fetch every open PR authored by the user, optionally limited to some
/// repositories, with one paginated GraphQL search.
pub fn fetch_my_open_prs(repos: &[RepoSlug]) -> Result<Vec<PrTarget>> {
    let search = my_open_prs_search(repos);
    let q = format!(
        "query($endCursor: String) {{ search(query: {search:?}, type: ISSUE, first: 50, after: $endCursor) \
         {{ pageInfo {{ hasNextPage endCursor }} nodes {{ ... on PullRequest {{ {GQL_PR} }} }} }} }}"
    );
    let out = run_gh(&["api", "graphql", "--paginate", "-f", &format!("query={q}")])?;
    // `--paginate` concatenates one JSON document per page.
    let mut prs = Vec::new();
    let stream = serde_json::Deserializer::from_slice(&out).into_iter::<serde_json::Value>();
    for page in stream {
        let page = page.context("parsing gh search output")?;
        let nodes = page["data"]["search"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for n in nodes {
            if let Ok(g) = serde_json::from_value::<GqlPr>(n)
                && let Some(p) = pr_from_gql(g)
            {
                prs.push(p);
            }
        }
    }
    Ok(prs)
}

/// The search string for the user's open PRs, optionally per repository.
fn my_open_prs_search(repos: &[RepoSlug]) -> String {
    let mut search = String::from("is:pr is:open author:@me");
    for r in repos {
        search.push_str(&format!(" repo:{r}"));
    }
    search
}

/// A paginated fetch with nested connections can take longer than a
/// single-object view.
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(90);

/// Author fields shared by every feedback node.
const GQL_AUTHOR: &str = "authorAssociation author { __typename login }";

/// Why a feedback fetch failed.
#[derive(Debug)]
pub enum FetchError {
    /// GitHub refused the request for its rate limit. `retry_at` is
    /// GitHub's own deadline (epoch ms), when the response named one.
    RateLimited {
        secondary: bool,
        retry_at: Option<i64>,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited {
                secondary,
                retry_at,
            } => {
                let kind = if *secondary { "secondary " } else { "" };
                match retry_at {
                    Some(t) => write!(f, "GitHub {kind}rate limit; retry at {t} (epoch ms)"),
                    None => write!(f, "GitHub {kind}rate limit; no retry time given"),
                }
            }
            FetchError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<anyhow::Error> for FetchError {
    fn from(e: anyhow::Error) -> Self {
        FetchError::Other(e)
    }
}

/// The status line and headers of one HTTP response, as `gh --include`
/// prints them. Header names are stored lower-case.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Headers {
    pub status: u16,
    fields: Vec<(String, String)>,
}

impl Headers {
    /// The first value of a header, by case-insensitive name.
    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.fields
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    fn parse(block: &str) -> Self {
        let mut lines = block.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let fields = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Headers { status, fields }
    }
}

/// Split `gh api --include` output into (headers, JSON body) pairs, one per
/// response (`--paginate` prints one per page). Output without a status
/// line is a single body with empty headers.
pub fn split_responses(buf: &[u8]) -> Vec<(Headers, &[u8])> {
    let mut out = Vec::new();
    let mut rest = buf;
    loop {
        rest = trim_start(rest);
        if rest.is_empty() {
            break;
        }
        let mut headers = Headers::default();
        if rest.starts_with(b"HTTP/") {
            // Headers end at the first blank line (LF or CRLF).
            let end = find_blank_line(rest);
            let (block, after) = match end {
                Some((at, skip)) => (&rest[..at], &rest[at + skip..]),
                None => (rest, &rest[rest.len()..]),
            };
            headers = Headers::parse(&String::from_utf8_lossy(block));
            rest = after;
        }
        let body_start = trim_start(rest);
        let mut stream =
            serde_json::Deserializer::from_slice(body_start).into_iter::<serde::de::IgnoredAny>();
        match stream.next() {
            Some(Ok(_)) => {
                let len = stream.byte_offset();
                out.push((headers, &body_start[..len]));
                rest = &body_start[len..];
            }
            // No (or an unparsable) body: keep the headers, stop.
            _ => {
                out.push((headers, body_start));
                break;
            }
        }
    }
    out
}

fn trim_start(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    &b[n..]
}

/// The offset of the first blank line and the length of the separator.
fn find_blank_line(b: &[u8]) -> Option<(usize, usize)> {
    let lf = b.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = b.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(c)) => Some(if a.0 <= c.0 { a } else { c }),
        (a, c) => a.or(c),
    }
}

/// Classify a failed fetch as a rate limit, from `gh`'s stderr and the
/// last response's headers. `now` is epoch ms.
fn rate_limit_error(stderr: &str, last: Option<&Headers>, now: i64) -> Option<FetchError> {
    let text = stderr.to_ascii_lowercase();
    let remaining_zero = last.and_then(|h| h.get("x-ratelimit-remaining")) == Some("0");
    let retry_after = last
        .and_then(|h| h.get("retry-after"))
        .and_then(|v| v.parse::<i64>().ok());
    let refused = last.is_some_and(|h| matches!(h.status, 403 | 429))
        && (retry_after.is_some() || remaining_zero);
    if !text.contains("rate limit") && !refused {
        return None;
    }
    let retry_at = match retry_after {
        Some(s) => Some(now + s.max(0) * 1000),
        None if remaining_zero => last
            .and_then(|h| h.get("x-ratelimit-reset"))
            .and_then(|v| v.parse::<i64>().ok())
            .map(|s| s * 1000),
        None => None,
    };
    Some(FetchError::RateLimited {
        secondary: text.contains("secondary"),
        retry_at,
    })
}

/// A GraphQL connection. GitHub may return a null list or null entries;
/// each entry is parsed on its own, so one bad node never drops its
/// siblings.
#[derive(Deserialize)]
struct Nodes {
    #[serde(default)]
    nodes: Option<Vec<Option<serde_json::Value>>>,
}

impl Nodes {
    fn parse<T: serde::de::DeserializeOwned>(self) -> Vec<T> {
        self.nodes
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }
}

fn nodes<T: serde::de::DeserializeOwned>(n: Option<Nodes>) -> Vec<T> {
    n.map(Nodes::parse).unwrap_or_default()
}

#[derive(Deserialize)]
struct GqlAuthor {
    #[serde(rename = "__typename", default)]
    typename: String,
    login: String,
}

impl GqlAuthor {
    fn into_author(self) -> Author {
        Author {
            kind: if self.typename == "Bot" {
                AuthorKind::Bot
            } else {
                AuthorKind::User
            },
            login: self.login,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlComment {
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    author_association: String,
    #[serde(default)]
    author: Option<GqlAuthor>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlReview {
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    author_association: String,
    #[serde(default)]
    author: Option<GqlAuthor>,
    #[serde(default)]
    comments: Option<Nodes>,
}

#[derive(Deserialize)]
struct GqlFeedbackPr {
    #[serde(flatten)]
    pr: GqlPr,
    #[serde(default)]
    comments: Option<Nodes>,
    #[serde(default)]
    reviews: Option<Nodes>,
}

fn time(s: &Option<String>) -> Option<i64> {
    s.as_deref().and_then(parse_rfc3339_ms)
}

/// The items of one PR, per the publication-time and review rules.
fn items_of(pr: &mut GqlFeedbackPr) -> Vec<Item> {
    let mut items = Vec::new();
    for c in nodes::<GqlComment>(pr.comments.take()) {
        items.push(Item {
            published: time(&c.published_at).or_else(|| time(&c.created_at)),
            id: c.id,
            kind: ItemKind::Comment,
            author: c.author.map(GqlAuthor::into_author),
            association: c.author_association,
            url: c.url,
            body: c.body,
            path: None,
            line: None,
            review_state: None,
        });
    }
    for r in nodes::<GqlReview>(pr.reviews.take()) {
        if r.state == "PENDING" {
            continue;
        }
        let submitted = time(&r.submitted_at);
        for c in nodes::<GqlComment>(r.comments) {
            items.push(Item {
                published: submitted.or_else(|| time(&c.published_at)),
                id: c.id,
                kind: ItemKind::ReviewComment,
                author: c.author.map(GqlAuthor::into_author),
                association: c.author_association,
                url: c.url,
                body: c.body,
                path: c.path,
                line: c.line.or(c.original_line),
                review_state: None,
            });
        }
        if !r.body.trim().is_empty() || r.state == "CHANGES_REQUESTED" {
            items.push(Item {
                published: submitted.or_else(|| time(&r.published_at)),
                id: r.id,
                kind: ItemKind::Review,
                author: r.author.map(GqlAuthor::into_author),
                association: r.author_association,
                url: r.url,
                body: r.body,
                path: None,
                line: None,
                review_state: Some(r.state),
            });
        }
    }
    items
}

/// Parse the JSON pages of a feedback fetch.
fn parse_feedback_pages<'a>(pages: impl IntoIterator<Item = &'a [u8]>) -> Result<Feedback> {
    let mut viewer: Option<String> = None;
    let mut prs = Vec::new();
    let mut cost: u32 = 0;
    let mut last_rate: Option<(u32, i64)> = None;
    for page in pages {
        let v: serde_json::Value =
            serde_json::from_slice(page).context("parsing gh search output")?;
        let data = &v["data"];
        if viewer.is_none() {
            viewer = data["viewer"]["login"].as_str().map(str::to_string);
        }
        let rl = &data["rateLimit"];
        if let Some(c) = rl["cost"].as_u64() {
            cost = cost.saturating_add(c.min(u64::from(u32::MAX)) as u32);
        }
        if let (Some(rem), Some(reset)) = (
            rl["remaining"].as_u64(),
            rl["resetAt"].as_str().and_then(parse_rfc3339_ms),
        ) {
            last_rate = Some((rem.min(u64::from(u32::MAX)) as u32, reset));
        }
        let nodes = data["search"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for n in nodes {
            if let Ok(mut g) = serde_json::from_value::<GqlFeedbackPr>(n) {
                let items = items_of(&mut g);
                if let Some(pr) = pr_from_gql(g.pr) {
                    prs.push(PrFeedback { pr, items });
                }
            }
        }
    }
    let viewer = viewer.ok_or_else(|| anyhow!("GitHub did not return the viewer login"))?;
    Ok(Feedback {
        viewer,
        prs,
        rate: last_rate.map(|(remaining, reset_at)| RateLimit {
            cost,
            remaining,
            reset_at,
        }),
    })
}

/// Fetch every open PR authored by the user (optionally limited to some
/// repositories) with its recent review feedback, in one paginated
/// GraphQL search with response headers.
pub fn fetch_my_open_prs_with_feedback(repos: &[RepoSlug]) -> Result<Feedback, FetchError> {
    let search = my_open_prs_search(repos);
    let q = format!(
        "query($endCursor: String) {{ viewer {{ login }} rateLimit {{ cost remaining resetAt }} \
         search(query: {search:?}, type: ISSUE, first: 25, after: $endCursor) \
         {{ pageInfo {{ hasNextPage endCursor }} nodes {{ ... on PullRequest {{ {GQL_PR} \
         comments(last: 50) {{ nodes {{ id url body createdAt publishedAt {GQL_AUTHOR} }} }} \
         reviews(last: 30) {{ nodes {{ id url body state submittedAt publishedAt {GQL_AUTHOR} \
         comments(first: 30) {{ nodes {{ id url body createdAt publishedAt path line originalLine \
         {GQL_AUTHOR} }} }} }} }} }} }} }} }}"
    );
    let mut cmd = Command::new("gh");
    cmd.args([
        "api",
        "graphql",
        "--paginate",
        "--include",
        "-f",
        &format!("query={q}"),
    ])
    .env("GH_PROMPT_DISABLED", "1")
    .env("NO_COLOR", "1");
    let out = crate::proc::run_bounded(&mut cmd, FEEDBACK_TIMEOUT).ok_or_else(|| {
        anyhow!(
            "gh api did not finish within {}s (is the GitHub CLI installed?)",
            FEEDBACK_TIMEOUT.as_secs()
        )
    })?;
    let responses = split_responses(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A relative `Retry-After` counts from when the response arrived.
        let now = crate::time::now_ms();
        if let Some(e) = rate_limit_error(&stderr, responses.last().map(|(h, _)| h), now) {
            return Err(e);
        }
        return Err(anyhow!("gh api failed: {}", stderr.trim()).into());
    }
    Ok(parse_feedback_pages(responses.into_iter().map(|(_, b)| b))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_view_json() {
        let j: PrJson = serde_json::from_str(
            r#"{"number":1234,"title":"t","url":"u","headRefName":"pr-topic",
               "headRefOid":"abc12344e621c24e81a87a191c2b2fab51bdd1f0",
               "headRepository":{"id":"x","name":"project"},
               "headRepositoryOwner":{"id":"y","login":"alice"},
               "files":[{"path":"a.rs","additions":1,"deletions":0}]}"#,
        )
        .unwrap();
        let p = pr_from(&RepoSlug::parse("upstream/project").unwrap(), j);
        assert_eq!(p.head_repo.unwrap().as_str(), "alice/project");
        assert_eq!(p.files, vec!["a.rs"]);
    }

    #[test]
    fn deleted_fork_has_no_head_repo() {
        let j: PrJson = serde_json::from_str(
            r#"{"number":1,"title":"t","url":"u","headRefName":"x","headRefOid":"y",
               "headRepository":null,"headRepositoryOwner":{"login":"ghost"}}"#,
        )
        .unwrap();
        assert!(
            pr_from(&RepoSlug::parse("a/b").unwrap(), j)
                .head_repo
                .is_none()
        );
    }

    #[test]
    fn parses_graphql_pr() {
        let g: GqlPr = serde_json::from_str(
            r#"{"number":4,"title":"t","url":"u","state":"OPEN","headRefName":"h",
               "headRefOid":"o","headRepository":{"name":"n","owner":{"login":"me"}},
               "repository":{"nameWithOwner":"Up/Stream"},"files":{"nodes":[{"path":"f"}]}}"#,
        )
        .unwrap();
        let p = pr_from_gql(g).unwrap();
        assert_eq!(
            (p.base.as_str(), p.head_repo.unwrap().as_str()),
            ("up/stream", "me/n")
        );
    }

    const PAGE1: &str = r#"{"data":{"viewer":{"login":"alice"},
      "rateLimit":{"cost":3,"remaining":4990,"resetAt":"2026-01-01T01:00:00Z"},
      "search":{"pageInfo":{"hasNextPage":true,"endCursor":"c1"},"nodes":[
        {"number":7,"title":"t","url":"https://github.com/upstream/project/pull/7",
         "state":"OPEN","headRefName":"pr-topic","headRefOid":"abc",
         "headRepository":{"name":"project","owner":{"login":"alice"}},
         "repository":{"nameWithOwner":"upstream/project"},"files":{"nodes":[]},
         "comments":{"nodes":[
           {"id":"C1","url":"u1","body":"please rename","createdAt":"2026-01-01T00:00:00Z",
            "publishedAt":"2026-01-01T00:00:05Z","authorAssociation":"MEMBER",
            "author":{"__typename":"User","login":"bob"}},
           {"id":"C2","url":"u2","body":"gone","createdAt":"2026-01-01T00:01:00Z",
            "publishedAt":null,"authorAssociation":"NONE","author":null}]},
         "reviews":{"nodes":[
           {"id":"R1","url":"r1","body":"","state":"APPROVED",
            "submittedAt":"2026-01-01T00:02:00Z","publishedAt":"2026-01-01T00:00:30Z",
            "authorAssociation":"OWNER","author":{"__typename":"User","login":"carol"},
            "comments":{"nodes":[
              {"id":"RC1","url":"rc1","body":"nit","createdAt":"2026-01-01T00:00:10Z",
               "publishedAt":"2026-01-01T00:00:20Z","path":"src/a.rs","line":null,
               "originalLine":12,"authorAssociation":"OWNER",
               "author":{"__typename":"User","login":"carol"}}]}},
           {"id":"R2","url":"r2","body":"","state":"CHANGES_REQUESTED",
            "submittedAt":null,"publishedAt":"2026-01-01T00:03:00Z",
            "authorAssociation":"NONE","author":{"__typename":"Bot","login":"review-bot"},
            "comments":{"nodes":[
              {"id":"RC2","url":"rc2","body":"unwrap","createdAt":"2026-01-01T00:00:40Z",
               "publishedAt":"2026-01-01T00:04:00Z","path":"src/b.rs","line":3,
               "originalLine":3,"authorAssociation":"NONE",
               "author":{"__typename":"Bot","login":"review-bot"}}]}},
           {"id":"R3","url":"r3","body":"draft","state":"PENDING",
            "submittedAt":null,"publishedAt":null,"authorAssociation":"MEMBER",
            "author":{"__typename":"User","login":"bob"},
            "comments":{"nodes":[{"id":"RC3","url":"rc3","body":"x",
              "authorAssociation":"MEMBER","author":{"__typename":"User","login":"bob"}}]}}
         ]}}]}}}"#;

    const PAGE2: &str = r#"{"data":{"viewer":{"login":"alice"},
      "rateLimit":{"cost":2,"remaining":4988,"resetAt":"2026-01-01T01:00:00Z"},
      "search":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{}]}}}"#;

    fn ms(s: &str) -> Option<i64> {
        crate::time::parse_rfc3339_ms(s)
    }

    #[test]
    fn parses_feedback_pages() {
        let f = parse_feedback_pages([PAGE1.as_bytes(), PAGE2.as_bytes()]).unwrap();
        assert_eq!(f.viewer, "alice");
        let rate = f.rate.unwrap();
        assert_eq!((rate.cost, rate.remaining), (5, 4988), "cost is summed");
        assert_eq!(Some(rate.reset_at), ms("2026-01-01T01:00:00Z"));
        assert_eq!(f.prs.len(), 1);
        let items = &f.prs[0].items;
        let get = |id: &str| items.iter().find(|i| i.id == id);

        let c1 = get("C1").unwrap();
        assert_eq!(c1.kind, ItemKind::Comment);
        assert_eq!(c1.published, ms("2026-01-01T00:00:05Z"));
        assert_eq!(c1.author.as_ref().unwrap().kind, AuthorKind::User);

        let c2 = get("C2").unwrap();
        assert!(c2.author.is_none());
        assert_eq!(
            c2.published,
            ms("2026-01-01T00:01:00Z"),
            "createdAt fallback"
        );

        // An approval without a body is not an item; its comments are, at
        // the review's submission time.
        assert!(get("R1").is_none());
        let rc1 = get("RC1").unwrap();
        assert_eq!(rc1.published, ms("2026-01-01T00:02:00Z"));
        assert_eq!(
            (rc1.path.as_deref(), rc1.line),
            (Some("src/a.rs"), Some(12))
        );

        // Changes requested counts even without a body; without
        // submittedAt the review uses its own publishedAt, and its comment
        // uses the comment's own publishedAt (not the review's).
        let r2 = get("R2").unwrap();
        assert_eq!(r2.kind, ItemKind::Review);
        assert_eq!(r2.review_state.as_deref(), Some("CHANGES_REQUESTED"));
        assert_eq!(r2.published, ms("2026-01-01T00:03:00Z"));
        assert_eq!(r2.author.as_ref().unwrap().kind, AuthorKind::Bot);
        let rc2 = get("RC2").unwrap();
        assert_eq!(rc2.published, ms("2026-01-01T00:04:00Z"));

        // Pending reviews and their comments are never items.
        assert!(get("R3").is_none() && get("RC3").is_none());
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn null_connections_and_entries_keep_siblings() {
        let page = r#"{"data":{"viewer":{"login":"alice"},"search":{"nodes":[
          {"number":1,"title":"t","url":"u","headRefName":"h","headRefOid":"o",
           "repository":{"nameWithOwner":"upstream/project"},
           "comments":{"nodes":[null,{"id":"C1","body":"x"},{"bad":true}]},
           "reviews":{"nodes":null}},
          {"number":2,"title":"t","url":"u","headRefName":"h","headRefOid":"o",
           "repository":{"nameWithOwner":"upstream/project"},
           "comments":null,
           "reviews":{"nodes":[{"id":"R1","state":"COMMENTED","body":"b",
             "comments":{"nodes":[null]}}]}}]}}}"#;
        let f = parse_feedback_pages([page.as_bytes()]).unwrap();
        assert_eq!(f.prs.len(), 2);
        let ids: Vec<&str> = f
            .prs
            .iter()
            .flat_map(|p| &p.items)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(ids, vec!["C1", "R1"]);
    }

    #[test]
    fn feedback_without_viewer_is_an_error() {
        assert!(parse_feedback_pages([br#"{"data":{"search":{"nodes":[]}}}"#.as_slice()]).is_err());
    }

    #[test]
    fn splits_included_responses() {
        let buf = b"HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 9\n\n{\"a\":1}\n\
                    HTTP/2.0 200 OK\r\nRetry-After: 5\r\n\r\n{\"b\":[1,{\"c\":\"}\"}]}";
        let r = split_responses(buf);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0.status, 200);
        assert_eq!(r[0].0.get("x-ratelimit-remaining"), Some("9"));
        assert_eq!(r[0].1, b"{\"a\":1}");
        assert_eq!(r[1].0.get("RETRY-AFTER"), Some("5"));
        assert_eq!(r[1].1, b"{\"b\":[1,{\"c\":\"}\"}]}");

        let plain = split_responses(b"{\"a\":1}{\"a\":2}");
        assert_eq!(plain.len(), 2);
        assert_eq!(plain[1].0, Headers::default());

        let headers_only = split_responses(b"HTTP/2.0 403 Forbidden\nRetry-After: 60\n\n");
        assert_eq!(headers_only.len(), 1);
        assert_eq!(headers_only[0].0.status, 403);
    }

    fn headers(text: &str) -> Headers {
        split_responses(text.as_bytes()).remove(0).0
    }

    #[test]
    fn classifies_rate_limits() {
        let now = 1_000_000;
        // Retry-After wins.
        let h = headers(
            "HTTP/2.0 403 Forbidden\nRetry-After: 30\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 5000\n\n",
        );
        match rate_limit_error(
            "gh: You have exceeded a secondary rate limit",
            Some(&h),
            now,
        ) {
            Some(FetchError::RateLimited {
                secondary: true,
                retry_at: Some(t),
            }) => {
                assert_eq!(t, now + 30_000)
            }
            other => panic!("{other:?}"),
        }
        // Primary limit: the reset header, in ms.
        let h =
            headers("HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 1767229200\n\n");
        match rate_limit_error("gh: API rate limit exceeded", Some(&h), now) {
            Some(FetchError::RateLimited {
                secondary: false,
                retry_at: Some(t),
            }) => {
                // The same instant as a GraphQL resetAt.
                assert_eq!(Some(t), ms("2026-01-01T01:00:00Z"))
            }
            other => panic!("{other:?}"),
        }
        // Text only: no deadline.
        assert!(matches!(
            rate_limit_error("gh: API rate limit exceeded", None, now),
            Some(FetchError::RateLimited { retry_at: None, .. })
        ));
        // Other failures are not rate limits, even when the last page
        // reported an exhausted quota.
        let h = headers("HTTP/2.0 502 Bad Gateway\n\n");
        assert!(rate_limit_error("gh: HTTP 502", Some(&h), now).is_none());
        let h = headers("HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 5000\n\n");
        assert!(rate_limit_error("gh: connection reset", Some(&h), now).is_none());
    }
}
