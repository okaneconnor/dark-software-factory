//! One pass of the factory: `dsf tick`, which a systemd timer runs every
//! few minutes. A tick keeps nothing in memory between passes; everything
//! it decides from is in the store or on GitHub, so a crash, a reboot or a
//! new binary between ticks loses nothing.
//!
//! 1. **Recover.** Runs a crash left mid-stage are marked interrupted, and
//!    their containers removed, so they continue from their last
//!    checkpoint. Finished runs' files older than `run_retention` go.
//! 2. **Check the host.** Docker must have the worker image and the disk
//!    enough room. If not, nothing starts or resumes: the problem is the
//!    host's, and failing runs one after another would wrongly block them.
//! 3. **Look.** Each repo's queue of `factory:ready` issues is listed and
//!    saved for `dsf status`. Issues still labelled `factory:running` with
//!    no run behind them are put right.
//! 4. **Work.** If claiming is allowed (inside the window, not paused, the
//!    host fit, not backing off), paused runs carry on and queued
//!    issues are claimed, up to `workers` at once, until nothing is left
//!    or claiming stops being allowed. Otherwise plans waiting for approval
//!    are still checked, so one no one approves expires on time.
//!
//! Anything wrong with the factory itself, rather than one issue, is a
//! *problem*: saved with the tick, recorded for the digest, and the reason
//! `dsf tick` exits non-zero, so systemd shows the unit failed. A fault in
//! a run (Docker or git failing under it) suspends the run to retry and
//! backs claiming off, half an hour at first and up to four hours.
//!
//! Every decision uses the factory's clock, so tests can run a tick at any
//! moment they like.

use std::collections::VecDeque;

use anyhow::{Context, Result};
use dsf_core::config::RepoSlug;
use dsf_core::labels;
use dsf_core::schedule::{self, ClaimDecision, ClaimState, Wait};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::engine::RunEnd;
use crate::factory::{Factory, Outcome};
use crate::store::{self, RunStatus, SuspendReason};
use crate::usage::UsageReading;

/// `kv` key: why claiming is paused, if it is. Set by `dsf pause`, or by
/// dsf itself when Claude stops accepting its token.
pub const PAUSED: &str = "paused";
/// `kv` key: no claiming before this time, after a usage limit or a fault.
pub const BACKOFF_UNTIL: &str = "backoff_until";
/// `kv` key: what the backoff is after, e.g. `a usage limit`.
pub const BACKOFF_REASON: &str = "backoff_reason";
/// `kv` key: the last tick's [`TickRecord`].
pub const LAST_TICK: &str = "last_tick";

/// What the last tick saw and did, for `dsf status` and the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickRecord {
    pub at: Timestamp,
    /// Issues labelled `factory:ready`, in the order they'd be claimed.
    pub queue: Vec<QueuedIssue>,
    /// What happened, one line each, masked.
    pub notes: Vec<String>,
    /// What's wrong with the factory itself, masked. Any at all make
    /// `dsf tick` exit non-zero.
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedIssue {
    pub repo: String,
    pub number: u64,
    pub title: String,
}

/// Work a tick may do.
enum Candidate {
    /// A paused run whose issue isn't queued.
    Paused(Box<store::Run>),
    Queued(RepoSlug, u64),
}

pub struct Scheduler {
    factory: Factory,
    tz: TimeZone,
    check_host: bool,
}

impl Scheduler {
    pub fn new(factory: Factory, tz: TimeZone) -> Self {
        Self {
            factory,
            tz,
            check_host: true,
        }
    }

    /// Takes the host as fit to run work without checking, for tests that
    /// never start a container and may have no Docker to ask.
    #[must_use]
    pub fn without_host_checks(mut self) -> Self {
        self.check_host = false;
        self
    }

    pub fn factory(&self) -> &Factory {
        &self.factory
    }

