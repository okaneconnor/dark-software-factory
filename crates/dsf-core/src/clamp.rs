//! Operator caps on what a repo's own workflow may ask for.
//!
//! A repo can only tune visit counts and timeouts, and only downwards from
//! the operator's caps. Everything else that matters for safety (images,
//! mounts, limits, agent networking, protected paths) isn't expressible in
//! a workflow file at all.

use std::fmt;
use std::time::Duration;

use crate::duration;
use crate::workflow::{StageId, StageKind, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// The most visits any one stage may have.
    pub max_visits: u32,
    /// The longest a command stage may run.
    pub command_timeout: Duration,
    /// The most stages a workflow may have.
    pub max_stages: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_visits: 5,
            command_timeout: Duration::from_hours(1),
            max_stages: 16,
        }
    }
}

/// A value the operator's caps lowered. Reported on the run so nobody
/// wonders why a repo's setting had no effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adjustment {
    Visits {
        stage: StageId,
        requested: u32,
        allowed: u32,
    },
    Timeout {
        stage: StageId,
        requested: Duration,
        allowed: Duration,
    },
}

impl fmt::Display for Adjustment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Visits {
                stage,
                requested,
                allowed,
            } => write!(
                f,
                "stage `{stage}`: max_visits {requested} lowered to {allowed}"
            ),
            Self::Timeout {
                stage,
                requested,
                allowed,
            } => write!(
                f,
                "stage `{stage}`: timeout {} lowered to {}",
                duration::format(*requested),
                duration::format(*allowed)
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the workflow has {stages} stages; the operator allows at most {max}")]
pub struct TooManyStages {
    pub stages: usize,
    pub max: usize,
}

impl Workflow {
    /// Applies the operator's caps. Lowering numbers can't invalidate a
    /// workflow, so the result is still valid. A workflow with too many
    /// stages is refused outright, since there's no sensible way to trim it.
    pub fn clamp(mut self, caps: &Caps) -> Result<(Self, Vec<Adjustment>), TooManyStages> {
        if self.stages.len() > caps.max_stages {
            return Err(TooManyStages {
                stages: self.stages.len(),
                max: caps.max_stages,
            });
        }
        let mut adjustments = Vec::new();
        for (id, stage) in &mut self.stages {
            if stage.max_visits > caps.max_visits {
                adjustments.push(Adjustment::Visits {
                    stage: id.clone(),
                    requested: stage.max_visits,
                    allowed: caps.max_visits,
                });
                stage.max_visits = caps.max_visits;
            }
            if let StageKind::Command { timeout, .. } = &mut stage.kind
                && *timeout > caps.command_timeout
            {
                adjustments.push(Adjustment::Timeout {
                    stage: id.clone(),
                    requested: *timeout,
                    allowed: caps.command_timeout,
                });
                *timeout = caps.command_timeout;
            }
        }
        Ok((self, adjustments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> StageId {
        StageId::new(name).unwrap()
    }

    #[test]
    fn builtin_workflow_fits_the_default_caps_untouched() {
        let workflow = Workflow::builtin().unwrap();
        let (clamped, adjustments) = workflow.clone().clamp(&Caps::default()).unwrap();
        assert_eq!(adjustments, Vec::<Adjustment>::new());
        assert_eq!(clamped, workflow);
    }

    #[test]
    fn lowers_visits_and_timeouts_and_reports_it() {
        let greedy = Workflow::builtin()
            .unwrap()
            .clamp(&Caps {
                max_visits: 2,
                command_timeout: Duration::from_mins(10),
                max_stages: 16,
            })
            .unwrap();
        let (workflow, adjustments) = greedy;
        assert_eq!(workflow.stage(&id("verify")).unwrap().max_visits, 2);
        assert_eq!(workflow.stage(&id("fix")).unwrap().max_visits, 2);
        assert_eq!(workflow.visit_budget(), 9);
        let messages: Vec<String> = adjustments.iter().map(ToString::to_string).collect();
        assert_eq!(
            messages,
            [
                "stage `fix`: max_visits 3 lowered to 2",
                "stage `verify`: max_visits 5 lowered to 2",
                "stage `verify`: timeout 30m lowered to 10m",
            ]
        );
    }

    #[test]
    fn refuses_workflows_with_too_many_stages() {
        let caps = Caps {
            max_stages: 3,
            ..Caps::default()
        };
        assert_eq!(
            Workflow::builtin().unwrap().clamp(&caps).unwrap_err(),
            TooManyStages { stages: 6, max: 3 }
        );
    }
}
