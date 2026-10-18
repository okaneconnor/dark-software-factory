//! The engine: runs one issue through its workflow, stage by stage, in
//! hardened containers, and publishes the result as a branch.
//!
//! Each stage boundary is written to the store before the next stage
//! starts, so a run can stop anywhere and resume from its last checkpoint
//! (docs/design.md §5). After every agent stage the worktree is committed
//! by host git and checked for protected paths; verify runs on a clean
//! export of that commit with `.factory/` from the base; the diff is
//! scanned for secrets before anything is pushed.
//!
//! The engine knows nothing about GitHub. Where an issue lives supplies
//! git credentials and opens the pull request through [`Forge`]; claiming
//! issues and keeping their labels current is `factory`'s job.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use dsf_core::clamp::Caps;
use dsf_core::protect::{self, ProtectedPaths};
use dsf_core::routing::{End, Next, Progress};
use dsf_core::schedule::usage_limit_resume;
use dsf_core::signature::FailureSignature;
use dsf_core::workflow::{
    self, AgentMode, Network as StageNetwork, Schema, StageId, StageKind, Terminal, Workflow,
};
use futures_util::future::BoxFuture;
use jiff::Timestamp;
use secrecy::SecretString;
use serde_json::Value;

use crate::agent::{self, AgentOutcome, Invocation};
use crate::hostgit::{Auth, Git, GitError, HostGit, Sha};
use crate::masker::Masker;
use crate::prompts::{self, Comment, Prompts};
use crate::rundir::RunDir;
use crate::sandbox::{
    self, ATTEMPT_LABEL, Bind, EnvValue, Limits, Network, RUN_LABEL, Sandbox, Spec,
};
use crate::store::{self, Advance, Attempt, AttemptEnd, NewRun, RunStatus, Store, SuspendReason};

/// `next_stage` value for a run that has finished its stages and only
/// needs publishing.
const PUBLISH: &str = "@publish";

/// Usage-limit hits in a row with no completed agent stage between them,
/// which lengthen the backoff when the reset time isn't known.
const LIMIT_HITS: &str = "usage_limit_hits";

/// How agent containers reach the internet.
#[derive(Debug, Clone)]
pub enum Egress {
    /// No network at all. For fake agents in tests.
    None,
    /// The agents' internal network, whose only way out is the proxy.
    Proxy { network: String, proxy_url: String },
}

pub struct Config {
    /// Names this dsf's containers and labels, so two instances (or
    /// parallel tests) on one Docker host never touch each other's.
    pub instance: String,
    pub state_dir: PathBuf,
    pub worker_image: String,
    pub model: String,
    pub max_turns: u32,
    pub agent_timeout: Duration,
    pub limits: Limits,
    pub egress: Egress,
    pub caps: Caps,
    pub extra_protected: Vec<String>,
    pub claude_token: SecretString,
}

/// An issue to work on.
#[derive(Debug, Clone)]
pub struct Issue {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub clone_url: String,
    pub publish_url: String,
    /// Trusted users' comments, oldest first, for the prompts.
    pub comments: Vec<Comment>,
    /// Whether gate stages wait for a human. A waiting gate suspends the
    /// run until it is resumed with approval.
    pub require_approval: bool,
}

/// What a run needs from where its issue lives, beyond plain git: GitHub
/// for real issues, nothing for local runs.
pub trait Forge: Send + Sync {
    /// Credentials to clone the issue's repository, if it needs any.
    fn clone_auth<'a>(&'a self, issue: &'a Issue) -> BoxFuture<'a, Result<Option<Auth>>>;

    /// Credentials to push the run's branch.
    fn push_auth<'a>(&'a self, issue: &'a Issue) -> BoxFuture<'a, Result<Option<Auth>>>;

    /// Opens a draft pull request for the pushed branch, or updates the one
    /// already open for it. `None` where there are no pull requests.
    fn open_pull_request<'a>(
        &'a self,
        draft: &'a PullRequestDraft<'a>,
    ) -> BoxFuture<'a, Result<Option<PullRequestRef>>>;
}

/// A pull request for a published branch.
#[derive(Debug, Clone, Copy)]
pub struct PullRequestDraft<'a> {
    pub issue: &'a Issue,
    pub branch: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRef {
    pub number: u64,
    pub url: String,
}

/// For local runs: no credentials and no pull requests.
pub struct NoForge;

impl Forge for NoForge {
    fn clone_auth<'a>(&'a self, _: &'a Issue) -> BoxFuture<'a, Result<Option<Auth>>> {
        Box::pin(async { Ok(None) })
    }

    fn push_auth<'a>(&'a self, _: &'a Issue) -> BoxFuture<'a, Result<Option<Auth>>> {
        Box::pin(async { Ok(None) })
    }