    /// One pass: recover, check the host, look, work. Problems with one
    /// repo or issue are noted and the rest carry on; only the store or
    /// Docker failing stops the tick.
    pub async fn tick(&self) -> Result<TickRecord> {
        let mut log = Log::default();
        for run_id in self.factory.engine().recover().await? {
            log.notes.push(format!(
                "run {run_id} was interrupted; it continues from its last checkpoint"
            ));
        }
        let config = self.factory.config();
        match self.factory.engine().prune(config.run_retention) {
            Ok(0) => {}
            Ok(removed) => log.notes.push(format!(
                "removed the files of {removed} run(s) finished over {} ago",
                dsf_core::duration::format(config.run_retention)
            )),
            Err(error) => log
                .problems
                .push(format!("removing old runs' files: {error:#}")),
        }
        if self.check_host {
            let min_free = u64::from(config.min_free_disk_gb).saturating_mul(1_000_000_000);
            log.problems
                .extend(self.factory.engine().preflight(min_free).await);
        }
        let healthy = log.problems.is_empty();

        self.forget_unconfigured(&mut log.notes)?;

        let mut queues = Vec::new();
        for repo in &config.repos {
            match self.look_at(&repo.slug, &mut log.notes).await {
                Ok(queue) => queues.push(queue),
                Err(error) => log.problems.push(format!("{}: {error:#}", repo.slug)),
            }
        }
        let queue = round_robin(queues);
        // Saved now as well as at the end, so `dsf status` shows this tick's
        // queue while the tick works through it, which can take hours.
        self.save(&queue, &log)?;

        // Only runs on configured repos: `dsf run --local` may share the
        // state directory on a development machine.
        let paused: Vec<store::Run> = self
            .factory
            .engine()
            .store()?
            .unfinished_runs()?
            .into_iter()
            .filter(|run| {
                run.status == RunStatus::Suspended
                    && run
                        .repo
                        .parse::<RepoSlug>()
                        .is_ok_and(|slug| config.repo(&slug).is_some())
            })
            .collect();
        match self.decision(0, healthy)? {
            ClaimDecision::Claim { .. } => {
                let candidates = paused
                    .into_iter()
                    .map(|run| Candidate::Paused(Box::new(run)))
                    .chain(queue.iter().filter_map(|issue| {
                        Some(Candidate::Queued(issue.repo.parse().ok()?, issue.number))
                    }))
                    .collect();
                self.work(candidates, &mut log).await?;
            }
            ClaimDecision::Wait(wait) => {
                log.notes.push(self.describe(wait));
                for run in paused
                    .iter()
                    .filter(|run| run.suspend_reason == Some(SuspendReason::Approval))
                {
                    let outcome = self.factory.continue_run(run, false).await;
                    self.record(&format!("{}#{}", run.repo, run.issue), outcome, &mut log)?;
                }
            }
        }

        let record = self.save(&queue, &log)?;
        let mut store = self.factory.engine().store()?;
        for problem in &record.problems {
            store.problem(problem, record.at)?;
        }
        Ok(record)
    }

    /// Cancels the unfinished runs of repos taken out of the config, so
    /// `dsf status` and the digest stop showing them. Local runs, which
    /// may share a development machine's state directory, are left alone.
    fn forget_unconfigured(&self, notes: &mut Vec<String>) -> Result<()> {
        let config = self.factory.config();
        let gone: Vec<store::Run> = self
            .factory
            .engine()
            .store()?
            .unfinished_runs()?
            .into_iter()
            .filter(|run| {
                !run.repo.starts_with("local/")
                    && !run
                        .repo
                        .parse::<RepoSlug>()
                        .is_ok_and(|slug| config.repo(&slug).is_some())
            })
            .collect();
        for run in gone {
            self.factory
                .engine()
                .cancel(run.id, "its repo is no longer in the config")?;
            notes.push(format!(
                "{}#{}: run {} canceled, because its repo is no longer in the config",
                run.repo, run.issue, run.id
            ));
        }
        Ok(())
    }

    /// Records what this tick has seen and done so far, for `dsf status`.
    fn save(&self, queue: &[QueuedIssue], log: &Log) -> Result<TickRecord> {
        let mask = |lines: &[String]| -> Vec<String> {
            lines
                .iter()
                .map(|line| self.factory.engine().mask(line))
                .collect()
        };
        let record = TickRecord {
            at: self.factory.now(),
            queue: queue.to_vec(),
            notes: mask(&log.notes),
            problems: mask(&log.problems),
        };
        self.factory
            .engine()
            .store()?
            .kv_set(LAST_TICK, &serde_json::to_string(&record)?)?;
        Ok(record)
    }

    /// Lists a repo's queue, oldest issue first, and puts right any issue
    /// labelled `factory:running` that no unfinished run is working on.
    async fn look_at(&self, repo: &RepoSlug, notes: &mut Vec<String>) -> Result<Vec<QueuedIssue>> {
        let github = self.factory.github();
        let name = repo.to_string();
        for issue in github
            .open_issues_labelled(repo, labels::RUNNING.name)
            .await?
        {
            let active = self
                .factory
                .engine()
                .active_run(&name, issue.number)?
                .is_some();
            if !active {
                match self.factory.repair(repo, issue.number).await {
                    Ok(note) => notes.push(note),
                    Err(error) => notes.push(format!("{repo}#{}: {error:#}", issue.number)),
                }
            }
        }
        let mut queue: Vec<QueuedIssue> = github
            .open_issues_labelled(repo, labels::READY.name)
            .await?
            .into_iter()
            .map(|issue| QueuedIssue {
                repo: name.clone(),
                number: issue.number,
                title: issue.title,
            })
            .collect();
        queue.sort_by_key(|issue| issue.number);
        Ok(queue)
    }

