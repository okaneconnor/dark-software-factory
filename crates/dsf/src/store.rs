//! Durable run state in SQLite: plain rows, updated in one transaction at
//! every stage boundary. Those boundaries are the only points where a run
//! stops and resumes, so a crash loses at most the stage in flight.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use dsf_core::routing::Progress;
use jiff::Timestamp;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params};
use rusqlite_migration::{M, Migrations};

const SCHEMA_V1: &str = "
CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    issue INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN
        ('queued', 'running', 'suspended', 'published', 'blocked', 'publish_failed', 'canceled')),
    suspend_reason TEXT CHECK (suspend_reason IN ('approval', 'usage_limit', 'interrupted')),
    resume_after TEXT,
    next_stage TEXT,
    progress TEXT NOT NULL,
    base_sha TEXT,
    checkpoint_sha TEXT,
    workflow TEXT NOT NULL,
    issue_title TEXT NOT NULL,
    issue_body TEXT NOT NULL,
    issue_author TEXT NOT NULL,
    issue_updated_at TEXT,
    plan TEXT,
    approval_comment_id INTEGER,
    end_reason TEXT,
    pr_number INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

-- At most one unfinished run per issue: the guard against double claims.
CREATE UNIQUE INDEX runs_one_active_per_issue ON runs (repo, issue)
    WHERE status IN ('queued', 'running', 'suspended');

CREATE TABLE stage_attempts (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs (id),
    stage TEXT NOT NULL,
    visit INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'finished', 'interrupted')),
    outcome TEXT,
    failure TEXT,
    container_id TEXT,
    before_sha TEXT,
    after_sha TEXT,
    exit_code INTEGER,
    usage TEXT,
    structured_output TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT
) STRICT;
CREATE INDEX stage_attempts_by_run ON stage_attempts (run_id);

-- A human-readable timeline for the board and digest; not the source of truth.
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    run_id INTEGER REFERENCES runs (id),
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL
) STRICT;
CREATE INDEX events_by_run ON events (run_id);

CREATE TABLE kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
";

/// Where a run's repository comes from and where its branch goes. Kept on
/// the run so it can resume without being told again.
const SCHEMA_V2: &str = "
ALTER TABLE runs ADD COLUMN clone_url TEXT NOT NULL DEFAULT '';
ALTER TABLE runs ADD COLUMN publish_url TEXT NOT NULL DEFAULT '';
";

/// What a run was told at claim time beyond the issue itself: the trusted
/// comments, as JSON, and whether its plan needs a human's approval.
const SCHEMA_V3: &str = "
ALTER TABLE runs ADD COLUMN issue_comments TEXT NOT NULL DEFAULT '[]';
ALTER TABLE runs ADD COLUMN require_approval INTEGER NOT NULL DEFAULT 0;
";

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(SCHEMA_V1), M::up(SCHEMA_V2), M::up(SCHEMA_V3)])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Suspended,
    Published,
    Blocked,
    PublishFailed,
    Canceled,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Published => "published",
            Self::Blocked => "blocked",
            Self::PublishFailed => "publish_failed",
            Self::Canceled => "canceled",
        }
    }

    pub const fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Blocked | Self::PublishFailed | Self::Canceled
        )
    }
}

impl FromStr for RunStatus {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Suspended,
            Self::Published,
            Self::Blocked,
            Self::PublishFailed,
            Self::Canceled,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
        .ok_or_else(|| anyhow!("unknown run status `{text}`"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendReason {
    Approval,
    UsageLimit,
    Interrupted,
}

impl SuspendReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::UsageLimit => "usage_limit",
            Self::Interrupted => "interrupted",
        }
    }
}

impl FromStr for SuspendReason {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        [Self::Approval, Self::UsageLimit, Self::Interrupted]
            .into_iter()
            .find(|reason| reason.as_str() == text)
            .ok_or_else(|| anyhow!("unknown suspend reason `{text}`"))
    }
}