    fn open_pull_request<'a>(
        &'a self,
        _: &'a PullRequestDraft<'a>,
    ) -> BoxFuture<'a, Result<Option<PullRequestRef>>> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Published {
        run_id: i64,
        branch: String,
        sha: String,
        pull_request: Option<PullRequestRef>,
    },
    Blocked {
        run_id: i64,
        reason: String,
    },
    Suspended {
        run_id: i64,
        reason: SuspendReason,
        resume_after: Option<Timestamp>,
        detail: String,
    },
    PublishFailed {
        run_id: i64,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResumeOptions {
    /// Treat a waiting gate as approved.
    pub approve: bool,
}

pub struct Engine {
    config: Config,
    store: Mutex<Store>,
    sandbox: Sandbox,
    git: Git,
    forge: Box<dyn Forge>,
    masker: Masker,
    /// `uid:gid` containers run as: the owner of the state directory.
    user: String,
    report: Box<dyn Fn(&str) + Send + Sync>,
}

/// A run in progress.
struct Run {
    id: i64,
    issue: Issue,
    dir: RunDir,
    repo: HostGit,
    base: Sha,
    checkpoint: Sha,
    workflow: Workflow,
    prompts: Prompts,
    protected: ProtectedPaths,
    protected_list: Vec<String>,
    plan: Option<String>,
    verify_output: Option<String>,
    review: Option<String>,
    approve_gate: bool,
}

enum StageResult {
    Outcome {
        label: String,
        signature: Option<FailureSignature>,
        end: Ended,
    },
    Suspend {
        reason: SuspendReason,
        resume_after: Option<Timestamp>,
        detail: String,
    },
    Block(String),
}

/// An attempt's results, owned until they are written.
#[derive(Default)]
struct Ended {
    failure: Option<String>,
    exit_code: Option<i64>,
    usage: Option<Value>,
    structured: Option<Value>,
}

impl Ended {
    fn failed(reason: &str) -> Self {
        Self {
            failure: Some(reason.to_owned()),
            ..Self::default()
        }
    }
}

/// What the loop does after a stage boundary.
enum Step {
    Next(Next),
    Stop(RunEnd),
}

enum Setup {
    Ready(Box<Run>),
    Blocked(String),
}

impl Engine {
    pub fn new(
        config: Config,
        store: Store,
        sandbox: Sandbox,
        git: Git,
        report: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Self> {
        let user = sandbox::owner_of(&config.state_dir)?;
        let masker = Masker::new();
        masker.add(&config.claude_token);
        Ok(Self {
            config,
            store: Mutex::new(store),
            sandbox,
            git,
            forge: Box::new(NoForge),
            masker,
            user,
            report,
        })
    }

    #[must_use]
    pub fn with_forge(mut self, forge: Box<dyn Forge>) -> Self {
        self.forge = forge;
        self
    }

    /// Records a run for `issue` and runs it until it is published,
    /// blocked or suspended.
    pub async fn start(&self, issue: &Issue) -> Result<RunEnd> {
        let run_id = self.create_run(issue)?.ok_or_else(|| {
            anyhow!(
                "{}#{} already has an unfinished run",
                issue.repo,
                issue.number
            )
        })?;
        self.run_new(run_id, issue).await
    }

    /// Records a queued run for `issue`, or returns `None` if it already
    /// has an unfinished one. The database's guard against double claims
    /// comes first; the caller claims the issue, then calls
    /// [`Self::run_new`] or [`Self::cancel`].
    pub fn create_run(&self, issue: &Issue) -> Result<Option<i64>> {
        let comments = serde_json::to_string(&issue.comments)?;
        let new = NewRun {
            repo: &issue.repo,
            issue: issue.number,
            title: &issue.title,
            body: &issue.body,
            author: &issue.author,
            updated_at: None,
            workflow: workflow::DEFAULT_TOML,
            clone_url: &issue.clone_url,
            publish_url: &issue.publish_url,
            comments: &comments,
            require_approval: issue.require_approval,
        };
        self.db()?.create_run(&new, Timestamp::now())
    }

    /// Runs a run made by [`Self::create_run`] from the beginning.
    pub async fn run_new(&self, run_id: i64, issue: &Issue) -> Result<RunEnd> {
        self.event(
            run_id,
            "started",
            &format!("{}#{}: {}", issue.repo, issue.number, issue.title),
        );
        match self.setup(run_id, issue).await {
            Ok(Setup::Ready(run)) => {
                let mut progress = Progress::default();
                let next = progress.begin(&run.workflow);
                self.drive(*run, progress, next).await
            }
            Ok(Setup::Blocked(reason)) => self.block(run_id, &reason),
            Err(error) => self.block(run_id, &format!("setup failed: {error:#}")),
        }
    }

    /// Ends an unfinished run that will never continue, e.g. because its
    /// issue changed or someone else claimed it.
    pub fn cancel(&self, run_id: i64, reason: &str) -> Result<()> {
        let reason = self.masker.mask(reason);
        self.db()?
            .end_run(run_id, RunStatus::Canceled, &reason, Timestamp::now())?;
        self.event(run_id, "canceled", &reason);
        Ok(())
    }

    /// Puts things right after dsf stopped without finishing, e.g. a crash
    /// or a reboot. Every attempt left running is marked interrupted and its
    /// run suspended, to continue from its last checkpoint, and the run's
    /// containers are removed so an orphaned agent can't carry on unseen.
    /// A run that stopped before its first stage has nothing to continue
    /// and is canceled. Returns the interrupted runs.
    ///
    /// Only safe while no run is in progress, i.e. under the state lock.
    pub async fn recover(&self) -> Result<Vec<i64>> {
        let unfinished = self.db()?.unfinished_runs()?;
        for run in unfinished.iter().filter(|r| r.status == RunStatus::Queued) {
            self.cancel(run.id, "dsf stopped before the run's first stage")?;
        }
        self.db()?.interrupt_unfinished(None, Timestamp::now())?;
        let mut interrupted = Vec::new();
        for run in unfinished.iter().filter(|r| r.status == RunStatus::Running) {
            let removed = self
                .sandbox
                .remove_labelled(&format!("{RUN_LABEL}={}", self.run_label(run.id)))
                .await?;
            let stage = run.next_stage.as_deref().unwrap_or("setup");
            let mut message =
                format!("dsf stopped during {stage}; the run continues from its last checkpoint");
            if removed > 0 {
                let _ = write!(message, " (removed {removed} leftover container(s))");
            }
            self.event(run.id, "interrupted", &message);
            interrupted.push(run.id);
        }
        Ok(interrupted)
    }

    /// The issue's unfinished run, if any.
    pub fn active_run(&self, repo: &str, issue: u64) -> Result<Option<store::Run>> {
        self.db()?.active_run(repo, issue)
    }

    /// The run state, for the scheduler and `dsf status`.
    pub fn store(&self) -> Result<MutexGuard<'_, Store>> {
        self.db()
    }

    pub fn stored_run(&self, run_id: i64) -> Result<store::Run> {
        self.db()?.run(run_id)
    }

    pub fn set_approval_comment(&self, run_id: i64, comment_id: u64) -> Result<()> {
        self.db()?
            .set_approval_comment(run_id, comment_id, Timestamp::now())
    }

    /// Hides every secret dsf holds or has handed out.
    pub fn mask(&self, text: &str) -> String {
        self.masker.mask(text)
    }

    /// Adds a masked entry to a run's timeline.
    pub fn note(&self, run_id: i64, kind: &str, message: &str) {
        self.event(run_id, kind, message);
    }

    /// Registers credentials a forge handed out, so they're masked too.
    fn remember(&self, auth: Option<&Auth>) {
        for secret in auth.iter().flat_map(|auth| auth.secrets()) {
            self.masker.add(&secret);
        }
    }

    /// Resumes a suspended or interrupted run from its last checkpoint.
    pub async fn resume(&self, run_id: i64, options: ResumeOptions) -> Result<RunEnd> {
        // A crashed attempt's container may still be running. It must be
        // gone before the worktree is reset under it.
        let removed = self
            .sandbox
            .remove_labelled(&format!("{RUN_LABEL}={}", self.run_label(run_id)))
            .await?;
        if removed > 0 {
            self.event(
                run_id,
                "cleanup",
                &format!("removed {removed} leftover container(s)"),
            );
        }
        self.db()?
            .interrupt_unfinished(Some(run_id), Timestamp::now())?;
        let stored = self.db()?.run(run_id)?;
        if !matches!(stored.status, RunStatus::Suspended | RunStatus::Queued) {
            bail!(
                "run {run_id} is {} and can't be resumed",
                stored.status.as_str()
            );
        }
        let dir = RunDir::open(&self.config.state_dir, run_id)?;
        let repo = HostGit::open(self.git.clone(), &dir.git_dir(), &dir.work_tree());
        let base = Sha::parse(
            stored
                .base_sha
                .as_deref()
                .context("the run never cloned its repository")?,
        )?;
        let checkpoint = Sha::parse(stored.checkpoint_sha.as_deref().unwrap_or(base.as_str()))?;
        repo.reset_to(&checkpoint).await?;
        let (workflow, _) = Workflow::parse(&stored.workflow)?.clamp(&self.config.caps)?;
        let prompts = Prompts::from_snapshot(&dir.factory_snapshot())?;
        let (verify_output, review) = self.handoff(run_id, &dir, &workflow)?;
        let (protected, protected_list) = self.protected();
        let issue = Issue {
            repo: stored.repo.clone(),
            number: stored.issue,
            title: stored.issue_title.clone(),
            body: stored.issue_body.clone(),
            author: stored.issue_author.clone(),
            clone_url: stored.clone_url.clone(),
            publish_url: stored.publish_url.clone(),
            comments: serde_json::from_str(&stored.issue_comments)
                .context("reading the run's stored comments")?,
            require_approval: stored.require_approval,
        };
        let run = Run {
            id: run_id,
            issue,
            dir,
            repo,
            base,
            checkpoint,
            workflow,
            prompts,
            protected,
            protected_list,
            plan: stored.plan.clone(),
            verify_output,
            review,
            approve_gate: options.approve,
        };
        self.event(run_id, "resumed", &format!("from {}", run.checkpoint));
        let mut progress = stored.progress;
        let next = match stored.next_stage.as_deref() {
            Some(PUBLISH) => return self.publish(&run).await,
            Some(stage) => {
                let stage =
                    StageId::new(stage).with_context(|| format!("stored stage `{stage}`"))?;
                if stored.suspend_reason == Some(SuspendReason::Interrupted) {
                    let next = progress.interrupted(&stage);
                    // Saved now, so repeated crashes are counted.
                    self.db()?
                        .set_progress(run_id, &progress, Timestamp::now())?;
                    next
                } else {
                    // The visit was counted when the stage was first entered.
                    Next::Run(stage)
                }
            }
            None => progress.begin(&run.workflow),
        };
        self.drive(run, progress, next).await
    }

    async fn setup(&self, run_id: i64, issue: &Issue) -> Result<Setup> {
        let dir = RunDir::create(&self.config.state_dir, run_id)?;
        let auth = self.forge.clone_auth(issue).await?;
        self.remember(auth.as_ref());
        let (repo, base) = match HostGit::clone(
            self.git.clone(),
            &issue.clone_url,
            auth.as_ref(),
            &dir.git_dir(),
            &dir.work_tree(),
        )
        .await
        {
            Ok(cloned) => cloned,
            Err(GitError::Refused(refusals)) => {
                return Ok(Setup::Blocked(GitError::Refused(refusals).to_string()));
            }
            Err(GitError::Failed(error)) => return Err(error),
        };
        self.db()?
            .set_base(run_id, base.as_str(), Timestamp::now())?;
        let snapshot = dir.factory_snapshot();
        match repo.snapshot_factory(&base, &snapshot).await {
            Ok(()) => {}
            Err(GitError::Refused(refusals)) => {
                return Ok(Setup::Blocked(format!(
                    ".factory/ at the base commit: {}",
                    GitError::Refused(refusals)
                )));
            }
            Err(GitError::Failed(error)) => return Err(error),
        }

        let workflow = match self.load_workflow(run_id, &snapshot) {
            Ok(workflow) => workflow,
            Err(problem) => return Ok(Setup::Blocked(problem)),
        };
        for stage in workflow.stages().values() {
            if let StageKind::Command { run, .. } = &stage.kind {
                let script = snapshot.join(run);
                let executable = std::fs::symlink_metadata(&script).is_ok_and(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                });
                if !executable {
                    return Ok(Setup::Blocked(format!(
                        "{run} is missing or not executable at the base commit"
                    )));
                }
            }
        }
        let prompts = match Prompts::from_snapshot(&snapshot) {
            Ok(prompts) => prompts,
            Err(error) => return Ok(Setup::Blocked(format!("prompt templates: {error:#}"))),
        };
        let (protected, protected_list) = self.protected();
        Ok(Setup::Ready(Box::new(Run {
            id: run_id,
            issue: issue.clone(),
            dir,
            repo,
            checkpoint: base.clone(),
            base,
            workflow,
            prompts,
            protected,
            protected_list,
            plan: None,
            verify_output: None,
            review: None,
            approve_gate: false,
        })))
    }

    /// The repo's `.factory/workflow.toml` from the base snapshot, clamped
    /// to the operator's caps, or the built-in workflow.
    fn load_workflow(&self, run_id: i64, snapshot: &Path) -> std::result::Result<Workflow, String> {
        let path = snapshot.join(".factory/workflow.toml");
        let (workflow, source) = if path.is_file() {
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading .factory/workflow.toml: {e}"))?;
            let workflow =
                Workflow::parse(&source).map_err(|e| format!(".factory/workflow.toml: {e}"))?;
            (workflow, Some(source))
        } else {
            (Workflow::builtin().map_err(|e| e.to_string())?, None)
        };
        let (workflow, adjustments) = workflow
            .clamp(&self.config.caps)
            .map_err(|e| e.to_string())?;
        for adjustment in adjustments {
            self.event(run_id, "clamped", &adjustment.to_string());
        }
        if let Some(source) = source {
            self.db()
                .and_then(|mut db| db.set_workflow(run_id, &source, Timestamp::now()))
                .map_err(|e| e.to_string())?;
            self.event(
                run_id,
                "workflow",
                "using the repo's .factory/workflow.toml",
            );
        }
        Ok(workflow)
    }

    fn protected(&self) -> (ProtectedPaths, Vec<String>) {
        let list = protect::BUILTIN
            .iter()
            .map(|entry| (*entry).to_owned())
            .chain(self.config.extra_protected.iter().cloned())
            .collect();
        (ProtectedPaths::new(&self.config.extra_protected), list)
    }

    async fn drive(&self, mut run: Run, mut progress: Progress, mut next: Next) -> Result<RunEnd> {
        loop {
            let stage = match next {
                Next::Run(stage) => stage,
                Next::End(End {
                    terminal: Terminal::Publish,
                    ..
                }) => return self.publish(&run).await,
                Next::End(End {
                    terminal: Terminal::Blocked,
                    reason,
                }) => {
                    return self.block(run.id, &reason.to_string());
                }
            };
            let visit = progress.visits(&stage);
            let attempt = self.db()?.start_attempt(
                run.id,
                stage.as_str(),
                visit,
                Some(run.checkpoint.as_str()),
                Timestamp::now(),
            )?;
            self.event(run.id, "stage", &format!("{stage} (visit {visit})"));
            let result = match self.run_stage(&mut run, &stage, visit, attempt).await {
                Ok(result) => result,
                Err(error) => StageResult::Block(format!("{stage} could not run: {error:#}")),
            };
            next = match self.conclude(&run, attempt, &stage, result, &mut progress)? {
                Step::Next(next) => next,
                Step::Stop(end) => return Ok(end),
            };
        }
    }

    /// Writes the stage boundary for an attempt's result, and says where
    /// the run goes next or how it stopped.
    fn conclude(
        &self,
        run: &Run,
        attempt: i64,
        stage: &StageId,
        result: StageResult,
        progress: &mut Progress,
    ) -> Result<Step> {
        match result {
            StageResult::Outcome {
                label,
                signature,
                end,
            } => {
                let next = progress.complete(&run.workflow, stage, &label, signature);
                self.event(run.id, "outcome", &format!("{stage}: {label}"));
                match &next {
                    Next::End(End {
                        terminal: Terminal::Blocked,
                        reason,
                    }) => {
                        let reason = self.masker.mask(&reason.to_string());
                        let advance = Advance::End {
                            status: RunStatus::Blocked,
                            reason: &reason,
                        };
                        self.finish(run, attempt, Some(&label), &end, progress, advance)?;
                        self.event(run.id, "blocked", &reason);
                        Ok(Step::Stop(RunEnd::Blocked {
                            run_id: run.id,
                            reason,
                        }))
                    }
                    Next::End(End {
                        terminal: Terminal::Publish,
                        ..
                    }) => {
                        self.finish(
                            run,
                            attempt,
                            Some(&label),
                            &end,
                            progress,
                            Advance::Next { stage: PUBLISH },
                        )?;
                        Ok(Step::Next(next))
                    }
                    Next::Run(next_stage) => {
                        let advance = Advance::Next {
                            stage: next_stage.as_str(),
                        };
                        self.finish(run, attempt, Some(&label), &end, progress, advance)?;
                        Ok(Step::Next(next))
                    }
                }
            }
            StageResult::Suspend {
                reason,
                resume_after,
                detail,
            } => {
                let detail = self.masker.mask(&detail);
                let advance = Advance::Suspend {
                    reason,
                    resume_after,
                };
                self.finish(
                    run,
                    attempt,
                    None,
                    &Ended::failed(&detail),
                    progress,
                    advance,
                )?;
                self.event(run.id, "suspended", &detail);
                Ok(Step::Stop(RunEnd::Suspended {
                    run_id: run.id,
                    reason,
                    resume_after,
                    detail,
                }))
            }
            StageResult::Block(reason) => {
                let reason = self.masker.mask(&reason);
                let advance = Advance::End {
                    status: RunStatus::Blocked,
                    reason: &reason,
                };
                self.finish(
                    run,
                    attempt,
                    None,
                    &Ended::failed(&reason),
                    progress,
                    advance,
                )?;
                self.event(run.id, "blocked", &reason);
                Ok(Step::Stop(RunEnd::Blocked {
                    run_id: run.id,
                    reason,
                }))
            }
        }
    }

    fn finish(
        &self,
        run: &Run,
        attempt: i64,
        outcome: Option<&str>,
        end: &Ended,
        progress: &Progress,
        advance: Advance<'_>,
    ) -> Result<()> {
        let failure = end.failure.as_deref().map(|f| self.masker.mask(f));
        self.db()?.finish_attempt(
            run.id,
            attempt,
            &AttemptEnd {
                outcome,
                failure: failure.as_deref(),
                exit_code: end.exit_code,
                after_sha: Some(run.checkpoint.as_str()),
                usage: end.usage.as_ref(),
                structured_output: end.structured.as_ref(),
            },
            progress,
            Some(run.checkpoint.as_str()),
            advance,
            Timestamp::now(),
        )
    }

    async fn run_stage(
        &self,
        run: &mut Run,
        stage: &StageId,
        visit: u32,
        attempt: i64,
    ) -> Result<StageResult> {
        let kind = run
            .workflow
            .stage(stage)
            .with_context(|| format!("stage {stage} is not in the workflow"))?
            .kind
            .clone();
        let attempt_dir = run.dir.attempt(attempt)?;
        match kind {
            StageKind::Agent {
                mode,
                prompt,
                schema,
            } => {
                self.agent_stage(
                    run,
                    stage,
                    visit,
                    attempt,
                    &attempt_dir,
                    mode,
                    &prompt,
                    schema,
                )
                .await
            }
            StageKind::Command {
                run: script,
                timeout,
                network,
            } => {
                self.command_stage(run, attempt, &attempt_dir, &script, timeout, network)
                    .await
            }
            StageKind::Gate => Ok(if run.issue.require_approval && !run.approve_gate {
                StageResult::Suspend {
                    reason: SuspendReason::Approval,
                    resume_after: None,
                    detail: "waiting for plan approval".to_owned(),
                }
            } else {
                StageResult::Outcome {
                    label: "approved".to_owned(),
                    signature: None,
                    end: Ended::default(),
                }
            }),
        }
    }

    #[allow(clippy::too_many_arguments)] // One stage's worth of context.
    async fn agent_stage(
        &self,
        run: &mut Run,
        stage: &StageId,
        visit: u32,
        attempt: i64,
        attempt_dir: &Path,
        mode: AgentMode,
        prompt_name: &str,
        schema: Option<Schema>,
    ) -> Result<StageResult> {
        let prompt = self.render_prompt(run, visit, prompt_name, schema).await?;
        std::fs::write(attempt_dir.join("prompt.md"), &prompt)?;
        let argv = agent::command_line(&Invocation {
            mode,
            prompt: &prompt,
            model: &self.config.model,
            schema,
            max_turns: self.config.max_turns,
        })?;
        let spec = self.agent_spec(run, attempt, argv, mode);
        let stream_path = attempt_dir.join("stream.jsonl");
        let finished = self
            .sandbox
            .run(&spec, &stream_path, &attempt_dir.join("stderr.log"), |id| {
                self.db()?.set_container(attempt, id)
            })
            .await?;
        let outcome = if finished.timed_out {
            let limit = dsf_core::duration::format(self.config.agent_timeout);
            AgentOutcome::Failed(format!("timed out after {limit}"))
        } else {
            agent::summarise(&read_lossy(&stream_path)).outcome
        };

        match outcome {
            AgentOutcome::Completed(completion) => {
                self.db()?.kv_delete(LIMIT_HITS)?;
                if mode == AgentMode::Write
                    && let Some(reason) = self.record_work(run, stage, visit).await?
                {
                    return Ok(StageResult::Block(reason));
                }
                self.completed(run, schema, completion)
            }
            AgentOutcome::Limited { resets_at, detail } => {
                run.repo.reset_to(&run.checkpoint).await?;
                let hits = {
                    let mut db = self.db()?;
                    let hits = db
                        .kv_get(LIMIT_HITS)?
                        .and_then(|n| n.parse::<u32>().ok())
                        .unwrap_or(0)
                        .saturating_add(1);
                    db.kv_set(LIMIT_HITS, &hits.to_string())?;
                    hits
                };
                Ok(StageResult::Suspend {
                    reason: SuspendReason::UsageLimit,
                    resume_after: Some(usage_limit_resume(Timestamp::now(), resets_at, hits)),
                    detail,
                })
            }
            AgentOutcome::AuthFailed(error) => {
                run.repo.reset_to(&run.checkpoint).await?;
                Ok(StageResult::Suspend {
                    reason: SuspendReason::Interrupted,
                    resume_after: None,
                    detail: format!(
                        "Claude authentication failed ({error}); fix the token, then resume"
                    ),
                })
            }
            AgentOutcome::Failed(reason) => {
                if mode == AgentMode::Write
                    && let Some(block) = self.record_work(run, stage, visit).await?
                {
                    return Ok(StageResult::Block(block));
                }
                Ok(StageResult::Outcome {
                    label: "failure".to_owned(),
                    signature: None,
                    end: Ended::failed(&reason),
                })
            }
        }
    }

    /// The stage's prompt, with the context gathered so far: the plan, the
    /// change, and the last verify output and review.
    async fn render_prompt(
        &self,
        run: &Run,
        visit: u32,
        name: &str,
        schema: Option<Schema>,
    ) -> Result<String> {
        let changed = run.checkpoint != run.base;
        let diff_stat = if changed {
            Some(run.repo.diff_stat(&run.base, &run.checkpoint).await?)
        } else {
            None
        };
        let diff = if changed && schema == Some(Schema::Review) {
            let diff = run.repo.diff(&run.base, &run.checkpoint).await?;
            Some(prompts::head(&diff, prompts::MAX_DIFF_BYTES))
        } else {
            None
        };
        let (comments, omitted_comments) = prompts::fit_comments(&run.issue.comments);
        let context = prompts::Context {
            visit,
            issue: prompts::Issue {
                repo: &run.issue.repo,
                number: run.issue.number,
                title: &run.issue.title,
                body: prompts::head(&run.issue.body, prompts::MAX_BODY_BYTES),
                comments,
                omitted_comments,
            },
            plan: run.plan.as_deref(),
            protected: &run.protected_list,
            diff_stat: diff_stat.as_deref(),
            diff,
            verify_output: run.verify_output.clone(),
            review: run.review.as_deref(),
        };
        run.prompts.render(name, &context)
    }

    /// The outcome of an agent stage that finished normally. A schema
    /// stage's outcome comes from its structured output and must be one of
    /// the schema's labels; anything else is a failure.
    fn completed(
        &self,
        run: &mut Run,
        schema: Option<Schema>,
        completion: agent::Completion,
    ) -> Result<StageResult> {
        let label = match schema {
            Some(schema) => completion
                .structured
                .as_ref()
                .and_then(|output| output["outcome"].as_str())
                .filter(|outcome| schema.outcomes().contains(outcome))
                .unwrap_or("failure")
                .to_owned(),
            None => "success".to_owned(),
        };
        if let Some(output) = &completion.structured {
            match schema {
                Some(Schema::Plan) => {
                    let plan = plan_text(output);
                    self.db()?.set_plan(run.id, &plan, Timestamp::now())?;
                    run.plan = Some(plan);
                }
                Some(Schema::Review) => {
                    run.review = (label == "changes_requested").then(|| review_text(output));
                }
                None => {}
            }
        }
        Ok(StageResult::Outcome {
            label,
            signature: None,
            end: Ended {
                usage: Some(completion.usage),
                structured: completion.structured,
                ..Ended::default()
            },
        })
    }

    /// Commits the agent's work and checks it. `Some` is a reason to block.
    async fn record_work(
        &self,
        run: &mut Run,
        stage: &StageId,
        visit: u32,
    ) -> Result<Option<String>> {
        match run
            .repo
            .checkpoint(&format!("dsf: {stage} (visit {visit})"))
            .await
        {
            Ok(Some(sha)) => run.checkpoint = sha,
            Ok(None) => {}
            Err(GitError::Refused(refusals)) => {
                return Ok(Some(format!(
                    "{stage} produced {}",
                    GitError::Refused(refusals)
                )));
            }
            Err(GitError::Failed(error)) => return Err(error),
        }
        if run.checkpoint == run.base {
            return Ok(None);
        }
        let changed = run.repo.changed_paths(&run.base, &run.checkpoint).await?;
        let violations = run.protected.violations(changed.iter().map(String::as_str));
        if violations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!(
                "{stage} changed protected paths: {}",
                violations.join(", ")
            )))
        }
    }

    fn agent_spec(&self, run: &Run, attempt: i64, argv: Vec<String>, mode: AgentMode) -> Spec {
        let mut env = vec![
            ("HOME".to_owned(), EnvValue::Plain(sandbox::HOME.to_owned())),
            (
                "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                EnvValue::Secret(self.config.claude_token.clone()),
            ),
            (
                "DISABLE_AUTOUPDATER".to_owned(),
                EnvValue::Plain("1".to_owned()),
            ),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
                EnvValue::Plain("1".to_owned()),
            ),
        ];
        let network = self.network_for(&mut env, true);
        Spec {
            name: format!("{}-{}-{attempt}", self.config.instance, run.id),
            image: self.config.worker_image.clone(),
            cmd: argv,
            env,
            binds: vec![
                Bind {
                    source: run.dir.work_tree(),
                    target: "/work".to_owned(),
                    read_only: mode == AgentMode::Read,
                },
                Bind {
                    source: run.dir.git_dir(),
                    target: "/work/.git".to_owned(),
                    read_only: true,
                },
            ],
            network,
            labels: self.labels(run.id, attempt),
            user: self.user.clone(),
            workdir: "/work".to_owned(),
            limits: self.config.limits,
            timeout: self.config.agent_timeout,
        }
    }

    /// The network for a container, adding proxy settings to `env` when it
    /// goes through the proxy.
    fn network_for(&self, env: &mut Vec<(String, EnvValue)>, wants_egress: bool) -> Network {
        match (&self.config.egress, wants_egress) {
            (Egress::Proxy { network, proxy_url }, true) => {
                for key in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
                    env.push((key.to_owned(), EnvValue::Plain(proxy_url.clone())));
                }
                Network::Named(network.clone())
            }
            _ => Network::None,
        }
    }

    async fn command_stage(
        &self,
        run: &mut Run,
        attempt: i64,
        attempt_dir: &Path,
        script: &str,
        timeout: Duration,
        network: StageNetwork,
    ) -> Result<StageResult> {
        let tree = run.dir.verify_tree(attempt)?;
        run.repo
            .export(&run.checkpoint, &tree, &run.dir.scratch())
            .await?;
        overlay_factory(&run.dir.factory_snapshot(), &tree)?;
        let mut env = vec![("HOME".to_owned(), EnvValue::Plain(sandbox::HOME.to_owned()))];
        let network = self.network_for(&mut env, network == StageNetwork::Proxy);
        let spec = Spec {
            name: format!("{}-{}-{attempt}", self.config.instance, run.id),
            image: self.config.worker_image.clone(),
            cmd: vec![format!("/work/{script}")],
            env,
            binds: vec![Bind {
                source: tree.clone(),
                target: "/work".to_owned(),
                read_only: false,
            }],
            network,
            labels: self.labels(run.id, attempt),
            user: self.user.clone(),
            workdir: "/work".to_owned(),
            limits: self.config.limits,
            timeout,
        };
        let (stdout, stderr) = (
            attempt_dir.join("stdout.log"),
            attempt_dir.join("stderr.log"),
        );
        let finished = self
            .sandbox
            .run(&spec, &stdout, &stderr, |id| {
                self.db()?.set_container(attempt, id)
            })
            .await?;
        let _ = std::fs::remove_dir_all(&tree);
        let mut output = read_lossy(&stdout);
        output.push_str(&read_lossy(&stderr));
        if !finished.timed_out && finished.exit_code == Some(0) {
            run.verify_output = None;
            return Ok(StageResult::Outcome {
                label: "success".to_owned(),
                signature: None,
                end: Ended {
                    exit_code: Some(0),
                    ..Ended::default()
                },
            });
        }
        if finished.timed_out {
            let _ = writeln!(
                output,
                "
[{script} timed out after {}]",
                dsf_core::duration::format(timeout)
            );
        }
        let signature = FailureSignature::of(&output);
        run.verify_output = Some(prompts::tail(&output, prompts::MAX_OUTPUT_BYTES));
        Ok(StageResult::Outcome {
            label: "failure".to_owned(),
            signature: Some(signature),
            end: Ended {
                failure: Some(format!("{script}: {signature}")),
                exit_code: finished.exit_code,
                ..Ended::default()
            },
        })
    }

    async fn publish(&self, run: &Run) -> Result<RunEnd> {
        if run.checkpoint == run.base {
            return self.block(run.id, "the run made no changes to publish");
        }
        let diff = run.repo.diff(&run.base, &run.checkpoint).await?;
        let findings = self.masker.find(&diff);
        if !findings.is_empty() {
            let lines: Vec<String> = findings.iter().map(|f| f.line.to_string()).collect();
            return self.block(
                run.id,
                &format!("the change contains what looks like a secret (diff lines {}), so it was not pushed", lines.join(", ")),
            );
        }
        let branch = format!("factory/issue-{}", run.issue.number);
        let pushed = async {
            let auth = self.forge.push_auth(&run.issue).await?;
            self.remember(auth.as_ref());
            run.repo
                .push(
                    &run.issue.publish_url,
                    &run.checkpoint,
                    &branch,
                    auth.as_ref(),
                )
                .await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = pushed {
            return self.publish_failed(run.id, &format!("push failed: {error:#}"));
        }
        let body = self.pull_request_body(run).await?;
        let draft = PullRequestDraft {
            issue: &run.issue,
            branch: &branch,
            title: &run.issue.title,
            body: &body,
        };
        let pull_request = match self.forge.open_pull_request(&draft).await {
            Ok(pull_request) => pull_request,
            Err(error) => {
                return self.publish_failed(
                    run.id,
                    &format!("pushed {branch}, but opening its pull request failed: {error:#}"),
                );
            }
        };
        let mut reason = format!("pushed {branch} at {}", run.checkpoint);
        if let Some(pull_request) = &pull_request {
            self.db()?
                .set_pr(run.id, pull_request.number, Timestamp::now())?;
            let _ = write!(reason, " and opened {}", pull_request.url);
        }
        self.db()?
            .end_run(run.id, RunStatus::Published, &reason, Timestamp::now())?;
        self.event(run.id, "published", &reason);
        Ok(RunEnd::Published {
            run_id: run.id,
            branch,
            sha: run.checkpoint.to_string(),
            pull_request,
        })
    }

    fn publish_failed(&self, run_id: i64, reason: &str) -> Result<RunEnd> {
        let reason = self.masker.mask(reason);
        self.db()?
            .end_run(run_id, RunStatus::PublishFailed, &reason, Timestamp::now())?;
        self.event(run_id, "publish_failed", &reason);
        Ok(RunEnd::PublishFailed { run_id, reason })
    }

    /// The pull request's description, masked.
    async fn pull_request_body(&self, run: &Run) -> Result<String> {
        let diff_stat = run.repo.diff_stat(&run.base, &run.checkpoint).await?;
        let attempts = self.db()?.attempts(run.id)?;
        let body = pull_request_body(&PullRequestFacts {
            run_id: run.id,
            issue: run.issue.number,
            plan: run.plan.as_deref(),
            workflow: &run.workflow,
            attempts: &attempts,
            diff_stat: &diff_stat,
        });
        Ok(self.masker.mask(&body))
    }

    /// Ends a run as blocked, e.g. when its plan was never approved.
    pub fn block(&self, run_id: i64, reason: &str) -> Result<RunEnd> {
        let reason = self.masker.mask(reason);
        self.db()?
            .end_run(run_id, RunStatus::Blocked, &reason, Timestamp::now())?;
        self.event(run_id, "blocked", &reason);
        Ok(RunEnd::Blocked { run_id, reason })
    }

    /// The verify output and review feedback a resumed fix stage needs,
    /// from the run's most recent attempts.
    fn handoff(
        &self,
        run_id: i64,
        dir: &RunDir,
        workflow: &Workflow,
    ) -> Result<(Option<String>, Option<String>)> {
        let attempts = self.db()?.attempts(run_id)?;
        let kind_of = |stage: &str| {
            StageId::new(stage)
                .and_then(|id| workflow.stage(&id))
                .map(|s| s.kind.clone())
        };
        let verify_output = attempts
            .iter()
            .rev()
            .find(|a| matches!(kind_of(&a.stage), Some(StageKind::Command { .. })))
            .filter(|a| a.outcome.as_deref() == Some("failure"))
            .map(|a| {
                let root = dir.root().join("attempts").join(a.id.to_string());
                let mut output = read_lossy(&root.join("stdout.log"));
                output.push_str(&read_lossy(&root.join("stderr.log")));
                prompts::tail(&output, prompts::MAX_OUTPUT_BYTES)
            });
        let review = attempts
            .iter()
            .rev()
            .find(|a| {
                matches!(
                    kind_of(&a.stage),
                    Some(StageKind::Agent {
                        schema: Some(Schema::Review),
                        ..
                    })
                )
            })
            .filter(|a| a.outcome.as_deref() == Some("changes_requested"))
            .and_then(|a| a.structured_output.as_deref())
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .map(|output| review_text(&output));
        Ok((verify_output, review))
    }

    fn run_label(&self, run_id: i64) -> String {
        format!("{}-{run_id}", self.config.instance)
    }

    fn labels(&self, run_id: i64, attempt: i64) -> Vec<(String, String)> {
        vec![
            (RUN_LABEL.to_owned(), self.run_label(run_id)),
            (ATTEMPT_LABEL.to_owned(), attempt.to_string()),
        ]
    }

    fn db(&self) -> Result<MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| anyhow!("the store lock was poisoned"))
    }

    /// Records a masked event and reports it.
    fn event(&self, run_id: i64, kind: &str, message: &str) {
        let message = self.masker.mask(message);
        if let Ok(mut db) = self.db() {
            let _ = db.event(Some(run_id), kind, &message, Timestamp::now());
        }
        (self.report)(&format!("run {run_id}: {message}"));
    }
}