    /// Runs candidates, up to `workers` at once, while claiming is
    /// allowed. Each run's end is acted on before the next claim: a usage
    /// limit or a fault starts a backoff, and a token Claude won't accept
    /// pauses the factory, so none of them burns through the rest of the
    /// queue.
    async fn work(&self, candidates: Vec<Candidate>, log: &mut Log) -> Result<()> {
        let workers = self.factory.config().workers;
        let mut pending: VecDeque<Candidate> = candidates.into();
        let mut running = FuturesUnordered::new();
        let mut stopped = false;
        loop {
            while !stopped && running.len() < workers {
                if let ClaimDecision::Wait(wait) = self.decision(running.len(), true)? {
                    log.notes
                        .push(format!("stopped claiming: {}", self.describe(wait)));
                    stopped = true;
                    break;
                }
                let Some(candidate) = pending.pop_front() else {
                    break;
                };
                running.push(self.attempt(candidate));
            }
            let Some((label, outcome)) = running.next().await else {
                break;
            };
            self.record(&label, outcome, log)?;
        }
        Ok(())
    }

    async fn attempt(&self, candidate: Candidate) -> (String, Result<Outcome>) {
        match candidate {
            Candidate::Paused(run) => (
                format!("{}#{}", run.repo, run.issue),
                self.factory.continue_run(&run, true).await,
            ),
            Candidate::Queued(repo, number) => (
                format!("{repo}#{number}"),
                self.factory.work_on(&repo, number).await,
            ),
        }
    }

    /// Notes an outcome and acts on how a run ended.
    fn record(&self, label: &str, outcome: Result<Outcome>, log: &mut Log) -> Result<()> {
        let end = match outcome {
            Err(error) => {
                // Something failed outside any run's stages, e.g. GitHub or
                // Docker when resuming. Trying the next issue at once would
                // likely fail the same way.
                let until = self.factory.engine().fault_backoff()?;
                log.problems.push(format!("{label}: {error:#}"));
                self.back_off(until, "a fault")?;
                return Ok(());
            }
            Ok(Outcome::Skipped(reason)) => {
                log.notes.push(reason);
                return Ok(());
            }
            Ok(Outcome::AwaitingApproval { run_id }) => {
                log.notes.push(format!(
                    "{label}: run {run_id} is waiting for plan approval"
                ));
                return Ok(());
            }
            Ok(Outcome::Ended(end)) => end,
        };
        match &end {
            RunEnd::Faulted { retry_after, .. } => {
                log.problems.push(format!("{label}: {}", summarise(&end)));
                self.back_off(*retry_after, "a fault")?;
            }
            RunEnd::Suspended {
                reason: SuspendReason::UsageLimit,
                resume_after: Some(until),
                ..
            } => {
                log.notes.push(format!("{label}: {}", summarise(&end)));
                self.back_off(*until, "a usage limit")?;
            }
            RunEnd::Suspended {
                run_id,
                reason: SuspendReason::Interrupted,
                detail,
                ..
            } => {
                log.problems.push(format!("{label}: {}", summarise(&end)));
                self.factory
                    .engine()
                    .store()?
                    .kv_set(PAUSED, &format!("run {run_id}: {detail}"))?;
            }
            _ => log.notes.push(format!("{label}: {}", summarise(&end))),
        }
        Ok(())
    }

    /// No claiming until `until`, unless a longer backoff is already set.
    fn back_off(&self, until: Timestamp, after: &str) -> Result<()> {
        let mut store = self.factory.engine().store()?;
        let current = store
            .kv_get(BACKOFF_UNTIL)?
            .and_then(|text| text.parse::<Timestamp>().ok());
        if current.is_none_or(|current| current < until) {
            store.kv_set(BACKOFF_UNTIL, &until.to_string())?;
            store.kv_set(BACKOFF_REASON, after)?;
        }
        Ok(())
    }

    /// Whether another run may start now, with `active` already running
    /// and the host found `healthy` at the start of the tick.
    fn decision(&self, active: usize, healthy: bool) -> Result<ClaimDecision> {
        let store = self.factory.engine().store()?;
        let now = self.factory.now();
        let config = self.factory.config();
        let state = ClaimState {
            paused: store.kv_get(PAUSED)?.is_some(),
            healthy,
            backoff_until: store
                .kv_get(BACKOFF_UNTIL)?
                .map(|text| text.parse::<Timestamp>())
                .transpose()
                .context("reading the stored backoff")?,
            usage_capped_until: UsageReading::load(&store)?
                .and_then(|reading| reading.capped_until(config.weekly_usage_cap, now)),
            active_runs: active,
        };
        Ok(schedule::claim_decision(
            now,
            &self.tz,
            &config.window,
            config.workers,
            &state,
        ))
    }

