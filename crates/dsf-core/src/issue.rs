//! Which issues the factory may work on: the trust rules, `blocked by`
//! dependencies and the plan-approval policy.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::labels::{self, Lifecycle};

/// GitHub logins the factory trusts. Logins are case-insensitive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist(BTreeSet<String>);

impl Allowlist {
    pub fn new<I, S>(logins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self(
            logins
                .into_iter()
                .map(|login| login.as_ref().to_ascii_lowercase())
                .collect(),
        )
    }

    pub fn contains(&self, login: &str) -> bool {
        self.0.contains(&login.to_ascii_lowercase())
    }
}

/// What the scheduler knows about an open issue when deciding whether to
/// claim it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub author: &'a str,
    /// Who most recently added `factory:ready`, from the issue events API.
    /// Labels alone don't say who applied them, and on a public repo anyone
    /// with triage access could.
    pub ready_labeller: Option<&'a str>,
    pub labels: &'a [String],
    /// Issues named by `blocked by` that are still open.
    pub open_blockers: &'a [u64],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Claim,
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Not labelled `factory:ready` alone.
    NotReady,
    UntrustedAuthor(String),
    /// `None` when the events API shows no one adding the label.
    UntrustedLabeller(Option<String>),
    WaitingOn(Vec<u64>),
}

/// Decides whether to claim an issue. Trust is checked before anything
/// else about the issue's content is acted on.
pub fn assess(candidate: &Candidate<'_>, allowlist: &Allowlist) -> Verdict {
    if Lifecycle::of(candidate.labels) != Some(Lifecycle::Ready) {
        return Verdict::Skip(SkipReason::NotReady);
    }
    if !allowlist.contains(candidate.author) {
        return Verdict::Skip(SkipReason::UntrustedAuthor(candidate.author.to_owned()));
    }
    match candidate.ready_labeller {
        Some(labeller) if allowlist.contains(labeller) => {}
        other => {
            return Verdict::Skip(SkipReason::UntrustedLabeller(other.map(str::to_owned)));
        }
    }
    if !candidate.open_blockers.is_empty() {
        return Verdict::Skip(SkipReason::WaitingOn(candidate.open_blockers.to_vec()));
    }
    Verdict::Claim
}

/// When a run stops for a human to approve its plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicy {
    Never,
    /// Only for issues labelled `factory:approve-plan`.
    #[default]
    Label,
    Always,
}

/// Whether the workflow's gate stages wait for a human on this issue.
/// When they don't, the engine passes them as `approved`.
pub fn gate_required<S: AsRef<str>>(policy: ApprovalPolicy, labels: &[S]) -> bool {
    match policy {
        ApprovalPolicy::Never => false,
        ApprovalPolicy::Label => labels::has(labels, labels::APPROVE_PLAN.name),
        ApprovalPolicy::Always => true,
    }
}

/// Issue numbers named after `blocked by` in an issue body, in this repo:
/// `Blocked by #12`, `blocked by #3, #4 and #5`. Fenced code blocks are
/// skipped, so examples in the spec don't hold the issue up.
pub fn blockers(body: &str) -> Vec<u64> {
    const PHRASE: &str = "blocked by";
    let mut found = BTreeSet::new();
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // ASCII lowercasing keeps byte offsets, so slicing stays aligned.
        let lower = line.to_ascii_lowercase();
        let mut rest = lower.as_str();
        while let Some(at) = rest.find(PHRASE) {
            rest = take_refs(&rest[at + PHRASE.len()..], &mut found);
        }
    }
    found.into_iter().collect()
}

