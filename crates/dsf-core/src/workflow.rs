//! Workflows: the graph of stages a run walks through, written in TOML.
//!
//! A [`Workflow`] can only be obtained by parsing, which validates it, so
//! anything holding one can rely on every route resolving and every stage
//! being reachable.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::duration::{self, DurationError};

/// The built-in workflow, used when a repo doesn't provide its own.
pub const DEFAULT_TOML: &str = include_str!("../../../defaults/workflow.toml");

/// Command stages may only run scripts from this directory. dsf runs them
/// from the snapshot taken at the run's base commit, so the agent cannot
/// rewrite the check that judges its work.
pub const COMMAND_DIR: &str = ".factory/";

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_mins(30);
const SUCCESS_FAILURE: &[&str] = &["success", "failure"];
const APPROVED_REJECTED: &[&str] = &["approved", "rejected"];

/// A stage name: `[a-z][a-z0-9_-]*`, at most 32 characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StageId(String);

impl StageId {
    pub fn new(id: &str) -> Option<Self> {
        let mut bytes = id.bytes();
        let valid = id.len() <= 32
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        valid.then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StageId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(&id).ok_or_else(|| format!("`{id}` is not a valid stage id"))
    }
}

impl From<StageId> for String {
    fn from(id: StageId) -> Self {
        id.0
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// Open a draft PR. Publishing is built in, not a configurable stage.
    Publish,
    /// Stop, label the issue `factory:blocked` and explain why.
    Blocked,
}

impl Terminal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "@publish",
            Self::Blocked => "@blocked",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "@publish" => Some(Self::Publish),
            "@blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

impl fmt::Display for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an outcome routes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Stage(StageId),
    End(Terminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Agent,
    Command,
    Gate,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Gate => "gate",
        })
    }
}

/// What an agent stage may do to the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Read-only worktree, and only the Read, Grep and Glob tools.
    Read,
    /// May edit the worktree.
    Write,
}

/// A structured result Claude must return. Its `outcome` field becomes
/// the stage's outcome label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Schema {
    Plan,
    Review,
}

impl Schema {
    pub const fn outcomes(self) -> &'static [&'static str] {
        match self {
            Self::Plan => &["ready", "needs_decision"],
            Self::Review => &["approve", "changes_requested"],
        }
    }
}

/// Network access for a command stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    None,
    /// Through the allowlisting egress proxy, like agent stages.
    Proxy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageKind {
    Agent {
        mode: AgentMode,
        prompt: String,
        schema: Option<Schema>,
    },
    Command {
        run: String,
        timeout: Duration,
        network: Network,
    },
    Gate,
}

impl StageKind {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Agent { .. } => Kind::Agent,
            Self::Command { .. } => Kind::Command,
            Self::Gate => Kind::Gate,
        }
    }

    /// The outcome labels this stage can finish with.
    pub fn outcomes(&self) -> &'static [&'static str] {
        let schema = match self {
            Self::Agent { schema, .. } => *schema,
            _ => None,
        };
        outcomes_of(self.kind(), schema)
    }
}