    fn describe(&self, wait: Wait) -> String {
        match wait {
            Wait::Paused => {
                let reason = self
                    .factory
                    .engine()
                    .store()
                    .ok()
                    .and_then(|store| store.kv_get(PAUSED).ok().flatten())
                    .unwrap_or_default();
                format!("claiming is paused ({reason}); `dsf unpause` restarts it")
            }
            Wait::Unhealthy => {
                "not starting anything: this host can't run work (see the problems)".to_owned()
            }
            Wait::BackingOff { until } => {
                let after = self
                    .factory
                    .engine()
                    .store()
                    .ok()
                    .and_then(|store| store.kv_get(BACKOFF_REASON).ok().flatten())
                    .unwrap_or_else(|| "a usage limit".to_owned());
                format!("backing off after {after} until {}", local(until, &self.tz))
            }
            Wait::OverUsageCap { until } => {
                let cap = self.factory.config().weekly_usage_cap;
                let why = self
                    .factory
                    .engine()
                    .store()
                    .ok()
                    .and_then(|store| UsageReading::load(&store).ok().flatten())
                    .map_or_else(
                        || format!("the weekly Claude allowance is over the {cap}% cap"),
                        |reading| reading.over_cap(cap),
                    );
                format!("{why}, so nothing starts until {}", local(until, &self.tz))
            }
            Wait::WindowClosed { opens_at } => format!(
                "outside the window, which opens at {}",
                local(opens_at, &self.tz)
            ),
            Wait::AtCapacity => "every worker is busy".to_owned(),
        }
    }
}

/// A time as the operator reads it, e.g. `Mon 19 Oct 23:00`.
pub fn local(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone())
        .strftime("%a %-d %b %H:%M")
        .to_string()
}

fn summarise(end: &RunEnd) -> String {
    match end {
        RunEnd::Published {
            run_id,
            pull_request: Some(pull_request),
            ..
        } => format!("run {run_id} opened {}", pull_request.url),
        RunEnd::Published { run_id, branch, .. } => format!("run {run_id} pushed {branch}"),
        RunEnd::Blocked { run_id, reason } => format!("run {run_id} blocked: {reason}"),
        RunEnd::PublishFailed { run_id, reason } => {
            format!("run {run_id} couldn't publish: {reason}")
        }
        RunEnd::Suspended {
            run_id,
            detail,
            resume_after: Some(after),
            ..
        } => format!("run {run_id} paused until {after}: {detail}"),
        RunEnd::Suspended { run_id, detail, .. } => format!("run {run_id} paused: {detail}"),
        RunEnd::Faulted {
            run_id,
            detail,
            retry_after,
        } => format!("run {run_id} hit a fault and tries again after {retry_after}: {detail}"),
    }
}

/// What a tick has to say so far.
#[derive(Default)]
struct Log {
    notes: Vec<String>,
    problems: Vec<String>,
}

/// Interleaves per-repo queues, so one busy repo can't starve the rest.
fn round_robin(queues: Vec<Vec<QueuedIssue>>) -> Vec<QueuedIssue> {
    let mut queues: Vec<VecDeque<QueuedIssue>> = queues.into_iter().map(Into::into).collect();
    let mut merged = Vec::new();
    loop {
        let before = merged.len();
        for queue in &mut queues {
            if let Some(issue) = queue.pop_front() {
                merged.push(issue);
            }
        }
        if merged.len() == before {
            return merged;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(repo: &str, number: u64) -> QueuedIssue {
        QueuedIssue {
            repo: repo.to_owned(),
            number,
            title: String::new(),
        }
    }

    #[test]
    fn queues_take_turns_by_repo() {
        let merged = round_robin(vec![
            vec![issue("a/x", 1), issue("a/x", 2), issue("a/x", 3)],
            vec![],
            vec![issue("b/y", 7)],
        ]);
        let order: Vec<String> = merged
            .iter()
            .map(|issue| format!("{}#{}", issue.repo, issue.number))
            .collect();
        assert_eq!(order, ["a/x#1", "b/y#7", "a/x#2", "a/x#3"]);
    }

    #[test]
    fn local_times_read_naturally() {
        let tz = TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").unwrap();
        let winter: Timestamp = "2026-12-01T22:00:00Z".parse().unwrap();
        assert_eq!(local(winter, &tz), "Tue 1 Dec 22:00");
        let summer: Timestamp = "2026-07-01T22:00:00Z".parse().unwrap();
        assert_eq!(local(summer, &tz), "Wed 1 Jul 23:00");
    }
}