fn read_lossy(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
}

/// The plan as later prompts and the PR body show it.
fn plan_text(output: &Value) -> String {
    let mut text = output["summary"].as_str().unwrap_or_default().to_owned();
    let list = |key: &str| output[key].as_array().cloned().unwrap_or_default();
    for (index, step) in list("steps").iter().enumerate() {
        let _ = write!(
            text,
            "
{}. {}",
            index + 1,
            step.as_str().unwrap_or_default()
        );
    }
    let questions = list("questions");
    if !questions.is_empty() {
        text.push_str("\n\nQuestions for the maintainer:");
        for question in questions {
            let _ = write!(text, "\n- {}", question.as_str().unwrap_or_default());
        }
    }
    text
}

fn review_text(output: &Value) -> String {
    let mut text = output["summary"].as_str().unwrap_or_default().to_owned();
    for issue in output["issues"].as_array().cloned().unwrap_or_default() {
        let _ = write!(text, "\n- {}", issue.as_str().unwrap_or_default());
    }
    text
}

/// What a pull request's description is written from.
struct PullRequestFacts<'a> {
    run_id: i64,
    issue: u64,
    plan: Option<&'a str>,
    workflow: &'a Workflow,
    attempts: &'a [Attempt],
    diff_stat: &'a str,
}