/// What a new run is created from: the issue as it was when claimed and
/// the workflow snapshot it will run under.
#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    pub repo: &'a str,
    pub issue: u64,
    pub title: &'a str,
    pub body: &'a str,
    pub author: &'a str,
    pub updated_at: Option<&'a str>,
    pub workflow: &'a str,
    pub clone_url: &'a str,
    pub publish_url: &'a str,
    /// JSON: the trusted comments the prompts include.
    pub comments: &'a str,
    pub require_approval: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: i64,
    pub repo: String,
    pub issue: u64,
    pub status: RunStatus,
    pub suspend_reason: Option<SuspendReason>,
    pub resume_after: Option<Timestamp>,
    pub next_stage: Option<String>,
    pub progress: Progress,
    pub base_sha: Option<String>,
    pub checkpoint_sha: Option<String>,
    pub workflow: String,
    pub issue_title: String,
    pub issue_body: String,
    pub issue_author: String,
    pub plan: Option<String>,
    pub end_reason: Option<String>,
    pub pr_number: Option<u64>,
    pub clone_url: String,
    pub publish_url: String,
    pub issue_comments: String,
    pub require_approval: bool,
    /// The comment that posted the plan for approval.
    pub approval_comment_id: Option<u64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub id: i64,
    pub stage: String,
    pub visit: u32,
    pub status: String,
    pub outcome: Option<String>,
    pub failure: Option<String>,
    pub container_id: Option<String>,
    pub exit_code: Option<i64>,
    pub after_sha: Option<String>,
    /// The JSON Claude returned for a schema stage.
    pub structured_output: Option<String>,
}

/// How a stage attempt ended.
#[derive(Debug, Clone, Copy, Default)]
pub struct AttemptEnd<'a> {
    pub outcome: Option<&'a str>,
    /// A failure class or signature, e.g. `verify:9f2c…` or `usage_limit`.
    pub failure: Option<&'a str>,
    pub exit_code: Option<i64>,
    pub after_sha: Option<&'a str>,
    pub usage: Option<&'a serde_json::Value>,
    pub structured_output: Option<&'a serde_json::Value>,
}

/// Where the run goes after an attempt.
#[derive(Debug, Clone, Copy)]
pub enum Advance<'a> {
    Next {
        stage: &'a str,
    },
    Suspend {
        reason: SuspendReason,
        resume_after: Option<Timestamp>,
    },
    End {
        status: RunStatus,
        reason: &'a str,
    },
}

