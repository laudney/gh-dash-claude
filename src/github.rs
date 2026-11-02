//! Target fetches through the `gh` CLI.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::slug::RepoSlug;
use crate::target::{IssueTarget, PrTarget, Target};

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
    let mut search = String::from("is:pr is:open author:@me");
    for r in repos {
        search.push_str(&format!(" repo:{r}"));
    }
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
}
