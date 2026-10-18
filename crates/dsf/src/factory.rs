//! The GitHub side of a run: whether an issue may be worked on, claiming
//! it without racing anyone, and keeping its labels and comments in step
//! with the run. `dsf run owner/repo#N` uses it for one issue; the
//! scheduler uses it for every candidate in a tick.
//!
//! Nothing here trusts a listing: every decision starts from a fresh read
//! of the issue and its label history, and the claim itself is removing
//! `factory:ready`, which only one claimant can do.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use dsf_core::config::{Config, RepoSlug};
use dsf_core::issue::{self, Candidate, SkipReason, Verdict};
use dsf_core::labels::{self, Lifecycle};
use futures_util::future::BoxFuture;
use jiff::{SignedDuration, Timestamp};

use crate::engine::{self, Engine, Forge, PullRequestDraft, PullRequestRef, ResumeOptions, RunEnd};
use crate::github::{GitHub, IssueEvent, NewPullRequest, Scope};
use crate::hostgit::Auth;
use crate::prompts::Comment;
use crate::store::{self, RunStatus, SuspendReason};

/// Where repositories are cloned from and pushed to.
pub const GIT_BASE: &str = "https://github.com/";

/// More `blocked by` references than this and the issue just waits; each
/// one costs an API call.
const MAX_BLOCKERS: usize = 20;

/// The GitHub App as the engine's [`Forge`]: narrow, short-lived tokens
/// for cloning and pushing, and draft pull requests.
pub struct GitHubForge {
    github: Arc<GitHub>,
}

impl GitHubForge {
    pub fn new(github: Arc<GitHub>) -> Self {
        Self { github }
    }

    async fn auth(&self, issue: &engine::Issue, scope: Scope) -> Result<Option<Auth>> {
        let repo: RepoSlug = issue.repo.parse().map_err(anyhow::Error::msg)?;
        Ok(Some(Auth::new(self.github.token(&repo, scope).await?)))
    }
}

impl Forge for GitHubForge {
    fn clone_auth<'a>(&'a self, issue: &'a engine::Issue) -> BoxFuture<'a, Result<Option<Auth>>> {
        Box::pin(self.auth(issue, Scope::Read))
    }

    fn push_auth<'a>(&'a self, issue: &'a engine::Issue) -> BoxFuture<'a, Result<Option<Auth>>> {
        Box::pin(self.auth(issue, Scope::Publish))
    }

    fn open_pull_request<'a>(
        &'a self,
        draft: &'a PullRequestDraft<'a>,
    ) -> BoxFuture<'a, Result<Option<PullRequestRef>>> {
        Box::pin(async move {
            let repo: RepoSlug = draft.issue.repo.parse().map_err(anyhow::Error::msg)?;
            let request = NewPullRequest {
                title: draft.title,
                head: draft.branch,
                body: draft.body,
            };
            let opened = self.github.open_pull_request(&repo, &request).await?;
            Ok(Some(PullRequestRef {
                number: opened.number,
                url: opened.html_url,
            }))
        })
    }
}

/// What came of looking at an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not eligible now. Nothing on GitHub was changed.
    Skipped(String),
    /// The run's plan is posted and waiting for `factory:plan-approved`.
    AwaitingApproval {
        run_id: i64,
    },
    Ended(RunEnd),
}

/// The time, from the system clock except in tests.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub struct Factory {
    engine: Engine,
    github: Arc<GitHub>,
    config: Config,
    git_base: String,
    clock: Clock,
}

impl Factory {
    /// `git_base` is [`GIT_BASE`] except in tests.
    pub fn new(engine: Engine, github: Arc<GitHub>, config: Config, git_base: &str) -> Self {
        Self {
            engine,
            github,
            config,
            git_base: git_base.to_owned(),
            clock: Arc::new(Timestamp::now),
        }
    }

