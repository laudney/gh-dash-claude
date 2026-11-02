//! GitHub repository identities (`owner/name`) and their parsing from
//! remote URLs.

use std::fmt;

/// A lower-cased `owner/name` GitHub repository slug.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoSlug(String);

impl RepoSlug {
    /// Build a slug from `owner/name` text, validating its characters.
    pub fn parse(s: &str) -> Option<Self> {
        let (owner, name) = s.split_once('/')?;
        let ok = |p: &str| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        (ok(owner) && ok(name) && name != "." && name != "..")
            .then(|| RepoSlug(format!("{owner}/{name}").to_ascii_lowercase()))
    }

    /// Build a slug from an owner and a name.
    pub fn from_parts(owner: &str, name: &str) -> Option<Self> {
        Self::parse(&format!("{owner}/{name}"))
    }

    /// The slug text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract the slug from a GitHub remote URL. Accepted forms:
    /// `git@github.com:o/n`, `ssh://git@github.com[:port]/o/n`,
    /// `https://github.com/o/n`, and `github.com:o/n` (as `git push` prints
    /// it), each with an optional `.git` and trailing `/`. Other hosts give
    /// `None`.
    pub fn from_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let rest = if let Some(r) = url.strip_prefix("git@github.com:") {
            r
        } else if let Some(r) = url.strip_prefix("github.com:") {
            r
        } else {
            strip_scheme_host(url)?
        };
        let rest = rest.trim_end_matches('/');
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let mut parts = rest.split('/');
        let (owner, name) = (parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Self::from_parts(owner, name)
    }
}

/// Strip `scheme://[user@]github.com[:port]/` and return the path.
fn strip_scheme_host(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh") {
        return None;
    }
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    host.eq_ignore_ascii_case("github.com").then_some(path)
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Option<String> {
        RepoSlug::from_url(x).map(|r| r.0)
    }

    #[test]
    fn url_forms() {
        let want = Some("alice/project".to_string());
        assert_eq!(s("git@github.com:alice/project.git"), want);
        assert_eq!(s("git@github.com:alice/project"), want);
        assert_eq!(s("https://github.com/alice/project.git"), want);
        assert_eq!(s("https://github.com/alice/project/"), want);
        assert_eq!(s("ssh://git@github.com/alice/project.git"), want);
        assert_eq!(s("ssh://git@github.com:22/alice/project"), want);
        assert_eq!(s("github.com:alice/project.git"), want);
    }

    #[test]
    fn other_hosts_and_bad_paths() {
        assert_eq!(s("git@gitlab.com:a/b.git"), None);
        assert_eq!(s("https://example.com/a/b"), None);
        assert_eq!(s("https://github.com/a"), None);
        assert_eq!(s("https://github.com/a/b/c"), None);
        assert_eq!(s("/srv/git/a.git"), None);
    }

    #[test]
    fn parse_validates() {
        assert!(RepoSlug::parse("upstream/project").is_some());
        assert!(RepoSlug::parse("a/b;rm").is_none());
        assert!(RepoSlug::parse("a").is_none());
        assert!(RepoSlug::parse("a/..").is_none());
        assert_eq!(RepoSlug::parse("A/B").unwrap().as_str(), "a/b");
    }
}