fn outcomes_of(kind: Kind, schema: Option<Schema>) -> &'static [&'static str] {
    match (kind, schema) {
        (Kind::Agent, Some(schema)) => schema.outcomes(),
        (Kind::Agent | Kind::Command, _) => SUCCESS_FAILURE,
        (Kind::Gate, _) => APPROVED_REJECTED,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub kind: StageKind,
    /// How many times a run may enter this stage.
    pub max_visits: u32,
    /// Outcome label to target. Covers exactly the outcomes the stage produces.
    pub on: BTreeMap<String, Target>,
    /// Where the run goes if it would enter this stage beyond `max_visits`.
    pub exhausted: Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub(crate) start: StageId,
    pub(crate) stages: BTreeMap<StageId, Stage>,
}

impl Workflow {
    pub fn parse(text: &str) -> Result<Self, WorkflowError> {
        let raw: RawWorkflow = toml::from_str(text)?;
        raw.validate().map_err(WorkflowError::Invalid)
    }

    /// The built-in default workflow.
    pub fn builtin() -> Result<Self, WorkflowError> {
        Self::parse(DEFAULT_TOML)
    }

    pub fn start(&self) -> &StageId {
        &self.start
    }

    pub fn stages(&self) -> &BTreeMap<StageId, Stage> {
        &self.stages
    }

    pub fn stage(&self, id: &StageId) -> Option<&Stage> {
        self.stages.get(id)
    }

    /// The write stages a run can reach from the start without passing a
    /// gate: where an agent could change code before anyone approved a
    /// plan. A run that needs approval refuses a workflow with any.
    pub fn ungated_writes(&self) -> Vec<StageId> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([&self.start]);
        let mut writes = Vec::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            let Some(stage) = self.stages.get(id) else {
                continue;
            };
            match stage.kind {
                StageKind::Gate => continue,
                StageKind::Agent {
                    mode: AgentMode::Write,
                    ..
                } => writes.push(id.clone()),
                _ => {}
            }
            for target in stage.on.values() {
                if let Target::Stage(next) = target {
                    queue.push_back(next);
                }
            }
        }
        writes
    }

    /// The most stage visits any run of this workflow can make: the sum of
    /// every stage's `max_visits`. This bound is why every run terminates.
    pub fn visit_budget(&self) -> u32 {
        self.stages
            .values()
            .map(|stage| stage.max_visits)
            .fold(0, u32::saturating_add)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("not a valid workflow file: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid workflow:{}", crate::bullets(.0))]
    Invalid(Vec<Problem>),
}

/// One reason a workflow is invalid. Validation reports all of them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("unsupported version {0} (this dsf reads version 1)")]
    UnsupportedVersion(u32),
    #[error("the workflow has no stages")]
    NoStages,
    #[error("`{0}` is not a valid stage id (use [a-z][a-z0-9_-], at most 32 characters)")]
    InvalidStageId(String),
    #[error("`start` names `{0}`, which is not a stage")]
    UnknownStart(String),
    #[error("stage `{stage}`: {kind} stages need `{field}`")]
    MissingField {
        stage: StageId,
        kind: Kind,
        field: &'static str,
    },
    #[error("stage `{stage}`: `{field}` does not apply to {kind} stages")]
    FieldNotAllowed {
        stage: StageId,
        kind: Kind,
        field: &'static str,
    },
    #[error("stage `{stage}`: prompt `{prompt}` must be a plain file name ending in .md.j2")]
    InvalidPrompt { stage: StageId, prompt: String },
    #[error(
        "stage `{stage}`: `run` must be the path of a script under .factory/, with no arguments (got `{run}`)"
    )]
    InvalidCommand { stage: StageId, run: String },
    #[error("stage `{stage}`: timeout `{value}`: {reason}")]
    InvalidTimeout {
        stage: StageId,
        value: String,
        reason: DurationError,
    },
    #[error("stage `{stage}`: timeout must be greater than zero")]
    ZeroTimeout { stage: StageId },
    #[error("stage `{stage}`: max_visits must be at least 1")]
    ZeroVisits { stage: StageId },
    #[error(
        "stage `{stage}`: outcome `{outcome}` routes to `{target}`, which is neither a stage nor @publish/@blocked"
    )]
    UnknownTarget {
        stage: StageId,
        outcome: String,
        target: String,
    },
    #[error("stage `{stage}`: outcome `{outcome}` has no route in `on`")]
    MissingRoute {
        stage: StageId,
        outcome: &'static str,
    },
    #[error("stage `{stage}`: `on` routes `{outcome}`, which this stage never produces (it produces {})", .produces.join(", "))]
    UnexpectedOutcome {
        stage: StageId,
        outcome: String,
        produces: &'static [&'static str],
    },
    #[error("stage `{stage}`: `exhausted` must be @publish or @blocked (got `{target}`)")]
    InvalidExhausted { stage: StageId, target: String },
    #[error("stage `{0}` cannot be reached from `start`")]
    Unreachable(StageId),
    #[error("no path from `start` reaches @publish")]
    PublishUnreachable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    version: u32,
    start: String,
    #[serde(default)]
    stages: BTreeMap<String, RawStage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    kind: Kind,
    mode: Option<AgentMode>,
    prompt: Option<String>,
    schema: Option<Schema>,
    run: Option<String>,
    timeout: Option<String>,
    network: Option<Network>,
    max_visits: Option<u32>,
    #[serde(default)]
    on: BTreeMap<String, String>,
    exhausted: Option<String>,
}

