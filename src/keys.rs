//! Mention keys: one Aho-Corasick pass counts every key of every target.

use std::collections::HashMap;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

use crate::slug::RepoSlug;

/// Index of a unique key.
pub type KeyId = usize;

/// What a key identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// `github.com/<base>/pull/<n>` or `github.com/<base>/issues/<n>`.
    Url,
    /// `<base>#<n>`.
    Short,
    /// A head branch name.
    Ref,
}

/// The input describing one target's keys.
#[derive(Debug, Clone)]
pub struct KeySpec {
    /// The target key used to look up its key ids (`<base>#<n>`).
    pub target: String,
    pub base: RepoSlug,
    pub number: u64,
    pub is_pr: bool,
    pub head_ref: Option<String>,
}

/// The key ids of one target, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetKeys {
    pub url: Vec<KeyId>,
    pub short: Vec<KeyId>,
    pub refs: Vec<KeyId>,
}

/// All keys of a resolution run.
pub struct KeySet {
    kinds: Vec<KeyKind>,
    ci: Option<AhoCorasick>,
    ci_ids: Vec<KeyId>,
    exact: Option<AhoCorasick>,
    exact_ids: Vec<KeyId>,
    targets: HashMap<String, TargetKeys>,
}

const DEFAULT_BRANCHES: [&str; 4] = ["main", "master", "develop", "trunk"];

impl KeySet {
    /// An empty set (used by `list`).
    pub fn empty() -> Self {
        Self::build(&[])
    }

    /// Build the key set, deduplicating keys per automaton and pattern.
    pub fn build(specs: &[KeySpec]) -> Self {
        let mut kinds = Vec::new();
        let mut ci_pats: Vec<String> = Vec::new();
        let mut ci_ids = Vec::new();
        let mut ci_index: HashMap<String, KeyId> = HashMap::new();
        let mut ex_pats: Vec<String> = Vec::new();
        let mut exact_ids = Vec::new();
        let mut ex_index: HashMap<String, KeyId> = HashMap::new();
        let mut targets: HashMap<String, TargetKeys> = HashMap::new();

        let mut add = |pat: String, kind: KeyKind, kinds: &mut Vec<KeyKind>| -> KeyId {
            let (index, pats, ids) = if kind == KeyKind::Ref {
                (&mut ex_index, &mut ex_pats, &mut exact_ids)
            } else {
                (&mut ci_index, &mut ci_pats, &mut ci_ids)
            };
            let norm = if kind == KeyKind::Ref {
                pat.clone()
            } else {
                pat.to_ascii_lowercase()
            };
            *index.entry(norm).or_insert_with(|| {
                let id = kinds.len();
                kinds.push(kind);
                pats.push(pat);
                ids.push(id);
                id
            })
        };

        for s in specs {
            let base = s.base.as_str();
            let n = s.number;
            let path = if s.is_pr { "pull" } else { "issues" };
            let url = add(
                format!("github.com/{base}/{path}/{n}"),
                KeyKind::Url,
                &mut kinds,
            );
            let short = add(format!("{base}#{n}"), KeyKind::Short, &mut kinds);
            let t = targets.entry(s.target.clone()).or_default();
            push_unique(&mut t.url, url);
            push_unique(&mut t.short, short);
            if let Some(r) = s.head_ref.as_deref()
                && r.len() >= 4
                && !DEFAULT_BRANCHES.contains(&r)
            {
                let id = add(r.to_string(), KeyKind::Ref, &mut kinds);
                push_unique(&mut targets.get_mut(&s.target).unwrap().refs, id);
            }
        }

        let ci = (!ci_pats.is_empty()).then(|| {
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .ascii_case_insensitive(true)
                .build(&ci_pats)
                .expect("mention automaton")
        });
        let exact = (!ex_pats.is_empty()).then(|| {
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .build(&ex_pats)
                .expect("ref automaton")
        });
        KeySet {
            kinds,
            ci,
            ci_ids,
            exact,
            exact_ids,
            targets,
        }
    }

    /// Number of unique keys.
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// The key ids of a target.
    pub fn target(&self, key: &str) -> Option<&TargetKeys> {
        self.targets.get(key)
    }

    /// Count every key in `buf`, with boundary checks; indexed by key id.
    pub fn count(&self, buf: &[u8]) -> Vec<u32> {
        let mut counts = vec![0u32; self.kinds.len()];
        if let Some(ac) = &self.ci {
            for m in ac.find_overlapping_iter(buf) {
                let id = self.ci_ids[m.pattern().as_usize()];
                if !buf.get(m.end()).is_some_and(u8::is_ascii_digit) {
                    counts[id] += 1;
                }
            }
        }
        if let Some(ac) = &self.exact {
            for m in ac.find_overlapping_iter(buf) {
                let id = self.exact_ids[m.pattern().as_usize()];
                let before = m.start().checked_sub(1).and_then(|i| buf.get(i));
                let after = buf.get(m.end());
                if !before.is_some_and(|&b| ref_byte(b)) && !after.is_some_and(|&b| ref_byte(b)) {
                    counts[id] += 1;
                }
            }
        }
        counts
    }
}

fn ref_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-')
}

fn push_unique(v: &mut Vec<KeyId>, id: KeyId) {
    if !v.contains(&id) {
        v.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(target: &str, n: u64, r: Option<&str>) -> KeySpec {
        KeySpec {
            target: target.into(),
            base: RepoSlug::parse("upstream/project").unwrap(),
            number: n,
            is_pr: true,
            head_ref: r.map(String::from),
        }
    }

    #[test]
    fn counts_with_boundaries() {
        let ks = KeySet::build(&[spec("a#1234", 1234, Some("pr-topic"))]);
        let t = ks.target("a#1234").unwrap().clone();
        let text = b"see https://github.com/upstream/project/pull/1234 and \
                     github.com/upstream/project/pull/12340, upstream/project#1234; \
                     git push fork pr-topic; pr-topic-v2; x/pr-topic";
        let c = ks.count(text);
        assert_eq!(c[t.url[0]], 1, "the 12340 URL must not count");
        assert_eq!(c[t.short[0]], 1);
        assert_eq!(c[t.refs[0]], 1, "longer ref tokens must not count");
    }

    #[test]
    fn shared_and_overlapping_keys() {
        let ks = KeySet::build(&[
            spec("a#1", 1, Some("topic")),
            spec("a#2", 2, Some("topic")),
            spec("a#3", 3, Some("topic-two")),
        ]);
        let a = ks.target("a#1").unwrap().refs[0];
        assert_eq!(a, ks.target("a#2").unwrap().refs[0], "shared ref key");
        let c = ks.count(b"topic topic-two");
        assert_eq!(c[a], 1);
        assert_eq!(c[ks.target("a#3").unwrap().refs[0]], 1);
    }

    #[test]
    fn default_and_short_refs_are_not_keys() {
        let ks = KeySet::build(&[spec("a#1", 1, Some("main")), spec("a#2", 2, Some("abc"))]);
        assert!(ks.target("a#1").unwrap().refs.is_empty());
        assert!(ks.target("a#2").unwrap().refs.is_empty());
    }

    #[test]
    fn ref_equal_to_short_key_stays_separate() {
        let ks = KeySet::build(&[
            spec("a#1", 1, Some("upstream/project#123")),
            spec("a#123", 123, None),
        ]);
        let r = ks.target("a#1").unwrap().refs[0];
        let s = ks.target("a#123").unwrap().short[0];
        assert_ne!(r, s);
        let c = ks.count(b"UPSTREAM/PROJECT#123");
        assert_eq!((c[r], c[s]), (0, 1));
    }
}