/// Plan, checks and changes, so the reviewer sees at a glance what was
/// done and how far the factory's own checks got.
fn pull_request_body(facts: &PullRequestFacts<'_>) -> String {
    const MAX_SECTION: usize = 20 * 1024;
    let kind = |stage: &str| {
        StageId::new(stage)
            .and_then(|id| facts.workflow.stage(&id))
            .map(|stage| stage.kind.clone())
    };
    let finished = |attempt: &&Attempt| attempt.status == "finished";
    let mut body = format!("Closes #{}.\n", facts.issue);
    if let Some(plan) = facts.plan {
        let _ = write!(body, "\n## Plan\n\n{}\n", prompts::head(plan, MAX_SECTION));
    }
    body.push_str("\n## Checks\n\n");
    let commands: Vec<(&Attempt, String)> = facts
        .attempts
        .iter()
        .filter(finished)
        .filter_map(|attempt| match kind(&attempt.stage) {
            Some(StageKind::Command { run, .. }) => Some((attempt, run)),
            _ => None,
        })
        .collect();
    match commands.last() {
        Some((attempt, script)) if attempt.outcome.as_deref() == Some("success") => {
            match commands.len() {
                1 => {
                    let _ = writeln!(body, "- `{script}` passed.");
                }
                runs => {
                    let _ = writeln!(body, "- `{script}` passed on the last of {runs} runs.");
                }
            }
        }
        Some((_, script)) => {
            let _ = writeln!(body, "- Warning: `{script}` did not pass on its last run.");
        }
        None => body.push_str("- No check command ran.\n"),
    }
    let review = facts
        .attempts
        .iter()
        .filter(finished)
        .rev()
        .find(|attempt| {
            matches!(
                kind(&attempt.stage),
                Some(StageKind::Agent {
                    schema: Some(Schema::Review),
                    ..
                })
            )
        });
    let verdict = review
        .and_then(|attempt| attempt.structured_output.as_deref())
        .and_then(|json| serde_json::from_str::<Value>(json).ok());
    match (review.and_then(|a| a.outcome.as_deref()), &verdict) {
        (Some("approve"), Some(output)) => {
            let _ = writeln!(
                body,
                "- The automated review approved it: {}",
                prompts::head(&review_text(output), MAX_SECTION)
            );
        }
        (Some("changes_requested"), Some(output)) => {
            let _ = writeln!(
                body,
                "- Warning: the automated review still asked for changes after its last round:\n\n{}",
                prompts::head(&review_text(output), MAX_SECTION)
            );
        }
        _ => body.push_str("- No automated review verdict.\n"),
    }
    let _ = write!(
        body,
        "\n## Changes\n\n```text\n{}\n```\n\n---\n\
         Opened as a draft by dsf, run {run}. An AI agent wrote this change in a sandbox; \
         review it like any other contribution. Nothing here is merged automatically.\n\
         <!-- dsf:run={run} -->\n",
        prompts::head(facts.diff_stat.trim_end(), MAX_SECTION),
        run = facts.run_id,
    );
    body
}