/// A stage attempt found unfinished at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub run_id: i64,
    pub attempt_id: i64,
    pub stage: String,
    pub container_id: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations()
            .to_latest(&mut conn)
            .context("migrating the database")?;
        Ok(Self { conn })
    }

    /// Creates a queued run, or returns `None` if the issue already has an
    /// unfinished one.
    pub fn create_run(&mut self, new: &NewRun<'_>, now: Timestamp) -> Result<Option<i64>> {
        let progress = serde_json::to_string(&Progress::default())?;
        let now = now.to_string();
        let inserted = self.conn.execute(
            "INSERT INTO runs (repo, issue, status, progress, workflow, issue_title, issue_body,
                               issue_author, issue_updated_at, created_at, updated_at, clone_url, publish_url,
                               issue_comments, require_approval)
             VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, ?12, ?13)",
            params![
                new.repo,
                i64::try_from(new.issue)?,
                progress,
                new.workflow,
                new.title,
                new.body,
                new.author,
                new.updated_at,
                now,
                new.clone_url,
                new.publish_url,
                new.comments,
                new.require_approval
            ],
        );
        match inserted {
            Ok(_) => Ok(Some(self.conn.last_insert_rowid())),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn run(&self, id: i64) -> Result<Run> {
        self.conn
            .query_row("SELECT * FROM runs WHERE id = ?1", [id], RunRow::read)
            .optional()?
            .with_context(|| format!("run {id} not found"))?
            .parse()
    }

    /// The issue's unfinished run, if it has one.
    pub fn active_run(&self, repo: &str, issue: u64) -> Result<Option<Run>> {
        self.conn
            .query_row(
                "SELECT * FROM runs WHERE repo = ?1 AND issue = ?2
                 AND status IN ('queued', 'running', 'suspended')",
                params![repo, i64::try_from(issue)?],
                RunRow::read,
            )
            .optional()?
            .map(RunRow::parse)
            .transpose()
    }

    /// The issue's most recent run, finished or not.
    pub fn latest_run(&self, repo: &str, issue: u64) -> Result<Option<Run>> {
        self.conn
            .query_row(
                "SELECT * FROM runs WHERE repo = ?1 AND issue = ?2 ORDER BY id DESC LIMIT 1",
                params![repo, i64::try_from(issue)?],
                RunRow::read,
            )
            .optional()?
            .map(RunRow::parse)
            .transpose()
    }

    /// Every run that hasn't finished, oldest first.
    pub fn unfinished_runs(&self) -> Result<Vec<Run>> {
        self.runs(
            "SELECT * FROM runs WHERE status IN ('queued', 'running', 'suspended') ORDER BY id",
            [],
        )
    }

    /// Runs that finished at or after `since`, most recent first.
    pub fn finished_since(&self, since: Timestamp) -> Result<Vec<Run>> {
        self.runs(
            "SELECT * FROM runs
             WHERE status IN ('published', 'blocked', 'publish_failed', 'canceled')
               AND updated_at >= ?1
             ORDER BY updated_at DESC, id DESC",
            [since.to_string()],
        )
    }

    fn runs(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Run>> {
        let mut query = self.conn.prepare(sql)?;
        let rows = query
            .query_map(params, RunRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(RunRow::parse).collect()
    }

    pub fn set_approval_comment(
        &mut self,
        run_id: i64,
        comment_id: u64,
        now: Timestamp,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET approval_comment_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, i64::try_from(comment_id)?, now.to_string()],
        )?;
        Ok(())
    }

    /// Records the commit a run starts from. It is also the first checkpoint.
    pub fn set_base(&mut self, run_id: i64, sha: &str, now: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET base_sha = ?2, checkpoint_sha = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, sha, now.to_string()],
        )?;
        Ok(())
    }

    /// Replaces the workflow snapshot, once the repo's own has been read.
    pub fn set_workflow(&mut self, run_id: i64, workflow: &str, now: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET workflow = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, workflow, now.to_string()],
        )?;
        Ok(())
    }

    /// Saves routing state between boundaries, e.g. an interruption count.
    pub fn set_progress(&mut self, run_id: i64, progress: &Progress, now: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET progress = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, serde_json::to_string(progress)?, now.to_string()],
        )?;
        Ok(())
    }

    /// Suspends a run between stages, before `next_stage` starts. No
    /// attempt is opened, and the visit `progress` counts for the stage
    /// stands: the run resumes with that stage.
    pub fn suspend_before(
        &mut self,
        run_id: i64,
        next_stage: &str,
        progress: &Progress,
        reason: SuspendReason,
        resume_after: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = 'suspended', suspend_reason = ?2, resume_after = ?3,
                             next_stage = ?4, progress = ?5, updated_at = ?6
             WHERE id = ?1",
            params![
                run_id,
                reason.as_str(),
                resume_after.map(|t| t.to_string()),
                next_stage,
                serde_json::to_string(progress)?,
                now.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn set_plan(&mut self, run_id: i64, plan: &str, now: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET plan = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, plan, now.to_string()],
        )?;
        Ok(())
    }

    pub fn set_pr(&mut self, run_id: i64, number: u64, now: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET pr_number = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, i64::try_from(number)?, now.to_string()],
        )?;
        Ok(())
    }

    /// Opens an attempt at `stage` and marks the run as running it.
    pub fn start_attempt(
        &mut self,
        run_id: i64,
        stage: &str,
        visit: u32,
        before_sha: Option<&str>,
        now: Timestamp,
    ) -> Result<i64> {
        let now = now.to_string();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO stage_attempts (run_id, stage, visit, status, before_sha, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
            params![run_id, stage, visit, before_sha, now],
        )?;
        let attempt_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE runs SET status = 'running', next_stage = ?2, suspend_reason = NULL,
                             resume_after = NULL, updated_at = ?3
             WHERE id = ?1",
            params![run_id, stage, now],
        )?;
        tx.commit()?;
        Ok(attempt_id)
    }

    pub fn set_container(&mut self, attempt_id: i64, container_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE stage_attempts SET container_id = ?2 WHERE id = ?1",
            params![attempt_id, container_id],
        )?;
        Ok(())
    }

    /// Closes an attempt and moves the run on, atomically: the stage
    /// boundary. `checkpoint` is the commit holding the stage's work, if it
    /// produced any.
    #[allow(clippy::too_many_arguments)] // Each is one column of the boundary.
    pub fn finish_attempt(
        &mut self,
        run_id: i64,
        attempt_id: i64,
        end: &AttemptEnd<'_>,
        progress: &Progress,
        checkpoint: Option<&str>,
        advance: Advance<'_>,
        now: Timestamp,
    ) -> Result<()> {
        let now = now.to_string();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE stage_attempts
             SET status = 'finished', outcome = ?2, failure = ?3, exit_code = ?4, after_sha = ?5,
                 usage = ?6, structured_output = ?7, finished_at = ?8
             WHERE id = ?1 AND run_id = ?9",
            params![
                attempt_id,
                end.outcome,
                end.failure,
                end.exit_code,
                end.after_sha,
                end.usage.map(ToString::to_string),
                end.structured_output.map(ToString::to_string),
                now,
                run_id
            ],
        )?;
        tx.execute(
            "UPDATE runs SET progress = ?2, checkpoint_sha = COALESCE(?3, checkpoint_sha), updated_at = ?4
             WHERE id = ?1",
            params![run_id, serde_json::to_string(progress)?, checkpoint, now],
        )?;
        match advance {
            Advance::Next { stage } => tx.execute(
                "UPDATE runs SET status = 'running', next_stage = ?2 WHERE id = ?1",
                params![run_id, stage],
            )?,
            Advance::Suspend {
                reason,
                resume_after,
            } => tx.execute(
                "UPDATE runs SET status = 'suspended', suspend_reason = ?2, resume_after = ?3
                 WHERE id = ?1",
                params![run_id, reason.as_str(), resume_after.map(|t| t.to_string())],
            )?,
            Advance::End { status, reason } => {
                anyhow::ensure!(
                    status.is_finished(),
                    "{} is not a final status",
                    status.as_str()
                );
                tx.execute(
                    "UPDATE runs SET status = ?2, end_reason = ?3, next_stage = NULL WHERE id = ?1",
                    params![run_id, status.as_str(), reason],
                )?
            }
        };
        tx.commit()?;
        Ok(())
    }

    /// Ends a run outside a stage boundary, e.g. when publishing fails or
    /// setup is refused.
    pub fn end_run(
        &mut self,
        run_id: i64,
        status: RunStatus,
        reason: &str,
        now: Timestamp,
    ) -> Result<()> {
        anyhow::ensure!(
            status.is_finished(),
            "{} is not a final status",
            status.as_str()
        );
        self.conn.execute(
            "UPDATE runs SET status = ?2, end_reason = ?3, next_stage = NULL, updated_at = ?4
             WHERE id = ?1",
            params![run_id, status.as_str(), reason, now.to_string()],
        )?;
        Ok(())
    }

    /// After a crash: marks every attempt left running as interrupted and
    /// suspends its run, returning them so their containers can be removed.
    /// `run` limits this to one run, so resuming it can't disturb another
    /// that is genuinely in progress.
    pub fn interrupt_unfinished(
        &mut self,
        run: Option<i64>,
        now: Timestamp,
    ) -> Result<Vec<Interrupted>> {
        let now = now.to_string();
        let tx = self.conn.transaction()?;
        let interrupted = {
            let mut query = tx.prepare(
                "SELECT run_id, id, stage, container_id FROM stage_attempts
                 WHERE status = 'running' AND (?1 IS NULL OR run_id = ?1) ORDER BY id",
            )?;
            query
                .query_map([run], |row| {
                    Ok(Interrupted {
                        run_id: row.get(0)?,
                        attempt_id: row.get(1)?,
                        stage: row.get(2)?,
                        container_id: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "UPDATE stage_attempts SET status = 'interrupted', finished_at = ?1
             WHERE status = 'running' AND (?2 IS NULL OR run_id = ?2)",
            params![now, run],
        )?;
        tx.execute(
            "UPDATE runs SET status = 'suspended', suspend_reason = 'interrupted', updated_at = ?1
             WHERE status = 'running' AND (?2 IS NULL OR id = ?2)",
            params![now, run],
        )?;
        tx.commit()?;
        Ok(interrupted)
    }

    pub fn attempts(&self, run_id: i64) -> Result<Vec<Attempt>> {
        let mut query = self.conn.prepare(
            "SELECT id, stage, visit, status, outcome, failure, container_id, exit_code, after_sha,
                    structured_output
             FROM stage_attempts WHERE run_id = ?1 ORDER BY id",
        )?;
        let attempts = query
            .query_map([run_id], |row| {
                Ok(Attempt {
                    id: row.get(0)?,
                    stage: row.get(1)?,
                    visit: row.get(2)?,
                    status: row.get(3)?,
                    outcome: row.get(4)?,
                    failure: row.get(5)?,
                    container_id: row.get(6)?,
                    exit_code: row.get(7)?,
                    after_sha: row.get(8)?,
                    structured_output: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(attempts)
    }

    /// Appends to the timeline. `message` must already be masked.
    pub fn event(
        &mut self,
        run_id: Option<i64>,
        kind: &str,
        message: &str,
        now: Timestamp,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (run_id, at, kind, message) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, now.to_string(), kind, message],
        )?;
        Ok(())
    }

    pub fn events(&self, run_id: i64) -> Result<Vec<(String, String)>> {
        let mut query = self
            .conn
            .prepare("SELECT kind, message FROM events WHERE run_id = ?1 ORDER BY id")?;
        let events = query
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(events)
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn kv_set(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&mut self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }
}

/// A `runs` row as stored, before parsing its text columns.
struct RunRow {
    id: i64,
    repo: String,
    issue: i64,
    status: String,
    suspend_reason: Option<String>,
    resume_after: Option<String>,
    next_stage: Option<String>,
    progress: String,
    base_sha: Option<String>,
    checkpoint_sha: Option<String>,
    workflow: String,
    issue_title: String,
    issue_body: String,
    issue_author: String,
    plan: Option<String>,
    end_reason: Option<String>,
    pr_number: Option<i64>,
    clone_url: String,
    publish_url: String,
    issue_comments: String,
    require_approval: bool,
    approval_comment_id: Option<i64>,
    created_at: String,
    updated_at: String,
}

impl RunRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            repo: row.get("repo")?,
            issue: row.get("issue")?,
            status: row.get("status")?,
            suspend_reason: row.get("suspend_reason")?,
            resume_after: row.get("resume_after")?,
            next_stage: row.get("next_stage")?,
            progress: row.get("progress")?,
            base_sha: row.get("base_sha")?,
            checkpoint_sha: row.get("checkpoint_sha")?,
            workflow: row.get("workflow")?,
            issue_title: row.get("issue_title")?,
            issue_body: row.get("issue_body")?,
            issue_author: row.get("issue_author")?,
            plan: row.get("plan")?,
            end_reason: row.get("end_reason")?,
            pr_number: row.get("pr_number")?,
            clone_url: row.get("clone_url")?,
            publish_url: row.get("publish_url")?,
            issue_comments: row.get("issue_comments")?,
            require_approval: row.get("require_approval")?,
            approval_comment_id: row.get("approval_comment_id")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn parse(self) -> Result<Run> {
        Ok(Run {
            id: self.id,
            repo: self.repo,
            issue: u64::try_from(self.issue)?,
            status: self.status.parse()?,
            suspend_reason: self.suspend_reason.as_deref().map(str::parse).transpose()?,
            resume_after: self.resume_after.as_deref().map(str::parse).transpose()?,
            next_stage: self.next_stage,
            progress: serde_json::from_str(&self.progress).context("reading run progress")?,
            base_sha: self.base_sha,
            checkpoint_sha: self.checkpoint_sha,
            workflow: self.workflow,
            issue_title: self.issue_title,
            issue_body: self.issue_body,
            issue_author: self.issue_author,
            plan: self.plan,
            end_reason: self.end_reason,
            pr_number: self.pr_number.map(u64::try_from).transpose()?,
            clone_url: self.clone_url,
            publish_url: self.publish_url,
            issue_comments: self.issue_comments,
            require_approval: self.require_approval,
            approval_comment_id: self.approval_comment_id.map(u64::try_from).transpose()?,
            created_at: self.created_at.parse()?,
            updated_at: self.updated_at.parse()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsf_core::workflow::{StageId, Workflow};

    fn now() -> Timestamp {
        "2026-10-18T23:00:00Z".parse().unwrap()
    }

    fn issue(number: u64) -> NewRun<'static> {
        NewRun {
            repo: "octocat/hello",
            issue: number,
            title: "Fix add",
            body: "add() subtracts",
            author: "octocat",
            updated_at: Some("2026-10-18T22:00:00Z"),
            workflow: dsf_core::workflow::DEFAULT_TOML,
            clone_url: "https://github.com/octocat/hello.git",
            publish_url: "https://github.com/octocat/hello.git",
            comments: "[]",
            require_approval: false,
        }
    }

    #[test]
    fn migrations_are_valid() {
        migrations().validate().unwrap();
    }

    #[test]
    fn one_unfinished_run_per_issue() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_run(&issue(1), now()).unwrap().unwrap();
        assert_eq!(store.create_run(&issue(1), now()).unwrap(), None);
        assert!(store.create_run(&issue(2), now()).unwrap().is_some());

        store
            .end_run(first, RunStatus::Blocked, "needs a decision", now())
            .unwrap();
        assert!(store.create_run(&issue(1), now()).unwrap().is_some());
    }

    #[test]
    fn finds_an_issues_active_run_with_its_claim_details() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(store.active_run("octocat/hello", 1).unwrap(), None);
        let comments = r#"[{"author":"octocat","body":"Integers only."}]"#;
        let new = NewRun {
            comments,
            require_approval: true,
            ..issue(1)
        };
        let id = store.create_run(&new, now()).unwrap().unwrap();
        store.set_approval_comment(id, 991, now()).unwrap();
        let run = store.active_run("octocat/hello", 1).unwrap().unwrap();
        assert_eq!(run.id, id);
        assert_eq!(run.issue_comments, comments);
        assert!(run.require_approval);
        assert_eq!(run.approval_comment_id, Some(991));
        assert_eq!(store.active_run("octocat/hello", 2).unwrap(), None);
        store
            .end_run(id, RunStatus::Canceled, "issue changed", now())
            .unwrap();
        assert_eq!(store.active_run("octocat/hello", 1).unwrap(), None);
    }

    #[test]
    fn a_stage_boundary_updates_attempt_and_run_together() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = store.create_run(&issue(1), now()).unwrap().unwrap();
        store.set_base(run_id, &"a".repeat(40), now()).unwrap();

        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        progress.begin(&workflow);
        let attempt = store
            .start_attempt(run_id, "plan", 1, Some(&"a".repeat(40)), now())
            .unwrap();
        store.set_container(attempt, "c0ffee").unwrap();
        assert_eq!(store.run(run_id).unwrap().status, RunStatus::Running);

        let plan = StageId::new("plan").unwrap();
        progress.complete(&workflow, &plan, "ready", None);
        let output = serde_json::json!({ "outcome": "ready" });
        store
            .finish_attempt(
                run_id,
                attempt,
                &AttemptEnd {
                    outcome: Some("ready"),
                    structured_output: Some(&output),
                    ..AttemptEnd::default()
                },
                &progress,
                None,
                Advance::Next { stage: "approve" },
                now(),
            )
            .unwrap();

        let run = store.run(run_id).unwrap();
        assert_eq!(run.next_stage.as_deref(), Some("approve"));
        assert_eq!(run.progress, progress);
        assert_eq!(run.checkpoint_sha, Some("a".repeat(40)));
        let attempts = store.attempts(run_id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "finished");
        assert_eq!(attempts[0].outcome.as_deref(), Some("ready"));
        assert_eq!(attempts[0].container_id.as_deref(), Some("c0ffee"));
        assert_eq!(
            attempts[0].structured_output.as_deref(),
            Some(r#"{"outcome":"ready"}"#)
        );
        assert_eq!(run.publish_url, "https://github.com/octocat/hello.git");
    }

    #[test]
    fn suspending_and_ending_set_the_run_state() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = store.create_run(&issue(1), now()).unwrap().unwrap();
        let attempt = store
            .start_attempt(run_id, "implement", 1, None, now())
            .unwrap();
        let resume: Timestamp = "2026-10-19T02:02:00Z".parse().unwrap();
        store
            .finish_attempt(
                run_id,
                attempt,
                &AttemptEnd {
                    failure: Some("usage_limit"),
                    ..AttemptEnd::default()
                },
                &Progress::default(),
                Some(&"b".repeat(40)),
                Advance::Suspend {
                    reason: SuspendReason::UsageLimit,
                    resume_after: Some(resume),
                },
                now(),
            )
            .unwrap();
        let run = store.run(run_id).unwrap();
        assert_eq!(run.status, RunStatus::Suspended);
        assert_eq!(run.suspend_reason, Some(SuspendReason::UsageLimit));
        assert_eq!(run.resume_after, Some(resume));
        assert_eq!(run.checkpoint_sha, Some("b".repeat(40)));

        let attempt = store
            .start_attempt(run_id, "implement", 1, None, now())
            .unwrap();
        assert_eq!(store.run(run_id).unwrap().suspend_reason, None);
        let ended = store.finish_attempt(
            run_id,
            attempt,
            &AttemptEnd::default(),
            &Progress::default(),
            None,
            Advance::End {
                status: RunStatus::Running,
                reason: "x",
            },
            now(),
        );
        assert!(ended.is_err(), "running is not a final status");
    }

    #[test]
    fn a_run_can_be_suspended_between_stages() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = store.create_run(&issue(1), now()).unwrap().unwrap();
        let workflow = Workflow::builtin().unwrap();
        let mut progress = Progress::default();
        progress.begin(&workflow);
        let resume: Timestamp = "2026-10-22T09:02:00Z".parse().unwrap();
        store
            .suspend_before(
                run_id,
                "plan",
                &progress,
                SuspendReason::UsageLimit,
                Some(resume),
                now(),
            )
            .unwrap();
        let run = store.run(run_id).unwrap();
        assert_eq!(run.status, RunStatus::Suspended);
        assert_eq!(run.suspend_reason, Some(SuspendReason::UsageLimit));
        assert_eq!(run.resume_after, Some(resume));
        assert_eq!(run.next_stage.as_deref(), Some("plan"));
        assert_eq!(run.progress.visits(&StageId::new("plan").unwrap()), 1);
        assert_eq!(store.attempts(run_id).unwrap(), []);
    }

    #[test]
    fn startup_interrupts_unfinished_attempts() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = store.create_run(&issue(1), now()).unwrap().unwrap();
        let attempt = store
            .start_attempt(run_id, "verify", 2, None, now())
            .unwrap();
        store.set_container(attempt, "dead").unwrap();

        let interrupted = store.interrupt_unfinished(None, now()).unwrap();
        assert_eq!(
            interrupted,
            [Interrupted {
                run_id,
                attempt_id: attempt,
                stage: "verify".into(),
                container_id: Some("dead".into()),
            }]
        );
        let run = store.run(run_id).unwrap();
        assert_eq!(run.status, RunStatus::Suspended);
        assert_eq!(run.suspend_reason, Some(SuspendReason::Interrupted));
        assert_eq!(store.attempts(run_id).unwrap()[0].status, "interrupted");
        assert_eq!(
            store.interrupt_unfinished(None, now()).unwrap(),
            Vec::<Interrupted>::new()
        );
    }

    #[test]
    fn interrupting_one_run_leaves_the_others_running() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_run(&issue(1), now()).unwrap().unwrap();
        let second = store.create_run(&issue(2), now()).unwrap().unwrap();
        store.start_attempt(first, "plan", 1, None, now()).unwrap();
        store.start_attempt(second, "plan", 1, None, now()).unwrap();

        let interrupted = store.interrupt_unfinished(Some(first), now()).unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].run_id, first);
        assert_eq!(store.run(first).unwrap().status, RunStatus::Suspended);
        assert_eq!(store.run(second).unwrap().status, RunStatus::Running);
        assert_eq!(store.attempts(second).unwrap()[0].status, "running");
    }

    #[test]
    fn lists_unfinished_and_recently_finished_runs() {
        let mut store = Store::open_in_memory().unwrap();
        let old = store.create_run(&issue(1), now()).unwrap().unwrap();
        let earlier: Timestamp = "2026-10-10T00:00:00Z".parse().unwrap();
        store
            .end_run(old, RunStatus::Published, "published", earlier)
            .unwrap();
        let recent = store.create_run(&issue(1), now()).unwrap().unwrap();
        store
            .end_run(recent, RunStatus::Blocked, "needs a decision", now())
            .unwrap();
        let open = store.create_run(&issue(2), now()).unwrap().unwrap();

        let ids = |runs: Vec<Run>| runs.into_iter().map(|run| run.id).collect::<Vec<_>>();
        assert_eq!(ids(store.unfinished_runs().unwrap()), [open]);
        let since: Timestamp = "2026-10-17T00:00:00Z".parse().unwrap();
        assert_eq!(ids(store.finished_since(since).unwrap()), [recent]);
        assert_eq!(ids(store.finished_since(earlier).unwrap()), [recent, old]);
        let latest = store.latest_run("octocat/hello", 1).unwrap().unwrap();
        assert_eq!((latest.id, latest.updated_at), (recent, now()));
        assert_eq!(store.latest_run("octocat/hello", 3).unwrap(), None);
    }

    #[test]
    fn events_and_kv() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = store.create_run(&issue(1), now()).unwrap().unwrap();
        store
            .event(Some(run_id), "claimed", "claimed #1", now())
            .unwrap();
        assert_eq!(
            store.events(run_id).unwrap(),
            [("claimed".to_owned(), "claimed #1".to_owned())]
        );
        assert_eq!(store.kv_get("paused").unwrap(), None);
        store.kv_set("paused", "true").unwrap();
        store.kv_set("paused", "false").unwrap();
        assert_eq!(store.kv_get("paused").unwrap().as_deref(), Some("false"));
        store.kv_delete("paused").unwrap();
        store.kv_delete("paused").unwrap();
        assert_eq!(store.kv_get("paused").unwrap(), None);
    }

    #[test]
    fn opens_a_file_database_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dsf.sqlite");
        let run_id = Store::open(&path)
            .unwrap()
            .create_run(&issue(1), now())
            .unwrap()
            .unwrap();
        assert_eq!(Store::open(&path).unwrap().run(run_id).unwrap().issue, 1);
    }
}
