//! The GitHub labels dsf uses, and the edits that move an issue through
//! its lifecycle.
//!
//! Exactly one lifecycle label should be on an issue at a time. The option
//! labels (`factory:approve-plan`, `factory:plan-approved`) are humans'
//! inputs and sit alongside it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label {
    pub name: &'static str,
    /// Hex colour without the `#`, as the GitHub API wants it.
    pub color: &'static str,
    pub description: &'static str,
}

pub const DRAFT: Label = Label {
    name: "factory:draft",
    color: "d4c5f9",
    description: "Being written; the factory ignores it",
};
pub const READY: Label = Label {
    name: "factory:ready",
    color: "0e8a16",
    description: "Approved for the factory to pick up",
};
pub const RUNNING: Label = Label {
    name: "factory:running",
    color: "fbca04",
    description: "The factory is working on this",
};
pub const AWAITING_APPROVAL: Label = Label {
    name: "factory:awaiting-approval",
    color: "c5def5",
    description: "Plan posted; add factory:plan-approved to continue",
};
pub const REVIEW: Label = Label {
    name: "factory:review",
    color: "1d76db",
    description: "Draft PR open for human review",
};
pub const BLOCKED: Label = Label {
    name: "factory:blocked",
    color: "b60205",
    description: "The factory stopped; see the latest comment",
};
pub const APPROVE_PLAN: Label = Label {
    name: "factory:approve-plan",
    color: "5319e7",
    description: "Ask for plan approval before implementing",
};
pub const PLAN_APPROVED: Label = Label {
    name: "factory:plan-approved",
    color: "006b75",
    description: "The posted plan is approved",
};

/// Every label `dsf labels init` creates.
pub const ALL: [Label; 8] = [
    DRAFT,
    READY,
    RUNNING,
    AWAITING_APPROVAL,
    REVIEW,
    BLOCKED,
    APPROVE_PLAN,
    PLAN_APPROVED,
];

/// Where an issue is in the factory's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Draft,
    Ready,
    Running,
    AwaitingApproval,
    Review,
    Blocked,
}

impl Lifecycle {
    const ALL: [Self; 6] = [
        Self::Draft,
        Self::Ready,
        Self::Running,
        Self::AwaitingApproval,
        Self::Review,
        Self::Blocked,
    ];

    pub const fn label(self) -> Label {
        match self {
            Self::Draft => DRAFT,
            Self::Ready => READY,
            Self::Running => RUNNING,
            Self::AwaitingApproval => AWAITING_APPROVAL,
            Self::Review => REVIEW,
            Self::Blocked => BLOCKED,
        }
    }

    /// The issue's lifecycle state, if it carries exactly one lifecycle
    /// label. Several at once is ambiguous and treated as none, so the
    /// factory leaves the issue alone until a human tidies it.
    pub fn of<S: AsRef<str>>(labels: &[S]) -> Option<Self> {
        let mut found = Self::ALL
            .into_iter()
            .filter(|state| has(labels, state.label().name));
        let state = found.next()?;
        found.next().is_none().then_some(state)
    }
}

/// Label edits for the GitHub API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelChange {
    pub add: Vec<&'static str>,
    pub remove: Vec<&'static str>,
}

impl LabelChange {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// The edits that leave `to` as the only lifecycle label.
pub fn move_to<S: AsRef<str>>(labels: &[S], to: Lifecycle) -> LabelChange {
    let mut change = LabelChange::default();
    for state in Lifecycle::ALL {
        let name = state.label().name;
        if state == to {
            if !has(labels, name) {
                change.add.push(name);
            }
        } else if has(labels, name) {
            change.remove.push(name);
        }
    }
    change
}

/// The edits for claiming an issue to start a fresh run. A leftover
/// `factory:plan-approved` from an earlier run goes too, so it can't be
/// mistaken for approval of the new plan.
pub fn claim<S: AsRef<str>>(labels: &[S]) -> LabelChange {
    let mut change = move_to(labels, Lifecycle::Running);
    if has(labels, PLAN_APPROVED.name) {
        change.remove.push(PLAN_APPROVED.name);
    }
    change
}

/// GitHub treats label names case-insensitively.
pub fn has<S: AsRef<str>>(labels: &[S], name: &str) -> bool {
    labels
        .iter()
        .any(|label| label.as_ref().eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_are_unique_and_namespaced() {
        let mut names: Vec<_> = ALL.iter().map(|label| label.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
        assert!(names.iter().all(|name| name.starts_with("factory:")));
        assert!(ALL.iter().all(|label| label.description.len() <= 100));
        assert!(
            ALL.iter().all(|label| label.color.len() == 6
                && label.color.bytes().all(|b| b.is_ascii_hexdigit()))
        );
    }

    #[test]
    fn lifecycle_needs_exactly_one_label() {
        assert_eq!(
            Lifecycle::of(&["bug", "factory:ready"]),
            Some(Lifecycle::Ready)
        );
        assert_eq!(Lifecycle::of(&["Factory:Ready"]), Some(Lifecycle::Ready));
        assert_eq!(Lifecycle::of(&["bug"]), None);
        assert_eq!(Lifecycle::of(&["factory:ready", "factory:running"]), None);
        assert_eq!(
            Lifecycle::of(&["factory:approve-plan", "factory:awaiting-approval"]),
            Some(Lifecycle::AwaitingApproval)
        );
    }

    #[test]
    fn claiming_swaps_ready_for_running_and_drops_stale_approval() {
        let change = claim(&["bug", "factory:ready", "factory:plan-approved"]);
        assert_eq!(change.add, ["factory:running"]);
        assert_eq!(change.remove, ["factory:ready", "factory:plan-approved"]);
    }

    #[test]
    fn moving_removes_every_other_lifecycle_label() {
        let change = move_to(
            &["factory:running", "factory:awaiting-approval"],
            Lifecycle::Blocked,
        );
        assert_eq!(change.add, ["factory:blocked"]);
        assert_eq!(
            change.remove,
            ["factory:running", "factory:awaiting-approval"]
        );
    }

    #[test]
    fn moving_to_the_current_state_changes_nothing() {
        assert!(move_to(&["factory:review", "bug"], Lifecycle::Review).is_empty());
    }

    #[test]
    fn usage_limit_requeue_returns_the_issue_to_ready() {
        let change = move_to(
            &["factory:running", "factory:approve-plan"],
            Lifecycle::Ready,
        );
        assert_eq!(change.add, ["factory:ready"]);
        assert_eq!(change.remove, ["factory:running"]);
    }
}
