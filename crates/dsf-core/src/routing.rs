//! Moving a run through its workflow: visit caps, outcome routing, the
//! repeat-failure breaker and interruption accounting.
//!
//! [`Progress`] is the whole of a run's routing state. The engine persists
//! it at every stage boundary and asks it what to do next.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::signature::FailureSignature;
use crate::workflow::{StageId, Target, Terminal, Workflow};

/// A stage interrupted this many times in a row blocks the run, so a stage
/// that reliably crashes the host can't loop forever.
const MAX_CONSECUTIVE_INTERRUPTIONS: u32 = 2;

/// What the engine should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Run(StageId),
    End(End),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub terminal: Terminal,
    pub reason: EndReason,
}

/// Why a run ended. Rendered into the PR body or the blocked comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The stage's outcome routes to the terminal.
    Routed { stage: StageId, outcome: String },
    /// The run would have entered the stage more than `max_visits` times.
    Exhausted { stage: StageId },
    /// The stage failed twice in a row with the same output.
    RepeatedFailure { stage: StageId },
    /// The stage was interrupted twice in a row.
    Interrupted { stage: StageId },
    /// The stage reported an outcome its routes don't cover. Validation
    /// makes this unreachable for well-behaved stages; it fails closed.
    UnknownOutcome { stage: StageId, outcome: String },
    /// The engine reported a stage the workflow doesn't have.
    UnknownStage { stage: StageId },
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Routed { stage, outcome } => write!(f, "`{stage}` finished with `{outcome}`"),
            Self::Exhausted { stage } => write!(f, "`{stage}` reached its visit limit"),
            Self::RepeatedFailure { stage } => {
                write!(f, "`{stage}` failed the same way twice in a row")
            }
            Self::Interrupted { stage } => write!(f, "`{stage}` was interrupted twice in a row"),
            Self::UnknownOutcome { stage, outcome } => {
                write!(f, "`{stage}` reported `{outcome}`, which has no route")
            }
            Self::UnknownStage { stage } => write!(f, "`{stage}` is not in the workflow"),
        }
    }
}

/// A run's routing state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    visits: BTreeMap<StageId, u32>,
    last_failures: BTreeMap<StageId, FailureSignature>,
    interruptions: u32,
}

impl Progress {
    /// Enters the workflow's start stage.
    pub fn begin(&mut self, workflow: &Workflow) -> Next {
        self.enter(workflow, workflow.start())
    }

    /// Records that `stage` finished with `outcome` and decides what's next.
    ///
    /// `failure` is the signature of a failed command's output, if any.
    /// When it matches the stage's previous failure, the run is blocked
    /// rather than spending its remaining visits on fixes that change
    /// nothing. Any other result clears the stage's remembered failure.
    pub fn complete(
        &mut self,
        workflow: &Workflow,
        stage: &StageId,
        outcome: &str,
        failure: Option<FailureSignature>,
    ) -> Next {
        self.interruptions = 0;
        let Some(definition) = workflow.stage(stage) else {
            return blocked(EndReason::UnknownStage {
                stage: stage.clone(),
            });
        };
        match failure {
            Some(signature) => {
                if self.last_failures.insert(stage.clone(), signature) == Some(signature) {
                    return blocked(EndReason::RepeatedFailure {
                        stage: stage.clone(),
                    });
                }
            }
            None => {
                self.last_failures.remove(stage);
            }
        }
        match definition.on.get(outcome) {
            None => blocked(EndReason::UnknownOutcome {
                stage: stage.clone(),
                outcome: outcome.to_owned(),
            }),
            Some(Target::End(terminal)) => Next::End(End {
                terminal: *terminal,
                reason: EndReason::Routed {
                    stage: stage.clone(),
                    outcome: outcome.to_owned(),
                },
            }),
            Some(Target::Stage(next)) => self.enter(workflow, next),
        }
    }

