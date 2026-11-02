//! The GitHub objects a message is about: a PR or an issue.

use crate::keys::KeySpec;
use crate::slug::RepoSlug;

/// A pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrTarget {
    pub base: RepoSlug,
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The head repository; `None` when GitHub no longer knows it (a
    /// deleted fork).
    pub head_repo: Option<RepoSlug>,
    pub head_ref: String,
    pub head_oid: String,
    /// The first (up to 100) changed file paths.
    pub files: Vec<String>,
}

/// An issue with its open linked PRs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTarget {
    pub base: RepoSlug,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub linked_prs: Vec<PrTarget>,
}

/// What a message is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Pr(PrTarget),
    Issue(IssueTarget),
}

impl PrTarget {
    /// The route and key identity `<base>#<n>`.
    pub fn key(&self) -> String {
        format!("{}#{}", self.base, self.number)
    }

    pub fn key_spec(&self) -> KeySpec {
        KeySpec {
            target: self.key(),
            base: self.base.clone(),
            number: self.number,
            is_pr: true,
            head_ref: Some(self.head_ref.clone()),
        }
    }
}

impl Target {
    /// The route and key identity `<base>#<n>`.
    pub fn key(&self) -> String {
        match self {
            Target::Pr(p) => p.key(),
            Target::Issue(i) => format!("{}#{}", i.base, i.number),
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Target::Pr(p) => &p.title,
            Target::Issue(i) => &i.title,
        }
    }

    /// The key specs of this target and, for an issue, its linked PRs.
    pub fn key_specs(&self) -> Vec<KeySpec> {
        match self {
            Target::Pr(p) => vec![p.key_spec()],
            Target::Issue(i) => {
                let mut v = vec![KeySpec {
                    target: self.key(),
                    base: i.base.clone(),
                    number: i.number,
                    is_pr: false,
                    head_ref: None,
                }];
                v.extend(i.linked_prs.iter().map(PrTarget::key_spec));
                v
            }
        }
    }

    /// Every route key that bears on this target: its own and, for
    /// an issue, those of its linked PRs.
    pub fn route_keys(&self) -> Vec<String> {
        match self {
            Target::Pr(p) => vec![p.key()],
            Target::Issue(i) => {
                let mut v = vec![self.key()];
                v.extend(i.linked_prs.iter().map(PrTarget::key));
                v
            }
        }
    }
}