/// Replaces `.factory/` in the verify tree with the base snapshot, so the
/// gate is always the one the run started with.
fn overlay_factory(snapshot: &Path, tree: &Path) -> Result<()> {
    let target = tree.join(".factory");
    if std::fs::symlink_metadata(&target).is_ok() {
        // remove_dir_all removes a symlink itself, never what it points to.
        std::fs::remove_dir_all(&target).or_else(|_| std::fs::remove_file(&target))?;
    }
    copy_tree(&snapshot.join(".factory"), &target)
}

/// Copies a tree dsf wrote itself, which therefore holds no symlinks.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        } else {
            bail!(
                "unexpected entry in the .factory snapshot: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_text_lists_steps_and_questions() {
        let output = serde_json::json!({
            "outcome": "needs_decision",
            "summary": "Two ways to do it.",
            "steps": ["Change calc.py", "Add a test"],
            "questions": ["Integers only?"]
        });
        assert_eq!(
            plan_text(&output),
            "Two ways to do it.\n1. Change calc.py\n2. Add a test\n\nQuestions for the maintainer:\n- Integers only?"
        );
    }

    #[test]
    fn review_text_lists_issues() {
        let output =
            serde_json::json!({ "summary": "Close.", "issues": ["Add a test", "Rename x"] });
        assert_eq!(review_text(&output), "Close.\n- Add a test\n- Rename x");
    }

    fn attempt(id: i64, stage: &str, outcome: &str, structured: Option<Value>) -> Attempt {
        Attempt {
            id,
            stage: stage.to_owned(),
            visit: 1,
            status: "finished".to_owned(),
            outcome: Some(outcome.to_owned()),
            failure: None,
            container_id: None,
            exit_code: None,
            after_sha: None,
            structured_output: structured.map(|output| output.to_string()),
        }
    }

    #[test]
    fn pull_request_body_shows_plan_checks_and_changes() {
        let workflow = Workflow::builtin().unwrap();
        let attempts = [
            attempt(1, "plan", "ready", None),
            attempt(2, "implement", "success", None),
            attempt(3, "verify", "failure", None),
            attempt(4, "fix", "success", None),
            attempt(5, "verify", "success", None),
            attempt(
                6,
                "review",
                "approve",
                Some(
                    serde_json::json!({ "outcome": "approve", "summary": "Correct and minimal." }),
                ),
            ),
        ];
        let body = pull_request_body(&PullRequestFacts {
            run_id: 12,
            issue: 7,
            plan: Some("Make add() add.\n1. Change calc.py"),
            workflow: &workflow,
            attempts: &attempts,
            diff_stat: " calc.py | 2 +-\n 1 file changed\n",
        });
        assert_eq!(
            body,
            "Closes #7.\n\n## Plan\n\nMake add() add.\n1. Change calc.py\n\n## Checks\n\n\
             - `.factory/verify.sh` passed on the last of 2 runs.\n\
             - The automated review approved it: Correct and minimal.\n\n\
             ## Changes\n\n```text\n calc.py | 2 +-\n 1 file changed\n```\n\n---\n\
             Opened as a draft by dsf, run 12. An AI agent wrote this change in a sandbox; \
             review it like any other contribution. Nothing here is merged automatically.\n\
             <!-- dsf:run=12 -->\n"
        );
    }

    #[test]
    fn pull_request_body_warns_when_review_still_wants_changes() {
        let workflow = Workflow::builtin().unwrap();
        let attempts = [
            attempt(1, "verify", "success", None),
            attempt(
                2,
                "review",
                "changes_requested",
                Some(serde_json::json!({ "summary": "Close.", "issues": ["Add a test"] })),
            ),
        ];
        let body = pull_request_body(&PullRequestFacts {
            run_id: 1,
            issue: 2,
            plan: None,
            workflow: &workflow,
            attempts: &attempts,
            diff_stat: "x",
        });
        assert!(!body.contains("## Plan"));
        assert!(body.contains(
            "- Warning: the automated review still asked for changes after its last round:\n\nClose.\n- Add a test"
        ));
    }

    #[test]
    fn overlay_replaces_the_agents_factory_with_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        std::fs::create_dir_all(snapshot.join(".factory/lib")).unwrap();
        std::fs::write(snapshot.join(".factory/verify.sh"), "real gate").unwrap();
        std::fs::write(snapshot.join(".factory/lib/x.sh"), "helper").unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join(".factory")).unwrap();
        std::fs::write(tree.join(".factory/verify.sh"), "weakened gate").unwrap();
        std::fs::write(tree.join(".factory/extra.sh"), "planted").unwrap();
        overlay_factory(&snapshot, &tree).unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.join(".factory/verify.sh")).unwrap(),
            "real gate"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join(".factory/lib/x.sh")).unwrap(),
            "helper"
        );
        assert!(!tree.join(".factory/extra.sh").exists());
    }

    #[test]
    fn overlay_replaces_a_symlinked_factory_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        std::fs::create_dir_all(snapshot.join(".factory")).unwrap();
        std::fs::write(snapshot.join(".factory/verify.sh"), "real gate").unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), "must survive").unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::os::unix::fs::symlink(&elsewhere, tree.join(".factory")).unwrap();
        overlay_factory(&snapshot, &tree).unwrap();
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
            "must survive"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join(".factory/verify.sh")).unwrap(),
            "real gate"
        );
    }
}