/// Reads a list of `#N` references, returning what follows it.
fn take_refs<'a>(mut rest: &'a str, found: &mut BTreeSet<u64>) -> &'a str {
    loop {
        rest = rest.trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ':' | '&' | '*' | '_')
        });
        if let Some(after) = rest.strip_prefix("and ") {
            rest = after;
            continue;
        }
        let Some(after_hash) = rest.strip_prefix('#') else {
            return rest;
        };
        let digits = after_hash
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_hash.len());
        if digits == 0 {
            return after_hash;
        }
        if let Ok(number) = after_hash[..digits].parse::<u64>()
            && number > 0
        {
            found.insert(number);
        }
        rest = &after_hash[digits..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NONE: [u64; 0] = [];

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn candidate<'a>(labels: &'a [String], blockers: &'a [u64]) -> Candidate<'a> {
        Candidate {
            author: "Alice",
            ready_labeller: Some("alice"),
            labels,
            open_blockers: blockers,
        }
    }

    #[test]
    fn claims_a_ready_issue_from_trusted_users() {
        let allowlist = Allowlist::new(["alice"]);
        let labels = labels(&["factory:ready"]);
        assert_eq!(assess(&candidate(&labels, &[]), &allowlist), Verdict::Claim);
    }

    #[test]
    fn skips_issues_that_are_not_ready() {
        let allowlist = Allowlist::new(["alice"]);
        for set in [
            &["factory:draft"][..],
            &["factory:ready", "factory:running"],
            &[],
        ] {
            let labels = labels(set);
            assert_eq!(
                assess(&candidate(&labels, &[]), &allowlist),
                Verdict::Skip(SkipReason::NotReady)
            );
        }
    }

    #[test]
    fn author_and_labeller_must_both_be_trusted() {
        let allowlist = Allowlist::new(["alice"]);
        let labels = labels(&["factory:ready"]);
        let stranger = Candidate {
            author: "mallory",
            ..candidate(&labels, &[])
        };
        assert_eq!(
            assess(&stranger, &allowlist),
            Verdict::Skip(SkipReason::UntrustedAuthor("mallory".into()))
        );
        let relabelled = Candidate {
            ready_labeller: Some("mallory"),
            ..candidate(&labels, &[])
        };
        assert_eq!(
            assess(&relabelled, &allowlist),
            Verdict::Skip(SkipReason::UntrustedLabeller(Some("mallory".into())))
        );
        let unknown = Candidate {
            ready_labeller: None,
            ..candidate(&labels, &[])
        };
        assert_eq!(
            assess(&unknown, &allowlist),
            Verdict::Skip(SkipReason::UntrustedLabeller(None))
        );
    }

    #[test]
    fn waits_for_open_blockers() {
        let allowlist = Allowlist::new(["alice"]);
        let labels = labels(&["factory:ready"]);
        assert_eq!(
            assess(&candidate(&labels, &[4, 9]), &allowlist),
            Verdict::Skip(SkipReason::WaitingOn(vec![4, 9]))
        );
    }

    #[test]
    fn approval_policy_decides_whether_gates_wait() {
        let plain = ["factory:ready"];
        let asked = ["factory:ready", "Factory:Approve-Plan"];
        assert!(!gate_required(ApprovalPolicy::Never, &asked));
        assert!(!gate_required(ApprovalPolicy::Label, &plain));
        assert!(gate_required(ApprovalPolicy::Label, &asked));
        assert!(gate_required(ApprovalPolicy::Always, &plain));
    }

    #[test]
    fn finds_blockers_in_the_usual_phrasings() {
        assert_eq!(blockers("Blocked by #12"), [12]);
        assert_eq!(blockers("blocked by #3, #4 and #5."), [3, 4, 5]);
        assert_eq!(blockers("**Blocked by:** #7 & #8"), [7, 8]);
        assert_eq!(blockers("blocked by #2\nalso BLOCKED BY #1 #2"), [1, 2]);
        assert_eq!(
            blockers("Note: blocked by #10; see blocked by #11"),
            [10, 11]
        );
    }

    #[test]
    fn ignores_non_references_and_code_blocks() {
        assert_eq!(blockers("blocked by nothing"), NONE);
        assert_eq!(blockers("blocked by #"), NONE);
        assert_eq!(blockers("blocked by #0"), NONE);
        assert_eq!(blockers("unblocked #4 is not a dependency"), NONE);
        assert_eq!(blockers("blocked by owner/repo#4"), NONE);
        assert_eq!(blockers("blocked by #99999999999999999999999"), NONE);
        assert_eq!(blockers("```\nblocked by #1\n```\nfine"), NONE);
        assert_eq!(blockers("~~~\nblocked by #1\n~~~\nblocked by #2"), [2]);
    }

    proptest! {
        #[test]
        fn never_panics_on_any_body(body in "\\PC{0,300}") {
            let _ = blockers(&body);
        }

        #[test]
        fn finds_every_listed_number(numbers in proptest::collection::btree_set(1u64..100_000, 1..6)) {
            let list: Vec<String> = numbers.iter().map(|n| format!("#{n}")).collect();
            let body = format!("Some spec.\n\nBlocked by {}\n", list.join(", "));
            prop_assert_eq!(blockers(&body), numbers.into_iter().collect::<Vec<_>>());
        }
    }
}
