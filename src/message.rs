//! The text delivered to a session.

use std::path::Path;

use crate::paths::display_home;
use crate::target::Target;

/// Build the delivered text: a header that identifies the target, then a
/// blank line and the user's instruction. The `worktree:` line appears only
/// when an owner signal names a worktree.
pub fn build(target: &Target, worktree: Option<&Path>, home: &Path, instruction: &str) -> String {
    let mut out = String::new();
    let key = target.key();
    out.push_str(&format!("[gh-dash] {key} — {}\n", target.title()));
    match target {
        Target::Pr(p) => {
            out.push_str(&format!("{}\n", p.url));
            let repo = p.head_repo.as_ref().map_or("?", |r| r.as_str());
            let oid = &p.head_oid[..p.head_oid.len().min(8)];
            out.push_str(&format!("head: {repo}:{} @ {oid}\n", p.head_ref));
        }
        Target::Issue(i) => {
            out.push_str(&format!("{}\n", i.url));
            if i.linked_prs.is_empty() {
                out.push_str("issue (no open linked PR)\n");
            } else {
                let linked: Vec<String> = i
                    .linked_prs
                    .iter()
                    .map(|p| format!("#{}", p.number))
                    .collect();
                out.push_str(&format!("issue; linked: {}\n", linked.join(", ")));
            }
        }
    }
    if let Some(w) = worktree {
        out.push_str(&format!("worktree: {}\n", display_home(w, home)));
    }
    out.push('\n');
    out.push_str(instruction.trim_end());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slug::RepoSlug;
    use crate::target::{IssueTarget, PrTarget};

    fn pr() -> PrTarget {
        PrTarget {
            base: RepoSlug::parse("upstream/project").unwrap(),
            number: 1234,
            title: "handle empty input".into(),
            url: "https://github.com/upstream/project/pull/1234".into(),
            head_repo: RepoSlug::parse("alice/project"),
            head_ref: "pr-topic".into(),
            head_oid: "abc12344e621c24e81a87a191c2b2fab51bdd1f0".into(),
            files: vec![],
        }
    }

    #[test]
    fn pr_message() {
        let m = build(
            &Target::Pr(pr()),
            Some(Path::new("/home/u/worktrees/project-pr-topic")),
            Path::new("/home/u"),
            "Address the review.\n",
        );
        assert_eq!(
            m,
            "[gh-dash] upstream/project#1234 — handle empty input\n\
             https://github.com/upstream/project/pull/1234\n\
             head: alice/project:pr-topic @ abc12344\n\
             worktree: ~/worktrees/project-pr-topic\n\
             \n\
             Address the review."
        );
    }

    #[test]
    fn issue_message_without_worktree() {
        let t = Target::Issue(IssueTarget {
            base: RepoSlug::parse("a/b").unwrap(),
            number: 3,
            title: "bug".into(),
            url: "https://github.com/a/b/issues/3".into(),
            linked_prs: vec![pr()],
        });
        let m = build(&t, None, Path::new("/h"), "look");
        assert!(m.contains("issue; linked: #1234\n"));
        assert!(!m.contains("worktree:"));
    }
}