impl RawWorkflow {
    fn validate(self) -> Result<Workflow, Vec<Problem>> {
        let mut problems = Vec::new();
        if self.version != 1 {
            problems.push(Problem::UnsupportedVersion(self.version));
        }
        if self.stages.is_empty() {
            problems.push(Problem::NoStages);
            return Err(problems);
        }
        // Later checks can't say anything useful about routes into a stage
        // whose name is unusable, so stop here if any are.
        let bad_ids: Vec<Problem> = self
            .stages
            .keys()
            .filter(|id| StageId::new(id).is_none())
            .map(|id| Problem::InvalidStageId(id.clone()))
            .collect();
        if !bad_ids.is_empty() {
            problems.extend(bad_ids);
            return Err(problems);
        }

        let ids: BTreeSet<String> = self.stages.keys().cloned().collect();
        if !ids.contains(&self.start) {
            problems.push(Problem::UnknownStart(self.start.clone()));
        }
        let mut stages = BTreeMap::new();
        for (id, raw) in self.stages {
            let id = StageId(id);
            if let Some(stage) = raw.into_stage(&id, &ids, &mut problems) {
                stages.insert(id, stage);
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }

        let start = StageId(self.start);
        check_graph(&start, &stages, &mut problems);
        if problems.is_empty() {
            Ok(Workflow { start, stages })
        } else {
            Err(problems)
        }
    }
}

impl RawStage {
    fn into_stage(
        self,
        id: &StageId,
        ids: &BTreeSet<String>,
        problems: &mut Vec<Problem>,
    ) -> Option<Stage> {
        let before = problems.len();
        let kind = self.kind;
        let mut reject = |field: &'static str, present: bool| {
            if present {
                problems.push(Problem::FieldNotAllowed {
                    stage: id.clone(),
                    kind,
                    field,
                });
            }
        };
        match kind {
            Kind::Agent => {
                reject("run", self.run.is_some());
                reject("timeout", self.timeout.is_some());
                reject("network", self.network.is_some());
            }
            Kind::Command => {
                reject("mode", self.mode.is_some());
                reject("prompt", self.prompt.is_some());
                reject("schema", self.schema.is_some());
            }
            Kind::Gate => {
                reject("mode", self.mode.is_some());
                reject("prompt", self.prompt.is_some());
                reject("schema", self.schema.is_some());
                reject("run", self.run.is_some());
                reject("timeout", self.timeout.is_some());
                reject("network", self.network.is_some());
            }
        }

        let stage_kind = match kind {
            Kind::Agent => self.agent_kind(id, problems),
            Kind::Command => self.command_kind(id, problems),
            Kind::Gate => Some(StageKind::Gate),
        };

        let max_visits = self.max_visits.unwrap_or(1);
        if max_visits == 0 {
            problems.push(Problem::ZeroVisits { stage: id.clone() });
        }
        let exhausted = match self.exhausted.as_deref() {
            None => Some(Terminal::Blocked),
            Some(target) => Terminal::parse(target).or_else(|| {
                problems.push(Problem::InvalidExhausted {
                    stage: id.clone(),
                    target: target.to_owned(),
                });
                None
            }),
        };
        let on = route(id, kind, self.schema, &self.on, ids, problems);

        match (stage_kind, exhausted) {
            (Some(kind), Some(exhausted)) if problems.len() == before => Some(Stage {
                kind,
                max_visits,
                on,
                exhausted,
            }),
            _ => None,
        }
    }