    /// Records that `stage` was interrupted (a crash, a restart or a lost
    /// container) and decides what's next. The stage re-runs within the
    /// same visit unless this keeps happening.
    pub fn interrupted(&mut self, stage: &StageId) -> Next {
        self.interruptions += 1;
        if self.interruptions >= MAX_CONSECUTIVE_INTERRUPTIONS {
            blocked(EndReason::Interrupted {
                stage: stage.clone(),
            })
        } else {
            Next::Run(stage.clone())
        }
    }

    pub fn visits(&self, stage: &StageId) -> u32 {
        self.visits.get(stage).copied().unwrap_or(0)
    }

    pub fn total_visits(&self) -> u32 {
        self.visits
            .values()
            .fold(0, |sum, n| sum.saturating_add(*n))
    }

    fn enter(&mut self, workflow: &Workflow, stage: &StageId) -> Next {
        let Some(definition) = workflow.stage(stage) else {
            return blocked(EndReason::UnknownStage {
                stage: stage.clone(),
            });
        };
        let visits = self.visits.entry(stage.clone()).or_insert(0);
        if *visits >= definition.max_visits {
            return Next::End(End {
                terminal: definition.exhausted,
                reason: EndReason::Exhausted {
                    stage: stage.clone(),
                },
            });
        }
        *visits += 1;
        Next::Run(stage.clone())
    }
}