    /// Decides with `clock`'s time rather than the system's, so tests can
    /// say when "now" is.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn now(&self) -> Timestamp {
        (self.clock)()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn github(&self) -> &GitHub {
        &self.github
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Looks at one issue and, if it may be worked on, works on it: a new
    /// run, a paused run continued, or a plan carried on after approval.
    pub async fn work_on(&self, repo: &RepoSlug, number: u64) -> Result<Outcome> {
        let settings = self
            .config
            .repo(repo)
            .with_context(|| format!("{repo} is not one of the configured repos"))?;
        let repo = &settings.slug;
        let issue = self.github.issue(repo, number).await?;
        if issue.is_pull_request() {
            return Ok(Outcome::Skipped(format!(
                "{repo}#{number} is a pull request"
            )));
        }
        if issue.state != "open" {
            return Ok(Outcome::Skipped(format!("{repo}#{number} is closed")));
        }
        let labels = issue.label_names();
        let events = self.github.issue_events(repo, number).await?;
        let label_events: Vec<_> = events
            .iter()
            .filter_map(IssueEvent::as_label_event)
            .collect();
        let active = self.engine.active_run(&repo.to_string(), number)?;

        if let Some(run) = &active
            && run.suspend_reason == Some(SuspendReason::Approval)
            && Lifecycle::of(&labels) == Some(Lifecycle::AwaitingApproval)
        {
            return self
                .after_approval(repo, run, &labels, &label_events, true)
                .await;
        }

        let own = self.github.login().await?;
        let body = issue.body.clone().unwrap_or_default();
        let open_blockers = self.open_blockers(repo, &body).await;
        let candidate = Candidate {
            author: &issue.user.login,
            ready_labeller: issue::ready_labeller(&label_events, own),
            labels: &labels,
            open_blockers: &open_blockers,
        };
        if let Verdict::Skip(reason) = issue::assess(&candidate, &self.config.allowlist) {
            return Ok(Outcome::Skipped(format!(
                "{repo}#{number}: {}",
                describe(&reason)
            )));
        }
        if let Some(run) = &active
            && let Some(after) = run.resume_after
            && after > self.now()
        {
            return Ok(Outcome::Skipped(format!(
                "{repo}#{number}: run {} is paused until {after}",
                run.id
            )));
        }

        let url = format!("{}{}/{}.git", self.git_base, repo.owner, repo.name);
        let work = engine::Issue {
            repo: repo.to_string(),
            number,
            title: issue.title.clone(),
            body,
            author: issue.user.login.clone(),
            clone_url: url.clone(),
            publish_url: url,
            comments: self.trusted_comments(repo, number).await?,
            require_approval: issue::gate_required(settings.approval, &labels),
        };
        match active {
            Some(run) if run.issue_title == work.title && run.issue_body == work.body => {
                // Paused by a usage limit or a crash, and the issue is as it
                // was: carry on from the last checkpoint.
                if !self.claim(repo, number, &labels).await? {
                    return Ok(Outcome::Skipped(lost_race(repo, number)));
                }
                let end = self.engine.resume(run.id, ResumeOptions::default()).await?;
                self.report(repo, number, &end).await;
                Ok(Outcome::Ended(end))
            }
            Some(run) => {
                self.engine.cancel(
                    run.id,
                    "the issue changed while the run was paused, so it starts again",
                )?;
                self.start(repo, number, &labels, &work).await
            }
            None => self.start(repo, number, &labels, &work).await,
        }
    }

    async fn start(
        &self,
        repo: &RepoSlug,
        number: u64,
        labels: &[String],
        work: &engine::Issue,
    ) -> Result<Outcome> {
        let Some(run_id) = self.engine.create_run(work)? else {
            return Ok(Outcome::Skipped(format!(
                "{repo}#{number} already has an unfinished run"
            )));
        };
        match self.claim(repo, number, labels).await {
            Ok(true) => {}
            Ok(false) => {
                self.engine.cancel(
                    run_id,
                    "factory:ready was removed before dsf could claim the issue",
                )?;
                return Ok(Outcome::Skipped(lost_race(repo, number)));
            }
            Err(error) => {
                self.engine
                    .cancel(run_id, &format!("claiming the issue failed: {error:#}"))?;
                return Err(error);
            }
        }
        let end = self.engine.run_new(run_id, work).await?;
        self.report(repo, number, &end).await;
        Ok(Outcome::Ended(end))
    }

    /// Takes the issue. Removing `factory:ready` is the claim: if it's
    /// already gone, someone else changed the issue first, and nothing
    /// else is touched.
    async fn claim(&self, repo: &RepoSlug, number: u64, labels: &[String]) -> Result<bool> {
        if !self
            .github
            .remove_label(repo, number, labels::READY.name)
            .await?
        {
            return Ok(false);
        }
        let change = labels::claim(labels);
        for name in change
            .remove
            .iter()
            .filter(|name| **name != labels::READY.name)
        {
            self.github.remove_label(repo, number, name).await?;
        }
        self.github.add_labels(repo, number, &change.add).await?;
        Ok(true)
    }

    /// Looks in on a paused run whose issue isn't queued: one stopped by a
    /// crash or restart while its issue still reads `factory:running`, or
    /// a plan waiting for approval. With `may_resume` it carries on where
    /// it can; without, it only cancels runs whose issue was closed and
    /// expires plans no one approved in time. A run whose issue a human has
    /// moved elsewhere is left for them.
    pub async fn continue_run(&self, run: &store::Run, may_resume: bool) -> Result<Outcome> {
        let repo = match run.repo.parse::<RepoSlug>() {
            Ok(slug) => match self.config.repo(&slug) {
                Some(settings) => settings.slug.clone(),
                None => {
                    return Ok(Outcome::Skipped(format!(
                        "run {}: {slug} is no longer one of the configured repos",
                        run.id
                    )));
                }
            },
            Err(_) => {
                return Ok(Outcome::Skipped(format!(
                    "run {} is not a GitHub run",
                    run.id
                )));
            }
        };
        let issue = self.github.issue(&repo, run.issue).await?;
        if issue.state != "open" {
            self.engine.cancel(run.id, "the issue was closed")?;
            return Ok(Outcome::Skipped(format!(
                "{repo}#{}: closed, so run {} was canceled",
                run.issue, run.id
            )));
        }
        let labels = issue.label_names();
        match Lifecycle::of(&labels) {
            Some(Lifecycle::AwaitingApproval)
                if run.suspend_reason == Some(SuspendReason::Approval) =>
            {
                let events = self.github.issue_events(&repo, run.issue).await?;
                let label_events: Vec<_> = events
                    .iter()
                    .filter_map(IssueEvent::as_label_event)
                    .collect();
                self.after_approval(&repo, run, &labels, &label_events, may_resume)
                    .await
            }
            Some(Lifecycle::Running) if may_resume => {
                if let Some(after) = run.resume_after
                    && after > self.now()
                {
                    return Ok(Outcome::Skipped(format!(
                        "{repo}#{}: run {} is paused until {after}",
                        run.issue, run.id
                    )));
                }
                let end = self.engine.resume(run.id, ResumeOptions::default()).await?;
                self.report(&repo, run.issue, &end).await;
                Ok(Outcome::Ended(end))
            }
            Some(Lifecycle::Ready) => Ok(Outcome::Skipped(format!(
                "{repo}#{}: queued; run {} continues when it's claimed",
                run.issue, run.id
            ))),
            _ => Ok(Outcome::Skipped(format!(
                "{repo}#{}: run {} waits for the issue's labels to change",
                run.issue, run.id
            ))),
        }
    }

    /// An issue labelled `factory:running` that no unfinished run is
    /// working on: dsf stopped before it could update the issue, or a
    /// person added the label. Moves it to where its last run ended, or to
    /// `factory:blocked` if there's nothing to go on.
    pub async fn repair(&self, repo: &RepoSlug, number: u64) -> Result<String> {
        let latest = self.engine.store()?.latest_run(&repo.to_string(), number)?;
        let end = latest.and_then(|run| match run.status {
            RunStatus::Published => Some(RunEnd::Published {
                run_id: run.id,
                branch: format!("factory/issue-{number}"),
                sha: run.checkpoint_sha.unwrap_or_default(),
                pull_request: run.pr_number.map(|pr| PullRequestRef {
                    number: pr,
                    url: format!("{GIT_BASE}{repo}/pull/{pr}"),
                }),
            }),
            RunStatus::Blocked | RunStatus::PublishFailed => Some(RunEnd::Blocked {
                run_id: run.id,
                reason: run.end_reason.unwrap_or_default(),
            }),
            _ => None,
        });
        if let Some(end) = end {
            self.try_report(repo, number, &end).await?;
            return Ok(format!(
                "{repo}#{number}: labels brought up to date with how its last run ended"
            ));
        }
        self.move_issue(repo, number, Lifecycle::Blocked).await?;
        self.github
            .post_comment(
                repo,
                number,
                "dsf has no unfinished run for this issue, so it can't be running. \
                 Add `factory:ready` to start again.",
            )
            .await?;
        Ok(format!(
            "{repo}#{number}: labelled factory:running with no run, so moved to factory:blocked"
        ))
    }

    /// Continues a run whose plan is waiting, if a trusted user approved it
    /// after it was posted and the approval is still on the issue, and
    /// `may_resume` allows. A plan left unapproved for longer than
    /// `approval_expiry` blocks the run: silence never approves.
    async fn after_approval(
        &self,
        repo: &RepoSlug,
        run: &store::Run,
        labels: &[String],
        events: &[issue::LabelEvent<'_>],
        may_resume: bool,
    ) -> Result<Outcome> {
        let Some(comment_id) = run.approval_comment_id else {
            // Posting the plan failed when the run paused; post it now.
            self.request_approval(repo, run.issue, run.id).await?;
            return Ok(Outcome::AwaitingApproval { run_id: run.id });
        };
        let posted = self.github.comment(repo, comment_id).await?.created_at;
        let approved = labels::has(labels, labels::PLAN_APPROVED.name)
            && issue::plan_approver(events, posted, &self.config.allowlist).is_some();
        if !approved {
            let expiry = self.config.approval_expiry;
            let expires = SignedDuration::try_from(expiry)
                .ok()
                .and_then(|wait| posted.checked_add(wait).ok())
                .unwrap_or(Timestamp::MAX);
            if self.now() >= expires {
                let end = self.engine.block(
                    run.id,
                    &format!(
                        "no one approved the plan within {}",
                        dsf_core::duration::format(expiry)
                    ),
                )?;
                self.report(repo, run.issue, &end).await;
                return Ok(Outcome::Ended(end));
            }
        }
        if !approved || !may_resume || run.status != RunStatus::Suspended {
            return Ok(Outcome::AwaitingApproval { run_id: run.id });
        }
        self.move_issue(repo, run.issue, Lifecycle::Running).await?;
        let end = self
            .engine
            .resume(run.id, ResumeOptions { approve: true })
            .await?;
        self.report(repo, run.issue, &end).await;
        Ok(Outcome::Ended(end))
    }

    /// Brings the issue up to date with how the run ended. A failure here
    /// is recorded rather than returned: the run's own state is saved, and
    /// the labels can be put right later.
    async fn report(&self, repo: &RepoSlug, number: u64, end: &RunEnd) {
        let run_id = match end {
            RunEnd::Published { run_id, .. }
            | RunEnd::Blocked { run_id, .. }
            | RunEnd::Suspended { run_id, .. }
            | RunEnd::PublishFailed { run_id, .. } => *run_id,
        };
        if let Err(error) = self.try_report(repo, number, end).await {
            self.engine.note(
                run_id,
                "github",
                &format!("updating {repo}#{number} failed: {error:#}"),
            );
        }
    }

    async fn try_report(&self, repo: &RepoSlug, number: u64, end: &RunEnd) -> Result<()> {
        let (state, comment) = match end {
            RunEnd::Published {
                pull_request: Some(pull_request),
                ..
            } => (
                Lifecycle::Review,
                format!(
                    "dsf opened a draft pull request for this: {}",
                    pull_request.url
                ),
            ),
            RunEnd::Published { branch, .. } => {
                (Lifecycle::Review, format!("dsf pushed `{branch}`."))
            }
            RunEnd::Blocked { run_id, reason } => (
                Lifecycle::Blocked,
                format!(
                    "dsf stopped (run {run_id}): {reason}\n\n\
                     Change the issue or the repository, then add `factory:ready` again to start over."
                ),
            ),
            RunEnd::PublishFailed { run_id, reason } => (
                Lifecycle::Blocked,
                format!("dsf finished the work but couldn't publish it (run {run_id}): {reason}"),
            ),
            RunEnd::Suspended {
                run_id,
                reason: SuspendReason::Approval,
                ..
            } => return self.request_approval(repo, number, *run_id).await,
            RunEnd::Suspended {
                run_id,
                reason: SuspendReason::UsageLimit,
                resume_after,
                ..
            } => {
                let mut comment = format!(
                    "dsf paused this (run {run_id}): the Claude usage limit was reached. \
                     It's back in the queue and continues from where it stopped"
                );
                match resume_after {
                    Some(after) => {
                        let _ = write!(comment, " after {after}.");
                    }
                    None => comment.push('.'),
                }
                (Lifecycle::Ready, comment)
            }
            RunEnd::Suspended {
                run_id,
                reason: SuspendReason::Interrupted,
                detail,
                ..
            } => (
                Lifecycle::Blocked,
                format!(
                    "dsf paused this (run {run_id}): {detail}\n\n\
                     Add `factory:ready` again to continue from where it stopped."
                ),
            ),
        };
        self.move_issue(repo, number, state).await?;
        self.github
            .post_comment(repo, number, &self.engine.mask(&comment))
            .await?;
        Ok(())
    }

    /// Posts the plan and waits for `factory:plan-approved`.
    async fn request_approval(&self, repo: &RepoSlug, number: u64, run_id: i64) -> Result<()> {
        let plan = self.engine.stored_run(run_id)?.plan.unwrap_or_default();
        let body = format!(
            "<!-- dsf:run={run_id} plan -->\n**dsf's plan** (run {run_id})\n\n{plan}\n\n\
             Add the `factory:plan-approved` label to go ahead. To change course instead, \
             edit the issue and add `factory:ready` again."
        );
        let comment = self
            .github
            .post_comment(repo, number, &self.engine.mask(&body))
            .await?;
        self.engine.set_approval_comment(run_id, comment.id)?;
        self.move_issue(repo, number, Lifecycle::AwaitingApproval)
            .await
    }

    /// Leaves `to` as the issue's only lifecycle label, from its labels as
    /// they are now.
    async fn move_issue(&self, repo: &RepoSlug, number: u64, to: Lifecycle) -> Result<()> {
        let current = self.github.issue(repo, number).await?.label_names();
        let change = labels::move_to(&current, to);
        for name in &change.remove {
            self.github.remove_label(repo, number, name).await?;
        }
        self.github.add_labels(repo, number, &change.add).await
    }

    /// Comments from trusted users only; anyone else's words never reach a
    /// prompt. dsf's own comments aren't trusted, so they're left out too.
    async fn trusted_comments(&self, repo: &RepoSlug, number: u64) -> Result<Vec<Comment>> {
        Ok(self
            .github
            .comments(repo, number)
            .await?
            .into_iter()
            .filter(|comment| self.config.allowlist.contains(&comment.user.login))
            .filter_map(|comment| {
                Some(Comment {
                    author: comment.user.login,
                    body: comment.body.filter(|body| !body.trim().is_empty())?,
                })
            })
            .collect())
    }

    /// The `blocked by` issues that are still open. One that can't be read
    /// counts as open, so the issue waits rather than runs early.
    async fn open_blockers(&self, repo: &RepoSlug, body: &str) -> Vec<u64> {
        let blockers = issue::blockers(body);
        if blockers.len() > MAX_BLOCKERS {
            return blockers;
        }
        let mut open = Vec::new();
        for number in blockers {
            let still_open = self
                .github
                .issue(repo, number)
                .await
                .map_or(true, |blocker| blocker.state == "open");
            if still_open {
                open.push(number);
            }
        }
        open
    }
}

/// Creates the `factory:` labels in a repository, and brings any that
/// differ back in line. Returns what it did, one line per label.
pub async fn init_labels(github: &GitHub, repo: &RepoSlug) -> Result<Vec<String>> {
    let existing = github.labels(repo).await?;
    let mut done = Vec::new();
    for spec in labels::ALL {
        let found = existing
            .iter()
            .find(|label| label.name.eq_ignore_ascii_case(spec.name));
        let action = match found {
            Some(label)
                if label.name == spec.name
                    && label.color.eq_ignore_ascii_case(spec.color)
                    && label.description.as_deref() == Some(spec.description) =>
            {
                "unchanged"
            }
            Some(_) => {
                github.put_label(repo, &spec, true).await?;
                "updated"
            }
            None => {
                github.put_label(repo, &spec, false).await?;
                "created"
            }
        };
        done.push(format!("{action:<9} {}", spec.name));
    }
    Ok(done)
}

fn lost_race(repo: &RepoSlug, number: u64) -> String {
    format!("{repo}#{number}: factory:ready was removed before dsf could claim it")
}

fn describe(reason: &SkipReason) -> String {
    match reason {
        SkipReason::NotReady => {
            "it isn't labelled factory:ready, or has another factory: state as well".to_owned()
        }
        SkipReason::UntrustedAuthor(login) => {
            format!("its author, {login}, isn't in github.allowed_users")
        }
        SkipReason::UntrustedLabeller(Some(login)) => {
            format!("factory:ready was added by {login}, who isn't in github.allowed_users")
        }
        SkipReason::UntrustedLabeller(None) => {
            "no one in github.allowed_users added factory:ready".to_owned()
        }
        SkipReason::WaitingOn(numbers) => {
            let list: Vec<String> = numbers.iter().map(|n| format!("#{n}")).collect();
            format!("it's blocked by open issue {}", list.join(", "))
        }
    }
}