    fn agent_kind(&self, id: &StageId, problems: &mut Vec<Problem>) -> Option<StageKind> {
        let mode = require(id, Kind::Agent, "mode", self.mode, problems);
        let prompt = require(id, Kind::Agent, "prompt", self.prompt.clone(), problems);
        if let Some(prompt) = &prompt
            && !is_prompt_name(prompt)
        {
            problems.push(Problem::InvalidPrompt {
                stage: id.clone(),
                prompt: prompt.clone(),
            });
        }
        Some(StageKind::Agent {
            mode: mode?,
            prompt: prompt?,
            schema: self.schema,
        })
    }

    fn command_kind(&self, id: &StageId, problems: &mut Vec<Problem>) -> Option<StageKind> {
        let run = require(id, Kind::Command, "run", self.run.clone(), problems);
        if let Some(run) = &run
            && !is_factory_script(run)
        {
            problems.push(Problem::InvalidCommand {
                stage: id.clone(),
                run: run.clone(),
            });
        }
        let timeout = match self.timeout.as_deref() {
            None => Some(DEFAULT_COMMAND_TIMEOUT),
            Some(text) => match duration::parse(text) {
                Ok(timeout) if timeout.is_zero() => {
                    problems.push(Problem::ZeroTimeout { stage: id.clone() });
                    None
                }
                Ok(timeout) => Some(timeout),
                Err(reason) => {
                    problems.push(Problem::InvalidTimeout {
                        stage: id.clone(),
                        value: text.to_owned(),
                        reason,
                    });
                    None
                }
            },
        };
        Some(StageKind::Command {
            run: run?,
            timeout: timeout?,
            network: self.network.unwrap_or_default(),
        })
    }
}

fn require<T>(
    id: &StageId,
    kind: Kind,
    field: &'static str,
    value: Option<T>,
    problems: &mut Vec<Problem>,
) -> Option<T> {
    if value.is_none() {
        problems.push(Problem::MissingField {
            stage: id.clone(),
            kind,
            field,
        });
    }
    value
}

fn route(
    id: &StageId,
    kind: Kind,
    schema: Option<Schema>,
    raw: &BTreeMap<String, String>,
    ids: &BTreeSet<String>,
    problems: &mut Vec<Problem>,
) -> BTreeMap<String, Target> {
    let produces = outcomes_of(kind, schema);
    let mut on = BTreeMap::new();
    for (outcome, target) in raw {
        if !produces.contains(&outcome.as_str()) {
            problems.push(Problem::UnexpectedOutcome {
                stage: id.clone(),
                outcome: outcome.clone(),
                produces,
            });
        } else if let Some(terminal) = Terminal::parse(target) {
            on.insert(outcome.clone(), Target::End(terminal));
        } else if ids.contains(target) {
            on.insert(outcome.clone(), Target::Stage(StageId(target.clone())));
        } else {
            problems.push(Problem::UnknownTarget {
                stage: id.clone(),
                outcome: outcome.clone(),
                target: target.clone(),
            });
        }
    }
    for outcome in produces {
        if !raw.contains_key(*outcome) {
            problems.push(Problem::MissingRoute {
                stage: id.clone(),
                outcome,
            });
        }
    }
    on
}

/// Every stage must be reachable from `start`, and some path must publish,
/// either through a route or by exhausting a stage whose `exhausted` is
/// `@publish`.
fn check_graph(start: &StageId, stages: &BTreeMap<StageId, Stage>, problems: &mut Vec<Problem>) {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start]);
    let mut publishes = false;
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let Some(stage) = stages.get(id) else {
            continue;
        };
        publishes |= stage.exhausted == Terminal::Publish;
        for target in stage.on.values() {
            match target {
                Target::Stage(next) => queue.push_back(next),
                Target::End(terminal) => publishes |= *terminal == Terminal::Publish,
            }
        }
    }
    for id in stages.keys() {
        if !seen.contains(id) {
            problems.push(Problem::Unreachable(id.clone()));
        }
    }
    if !publishes {
        problems.push(Problem::PublishUnreachable);
    }
}

