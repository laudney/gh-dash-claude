//! Review feedback on a PR: conversation comments, reviews, and inline
//! review comments, as fetched from GitHub. Data only; no I/O.

use serde::{Deserialize, Serialize};

use crate::target::PrTarget;

/// What kind of GitHub item this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A comment in the PR conversation.
    Comment,
    /// A submitted review (its own body or state).
    Review,
    /// An inline comment on the diff, part of a review.
    ReviewComment,
}

/// Whether an author is a person or a bot account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    User,
    Bot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub login: String,
    pub kind: AuthorKind,
}

/// One piece of review feedback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The GraphQL node id: the item's identity.
    pub id: String,
    pub kind: ItemKind,
    /// `None` for a deleted account.
    pub author: Option<Author>,
    /// `authorAssociation`, verbatim (`OWNER`, `MEMBER`, …).
    pub association: String,
    /// When the item became visible, in epoch milliseconds.
    pub published: Option<i64>,
    pub url: String,
    pub body: String,
    /// The file of an inline comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The line of an inline comment (`line`, else `originalLine`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// The state of a review (`CHANGES_REQUESTED`, `APPROVED`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_state: Option<String>,
}

/// One open PR with its recent feedback.
#[derive(Debug, Clone)]
pub struct PrFeedback {
    pub pr: PrTarget,
    pub items: Vec<Item>,
}

/// The GraphQL rate-limit status after a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// The summed cost of every page of the fetch.
    pub cost: u32,
    pub remaining: u32,
    /// Epoch milliseconds.
    pub reset_at: i64,
}

/// Everything one poll learns from GitHub.
#[derive(Debug, Clone)]
pub struct Feedback {
    /// The authenticated user's login.
    pub viewer: String,
    pub prs: Vec<PrFeedback>,
    pub rate: Option<RateLimit>,
}