fn blocked(reason: EndReason) -> Next {
    Next::End(End {
        terminal: Terminal::Blocked,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{Stage, StageKind};
    use proptest::prelude::*;

    fn id(name: &str) -> StageId {
        StageId::new(name).unwrap()
    }

    fn sig(text: &str) -> FailureSignature {
        FailureSignature::of(text)
    }

    /// Drives the built-in workflow through `outcomes`, returning the stages
    /// run and how the run ended.
    fn drive(outcomes: &[&str]) -> (Vec<String>, End) {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        let mut next = progress.begin(&workflow);
        let mut ran = Vec::new();
        let mut outcomes = outcomes.iter();
        loop {
            match next {
                Next::Run(stage) => {
                    ran.push(stage.to_string());
                    let outcome = outcomes.next().expect("ran out of outcomes");
                    next = progress.complete(&workflow, &stage, outcome, None);
                }
                Next::End(end) => {
                    assert!(outcomes.next().is_none(), "outcomes left over");
                    return (ran, end);
                }
            }
        }
    }

    #[test]
    fn happy_path_publishes_after_one_review() {
        let (ran, end) = drive(&["ready", "approved", "success", "success", "approve"]);
        assert_eq!(ran, ["plan", "approve", "implement", "verify", "review"]);
        assert_eq!(end.terminal, Terminal::Publish);
        assert_eq!(
            end.reason,
            EndReason::Routed {
                stage: id("review"),
                outcome: "approve".into()
            }
        );
    }

    #[test]
    fn plan_needing_a_decision_blocks() {
        let (ran, end) = drive(&["needs_decision"]);
        assert_eq!(ran, ["plan"]);
        assert_eq!(end.terminal, Terminal::Blocked);
    }

    #[test]
    fn verify_fix_loop_blocks_when_fix_runs_out() {
        let (ran, end) = drive(&[
            "ready", "approved", "success", // plan, approve, implement
            "failure", "success", // verify 1, fix 1
            "failure", "success", // verify 2, fix 2
            "failure", "success", // verify 3, fix 3
            "failure", // verify 4: fix is exhausted
        ]);
        assert_eq!(ran.len(), 10);
        assert_eq!(end.terminal, Terminal::Blocked);
        assert_eq!(end.reason, EndReason::Exhausted { stage: id("fix") });
    }

    #[test]
    fn review_that_never_approves_publishes_when_exhausted() {
        let (ran, end) = drive(&[
            "ready",
            "approved",
            "success", //
            "success",
            "changes_requested",
            "success", // verify, review 1, fix 1
            "success",
            "changes_requested",
            "success", // verify, review 2, fix 2
            "success", // verify: review is exhausted
        ]);
        assert_eq!(ran.last().map(String::as_str), Some("verify"));
        assert_eq!(end.terminal, Terminal::Publish);
        assert_eq!(
            end.reason,
            EndReason::Exhausted {
                stage: id("review")
            }
        );
    }

    #[test]
    fn same_verify_failure_twice_blocks() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        let verify = id("verify");
        progress.begin(&workflow);
        let next = progress.complete(&workflow, &verify, "failure", Some(sig("test a failed")));
        assert_eq!(next, Next::Run(id("fix")));
        progress.complete(&workflow, &id("fix"), "success", None);
        let next = progress.complete(&workflow, &verify, "failure", Some(sig("test a failed")));
        assert_eq!(
            next,
            Next::End(End {
                terminal: Terminal::Blocked,
                reason: EndReason::RepeatedFailure { stage: verify }
            })
        );
    }

    #[test]
    fn a_different_failure_or_a_success_resets_the_breaker() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        let verify = id("verify");
        progress.begin(&workflow);
        progress.complete(&workflow, &verify, "failure", Some(sig("test a failed")));
        let next = progress.complete(&workflow, &verify, "failure", Some(sig("test b failed")));
        assert_eq!(next, Next::Run(id("fix")));
        progress.complete(&workflow, &verify, "success", None);
        let next = progress.complete(&workflow, &verify, "failure", Some(sig("test b failed")));
        assert_eq!(next, Next::Run(id("fix")));
    }

    #[test]
    fn one_interruption_reruns_the_stage_without_a_visit_and_two_block() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        let plan = id("plan");
        assert_eq!(progress.begin(&workflow), Next::Run(plan.clone()));
        assert_eq!(progress.interrupted(&plan), Next::Run(plan.clone()));
        assert_eq!(progress.visits(&plan), 1);
        assert_eq!(
            progress.interrupted(&plan),
            Next::End(End {
                terminal: Terminal::Blocked,
                reason: EndReason::Interrupted { stage: plan }
            })
        );
    }

    #[test]
    fn a_completed_stage_resets_the_interruption_count() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        progress.begin(&workflow);
        progress.interrupted(&id("plan"));
        progress.complete(&workflow, &id("plan"), "ready", None);
        assert_eq!(
            progress.interrupted(&id("approve")),
            Next::Run(id("approve"))
        );
    }

    #[test]
    fn unknown_outcomes_and_stages_fail_closed() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        progress.begin(&workflow);
        let next = progress.complete(&workflow, &id("plan"), "yolo", None);
        assert!(matches!(
            next,
            Next::End(End {
                terminal: Terminal::Blocked,
                reason: EndReason::UnknownOutcome { .. }
            })
        ));
        let next = progress.complete(&workflow, &id("deploy"), "success", None);
        assert!(matches!(
            next,
            Next::End(End {
                terminal: Terminal::Blocked,
                reason: EndReason::UnknownStage { .. }
            })
        ));
    }

    #[test]
    fn end_reasons_read_well_in_a_comment() {
        let reasons = [
            EndReason::Routed {
                stage: id("plan"),
                outcome: "needs_decision".into(),
            },
            EndReason::Exhausted { stage: id("fix") },
            EndReason::RepeatedFailure {
                stage: id("verify"),
            },
        ];
        let messages: Vec<String> = reasons.iter().map(ToString::to_string).collect();
        assert_eq!(
            messages,
            [
                "`plan` finished with `needs_decision`",
                "`fix` reached its visit limit",
                "`verify` failed the same way twice in a row",
            ]
        );
    }

    #[test]
    fn progress_round_trips_through_serde() {
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        progress.begin(&workflow);
        progress.complete(&workflow, &id("plan"), "ready", None);
        progress.complete(
            &workflow,
            &id("verify"),
            "failure",
            Some(sig("error[E0308]")),
        );
        let toml = toml::to_string(&progress).unwrap();
        assert_eq!(toml::from_str::<Progress>(&toml).unwrap(), progress);
    }

    /// Every path through the built-in workflow, explored exhaustively,
    /// ends within its visit budget. Returns the longest path's length.
    #[test]
    fn every_path_through_the_builtin_workflow_terminates_within_budget() {
        fn longest(workflow: &Workflow, progress: &Progress, stage: &StageId, depth: u32) -> u32 {
            let outcomes = workflow.stage(stage).unwrap().kind.outcomes();
            outcomes
                .iter()
                .map(|outcome| {
                    let mut branch = progress.clone();
                    match branch.complete(workflow, stage, outcome, None) {
                        Next::Run(next) => longest(workflow, &branch, &next, depth + 1),
                        Next::End(_) => depth,
                    }
                })
                .max()
                .unwrap()
        }
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        let Next::Run(start) = progress.begin(&workflow) else {
            panic!("the built-in workflow should start by running a stage");
        };
        let worst = longest(&workflow, &progress, &start, 1);
        assert!(worst <= workflow.visit_budget());
        assert_eq!(worst, 12, "the longest run of the default workflow changed");
    }

    /// An arbitrary graph: up to six stages of random kinds, visit caps and
    /// routes, built directly so it needn't pass reachability checks.
    fn arbitrary_workflow() -> impl Strategy<Value = Workflow> {
        (1usize..=6)
            .prop_flat_map(|count| {
                let stage = (
                    0usize..3,
                    1u32..=4,
                    any::<bool>(),
                    proptest::collection::vec(0..count + 2, 2),
                );
                proptest::collection::vec(stage, count)
            })
            .prop_map(|specs| {
                let count = specs.len();
                let target = |index: usize| match index {
                    i if i < count => Target::Stage(id(&format!("s{i}"))),
                    i if i == count => Target::End(Terminal::Publish),
                    _ => Target::End(Terminal::Blocked),
                };
                let stages = specs
                    .into_iter()
                    .enumerate()
                    .map(
                        |(index, (kind, max_visits, publish_when_exhausted, routes))| {
                            let kind = match kind {
                                0 => StageKind::Gate,
                                1 => StageKind::Command {
                                    run: ".factory/verify.sh".into(),
                                    timeout: std::time::Duration::from_secs(60),
                                    network: crate::workflow::Network::None,
                                },
                                _ => StageKind::Agent {
                                    mode: crate::workflow::AgentMode::Write,
                                    prompt: "x.md.j2".into(),
                                    schema: None,
                                },
                            };
                            let on = kind
                                .outcomes()
                                .iter()
                                .zip(routes)
                                .map(|(outcome, route)| ((*outcome).to_owned(), target(route)))
                                .collect();
                            let exhausted = if publish_when_exhausted {
                                Terminal::Publish
                            } else {
                                Terminal::Blocked
                            };
                            (
                                id(&format!("s{index}")),
                                Stage {
                                    kind,
                                    max_visits,
                                    on,
                                    exhausted,
                                },
                            )
                        },
                    )
                    .collect();
                Workflow {
                    start: id("s0"),
                    stages,
                }
            })
    }

    proptest! {
        /// Whatever the graph and whatever the outcomes, a run ends after
        /// at most Σ max_visits stage runs, and never exceeds a stage's cap.
        #[test]
        fn every_run_terminates_within_the_visit_budget(
            workflow in arbitrary_workflow(),
            choices in proptest::collection::vec(any::<prop::sample::Index>(), 64),
        ) {
            let budget = workflow.visit_budget();
            let mut progress = Progress::default();
            let mut next = progress.begin(&workflow);
            let mut runs = 0;
            let mut choices = choices.iter().cycle();
            while let Next::Run(stage) = next {
                runs += 1;
                prop_assert!(runs <= budget);
                let definition = workflow.stage(&stage).unwrap();
                prop_assert!(progress.visits(&stage) <= definition.max_visits);
                let outcomes = definition.kind.outcomes();
                let outcome = outcomes[choices.next().unwrap().index(outcomes.len())];
                next = progress.complete(&workflow, &stage, outcome, None);
            }
            prop_assert_eq!(progress.total_visits(), runs);
        }
    }
}