/// Prompts are looked up by name in the run's snapshot, so a name must not
/// be able to point anywhere else.
fn is_prompt_name(name: &str) -> bool {
    name.strip_suffix(".md.j2").is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    })
}

/// A relative path under [`COMMAND_DIR`] with plain components, run as a
/// single executable with no shell and no arguments.
fn is_factory_script(run: &str) -> bool {
    run.strip_prefix(COMMAND_DIR).is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems(toml: &str) -> Vec<Problem> {
        match Workflow::parse(toml) {
            Err(WorkflowError::Invalid(problems)) => problems,
            other => panic!("expected validation problems, got {other:?}"),
        }
    }

    fn id(name: &str) -> StageId {
        StageId::new(name).unwrap()
    }

    const MINIMAL: &str = r#"
        version = 1
        start = "build"
        [stages.build]
        kind = "command"
        run = ".factory/verify.sh"
        on = { success = "@publish", failure = "@blocked" }
    "#;

    #[test]
    fn builtin_workflow_is_valid_with_a_budget_of_13_visits() {
        let workflow = Workflow::builtin().unwrap();
        assert_eq!(workflow.start(), &id("plan"));
        assert_eq!(workflow.stages().len(), 6);
        assert_eq!(workflow.visit_budget(), 13);
    }

    #[test]
    fn a_gate_must_come_before_any_write_stage_to_count() {
        assert_eq!(Workflow::builtin().unwrap().ungated_writes(), []);
        let skips_the_gate = Workflow::parse(
            r#"
            version = 1
            start = "plan"
            [stages.plan]
            kind = "agent"
            mode = "read"
            prompt = "plan.md.j2"
            schema = "plan"
            on = { ready = "implement", needs_decision = "approve" }
            [stages.approve]
            kind = "gate"
            on = { approved = "implement", rejected = "@blocked" }
            [stages.implement]
            kind = "agent"
            mode = "write"
            prompt = "implement.md.j2"
            on = { success = "verify", failure = "@blocked" }
            [stages.verify]
            kind = "command"
            run = ".factory/verify.sh"
            on = { success = "@publish", failure = "fix" }
            [stages.fix]
            kind = "agent"
            mode = "write"
            prompt = "fix.md.j2"
            on = { success = "verify", failure = "@blocked" }
            "#,
        )
        .unwrap();
        assert_eq!(
            skips_the_gate.ungated_writes(),
            [id("implement"), id("fix")]
        );
        assert_eq!(Workflow::parse(MINIMAL).unwrap().ungated_writes(), []);
    }

    #[test]
    fn builtin_workflow_has_the_agreed_shape() {
        let workflow = Workflow::builtin().unwrap();
        let verify = workflow.stage(&id("verify")).unwrap();
        assert_eq!(
            verify.kind,
            StageKind::Command {
                run: ".factory/verify.sh".into(),
                timeout: Duration::from_mins(30),
                network: Network::None,
            }
        );
        assert_eq!(verify.max_visits, 5);
        assert_eq!(verify.on["failure"], Target::Stage(id("fix")));

        let review = workflow.stage(&id("review")).unwrap();
        assert_eq!(review.exhausted, Terminal::Publish);
        assert_eq!(review.on["approve"], Target::End(Terminal::Publish));

        let plan = workflow.stage(&id("plan")).unwrap();
        assert!(matches!(
            plan.kind,
            StageKind::Agent {
                mode: AgentMode::Read,
                schema: Some(Schema::Plan),
                ..
            }
        ));
    }

    #[test]
    fn defaults_apply_to_optional_fields() {
        let workflow = Workflow::parse(MINIMAL).unwrap();
        let build = workflow.stage(&id("build")).unwrap();
        assert_eq!(build.max_visits, 1);
        assert_eq!(build.exhausted, Terminal::Blocked);
    }

    #[test]
    fn unknown_fields_and_values_are_toml_errors() {
        for toml in [
            "version = 1\nstart = \"a\"\nimage = \"evil\"",
            "version = 1\nstart = \"a\"\n[stages.a]\nkind = \"agent\"\nmounts = [\"/\"]",
            "version = 1\nstart = \"a\"\n[stages.a]\nkind = \"shell\"",
            "version = 1\nstart = \"a\"\n[stages.a]\nkind = \"agent\"\nmode = \"admin\"",
            "version = 1\nstart = \"a\"\n[stages.a]\nkind = \"agent\"\nschema = \"custom\"",
        ] {
            assert!(
                matches!(Workflow::parse(toml), Err(WorkflowError::Toml(_))),
                "{toml}"
            );
        }
    }

    #[test]
    fn reports_version_empty_and_bad_ids() {
        assert_eq!(
            problems("version = 2\nstart = \"a\""),
            [Problem::UnsupportedVersion(2), Problem::NoStages]
        );
        assert_eq!(
            problems(
                "version = 1\nstart = \"Build\"\n[stages.Build]\nkind = \"gate\"\non = { approved = \"@publish\", rejected = \"@blocked\" }"
            ),
            [Problem::InvalidStageId("Build".into())]
        );
    }

    #[test]
    fn reports_missing_and_misplaced_fields() {
        let found = problems(
            r#"
            version = 1
            start = "a"
            [stages.a]
            kind = "agent"
            run = ".factory/x.sh"
            on = { success = "b", failure = "@blocked" }
            [stages.b]
            kind = "command"
            prompt = "x.md.j2"
            on = { success = "@publish", failure = "@blocked" }
            "#,
        );
        assert_eq!(
            found,
            [
                Problem::FieldNotAllowed {
                    stage: id("a"),
                    kind: Kind::Agent,
                    field: "run"
                },
                Problem::MissingField {
                    stage: id("a"),
                    kind: Kind::Agent,
                    field: "mode"
                },
                Problem::MissingField {
                    stage: id("a"),
                    kind: Kind::Agent,
                    field: "prompt"
                },
                Problem::FieldNotAllowed {
                    stage: id("b"),
                    kind: Kind::Command,
                    field: "prompt"
                },
                Problem::MissingField {
                    stage: id("b"),
                    kind: Kind::Command,
                    field: "run"
                },
            ]
        );
    }

    #[test]
    fn prompts_must_be_plain_template_names() {
        for prompt in [
            "../secrets.md.j2",
            "sub/plan.md.j2",
            "plan.txt",
            ".md.j2",
            "Plan.md.j2",
        ] {
            let toml = format!(
                "version = 1\nstart = \"a\"\n[stages.a]\nkind = \"agent\"\nmode = \"read\"\nprompt = \"{prompt}\"\non = {{ success = \"@publish\", failure = \"@blocked\" }}"
            );
            assert_eq!(
                problems(&toml),
                [Problem::InvalidPrompt {
                    stage: id("a"),
                    prompt: prompt.into()
                }],
                "{prompt}"
            );
        }
    }

    #[test]
    fn commands_must_be_scripts_under_factory() {
        for run in [
            "make test",
            "verify.sh",
            ".factory/../Makefile",
            ".factory/",
            ".factory//x.sh",
            ".factory/verify.sh --fast",
            "/bin/sh",
            ".factory/./verify.sh",
        ] {
            let toml = MINIMAL.replace(".factory/verify.sh", run);
            assert_eq!(
                problems(&toml),
                [Problem::InvalidCommand {
                    stage: id("build"),
                    run: run.into()
                }],
                "{run}"
            );
        }
        let nested = MINIMAL.replace(".factory/verify.sh", ".factory/ci/check_all-v2.sh");
        assert!(Workflow::parse(&nested).is_ok());
    }

    #[test]
    fn timeouts_and_visits_must_be_positive() {
        let toml = MINIMAL.replace(
            "kind = \"command\"",
            "kind = \"command\"\ntimeout = \"0s\"\nmax_visits = 0",
        );
        assert_eq!(
            problems(&toml),
            [
                Problem::ZeroTimeout { stage: id("build") },
                Problem::ZeroVisits { stage: id("build") },
            ]
        );
        let toml = MINIMAL.replace(
            "kind = \"command\"",
            "kind = \"command\"\ntimeout = \"soon\"",
        );
        assert!(matches!(
            problems(&toml).as_slice(),
            [Problem::InvalidTimeout { value, .. }] if value == "soon"
        ));
    }

    #[test]
    fn routes_must_cover_exactly_the_stage_outcomes() {
        let found = problems(
            r#"
            version = 1
            start = "review"
            [stages.review]
            kind = "agent"
            mode = "read"
            prompt = "review.md.j2"
            schema = "review"
            on = { approve = "@publish", success = "@publish", rejected = "nowhere" }
            "#,
        );
        assert_eq!(
            found,
            [
                Problem::UnexpectedOutcome {
                    stage: id("review"),
                    outcome: "rejected".into(),
                    produces: Schema::Review.outcomes(),
                },
                Problem::UnexpectedOutcome {
                    stage: id("review"),
                    outcome: "success".into(),
                    produces: Schema::Review.outcomes(),
                },
                Problem::MissingRoute {
                    stage: id("review"),
                    outcome: "changes_requested"
                },
            ]
        );
    }

    #[test]
    fn targets_must_exist_and_exhausted_must_be_terminal() {
        let found = problems(
            r#"
            version = 1
            start = "gate"
            [stages.gate]
            kind = "gate"
            exhausted = "gate"
            on = { approved = "@merge", rejected = "elsewhere" }
            "#,
        );
        assert_eq!(
            found,
            [
                Problem::InvalidExhausted {
                    stage: id("gate"),
                    target: "gate".into()
                },
                Problem::UnknownTarget {
                    stage: id("gate"),
                    outcome: "approved".into(),
                    target: "@merge".into(),
                },
                Problem::UnknownTarget {
                    stage: id("gate"),
                    outcome: "rejected".into(),
                    target: "elsewhere".into(),
                },
            ]
        );
    }

    #[test]
    fn start_must_exist() {
        let toml = MINIMAL.replace("start = \"build\"", "start = \"plan\"");
        assert_eq!(problems(&toml), [Problem::UnknownStart("plan".into())]);
    }

    #[test]
    fn every_stage_must_be_reachable_and_publish_must_be_too() {
        let found = problems(
            r#"
            version = 1
            start = "a"
            [stages.a]
            kind = "gate"
            on = { approved = "a", rejected = "@blocked" }
            [stages.orphan]
            kind = "gate"
            on = { approved = "@publish", rejected = "@blocked" }
            "#,
        );
        assert_eq!(
            found,
            [
                Problem::Unreachable(id("orphan")),
                Problem::PublishUnreachable
            ]
        );
    }

    #[test]
    fn publishing_by_exhaustion_counts_as_reachable() {
        let toml = r#"
            version = 1
            start = "loop"
            [stages.loop]
            kind = "gate"
            max_visits = 3
            exhausted = "@publish"
            on = { approved = "loop", rejected = "@blocked" }
        "#;
        assert!(Workflow::parse(toml).is_ok());
    }

    #[test]
    fn invalid_workflow_message_lists_every_problem() {
        let error = Workflow::parse("version = 3\nstart = \"x\"").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid workflow:\n  - unsupported version 3 (this dsf reads version 1)\n  - the workflow has no stages"
        );
    }

    #[test]
    fn stage_ids_follow_the_naming_rule() {
        assert!(StageId::new("fix-2_b").is_some());
        for bad in [
            "",
            "2fix",
            "Fix",
            "-fix",
            "fix stage",
            "@publish",
            &"a".repeat(33),
        ] {
            assert!(StageId::new(bad).is_none(), "{bad}");
        }
    }
}
