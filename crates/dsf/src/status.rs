//! `dsf status`: the factory at a glance, read from the store alone. Its
//! columns are the web board's: ready, running, awaiting approval, review
//! and blocked, with recent finished runs from the last week.

use std::fmt::Write as _;

use anyhow::Result;
use dsf_core::config::Config;
use dsf_core::schedule::next_daily;
use dsf_core::workflow::StageId;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::scheduler::{BACKOFF_REASON, BACKOFF_UNTIL, LAST_TICK, PAUSED, TickRecord, local};
use crate::store::{Attempt, Run, RunStatus, Store, SuspendReason};
use crate::usage::{UsageReading, percent};

/// How far back finished runs are shown.
const RECENT: SignedDuration = SignedDuration::from_hours(7 * 24);

/// Everything `dsf status` shows, as read at one moment.
#[derive(Debug, Clone)]
pub struct Board {
    pub now: Timestamp,
    pub paused: Option<String>,
    pub backoff_until: Option<Timestamp>,
    /// What the backoff is after, e.g. `a usage limit`.
    pub backoff_reason: Option<String>,
    pub usage: Option<UsageReading>,
    pub last_tick: Option<TickRecord>,
    /// Unfinished runs, each with its latest attempt.
    pub unfinished: Vec<(Run, Option<Attempt>)>,
    /// Runs finished in the last week, most recent first.
    pub finished: Vec<Run>,
}

impl Board {
    pub fn load(store: &Store, now: Timestamp) -> Result<Self> {
        let mut unfinished = Vec::new();
        for run in store.unfinished_runs()? {
            let latest = store.attempts(run.id)?.pop();
            unfinished.push((run, latest));
        }
        Ok(Self {
            now,
            paused: store.kv_get(PAUSED)?,
            backoff_until: store
                .kv_get(BACKOFF_UNTIL)?
                .and_then(|text| text.parse().ok())
                .filter(|until| *until > now),
            backoff_reason: store.kv_get(BACKOFF_REASON)?,
            usage: UsageReading::load(store)?,
            last_tick: store
                .kv_get(LAST_TICK)?
                .and_then(|json| serde_json::from_str(&json).ok()),
            unfinished,
            finished: store.finished_since(now.checked_sub(RECENT).unwrap_or(now))?,
        })
    }

    pub fn render(&self, config: &Config, tz: &TimeZone) -> String {
        let mut out = self.header(config, tz);
        let columns = self.columns(config, tz);
        for (title, items) in [
            ("Ready", &columns.ready),
            ("Running", &columns.running),
            ("Awaiting approval", &columns.awaiting),
            ("Review, last 7 days", &columns.review),
            ("Blocked, last 7 days", &columns.blocked),
        ] {
            let _ = writeln!(out, "\n{title} ({})", items.len());
            for item in items {
                let _ = writeln!(out, "  {item}");
            }
        }
        if let Some(tick) = &self.last_tick {
            for (title, lines) in [
                ("Last tick's problems", &tick.problems),
                ("Last tick's notes", &tick.notes),
            ] {
                if !lines.is_empty() {
                    let _ = writeln!(out, "\n{title}");
                    for line in lines {
                        let _ = writeln!(out, "  {line}");
                    }
                }
            }
        }
        out
    }

    /// The time, the window, usage, whether claiming is allowed, and the
    /// last tick.
    fn header(&self, config: &Config, tz: &TimeZone) -> String {
        let mut out = format!("dsf at {} ({})\n", local(self.now, tz), config.timezone);
        let _ = writeln!(out, "Window:   {}", window_text(config, tz, self.now));
        let cap = config.weekly_usage_cap;
        let _ = writeln!(out, "Usage:    {}", self.usage_text(cap, tz));
        let _ = writeln!(out, "Claiming: {}", self.claiming_text(config, tz));
        let _ = writeln!(out, "Last tick: {}", self.last_tick_text(tz));
        out
    }

    /// Whether claiming is allowed, and if not, why and until when.
    pub fn claiming_text(&self, config: &Config, tz: &TimeZone) -> String {
        let cap = config.weekly_usage_cap;
        let capped = self.usage.as_ref().and_then(|reading| {
            let until = reading.capped_until(cap, self.now)?;
            // A longer backoff is the real wait.
            let longest = self.backoff_until.is_none_or(|backoff| backoff <= until);
            longest.then_some((reading, until))
        });
        if let Some(reason) = &self.paused {
            format!("paused ({reason}); `dsf unpause` restarts it")
        } else if let Some((reading, until)) = capped {
            format!(
                "{}, so nothing starts until {}",
                reading.over_cap(cap),
                local(until, tz)
            )
        } else if let Some(until) = self.backoff_until {
            let after = self.backoff_reason.as_deref().unwrap_or("a usage limit");
            format!("backing off after {after} until {}", local(until, tz))
        } else {
            "allowed".to_owned()
        }
    }

    pub fn last_tick_text(&self, tz: &TimeZone) -> String {
        self.last_tick
            .as_ref()
            .map_or_else(|| "never".to_owned(), |tick| local(tick.at, tz))
    }

    /// The last reading of each allowance, e.g. `weekly 62% (resets Thu
    /// 22 Oct 10:00), 5-hour 12% (resets ...); cap 80%, read Mon 19 Oct
    /// 23:20`. A window that has reset since is marked so, since its
    /// figure is out of date.
    pub fn usage_text(&self, cap: u8, tz: &TimeZone) -> String {
        let Some(reading) = &self.usage else {
            return format!("no reading yet; cap {cap}% of the weekly allowance");
        };
        let window = |name: &str, window: Option<crate::agent::Window>| {
            let window = window?;
            let used = percent(window.utilization);
            Some(match window.resets_at {
                Some(reset) if reset > self.now => {
                    format!("{name} {used}% (resets {})", local(reset, tz))
                }
                Some(_) => format!("{name} {used}% (reset since)"),
                None => format!("{name} {used}%"),
            })
        };
        let windows: Vec<String> = [
            window("weekly", reading.usage.seven_day),
            window("5-hour", reading.usage.five_hour),
        ]
        .into_iter()
        .flatten()
        .collect();
        let windows = if windows.is_empty() {
            "no allowance figures".to_owned()
        } else {
            windows.join(", ")
        };
        format!("{windows}; cap {cap}%, read {}", local(reading.seen_at, tz))
    }

    /// The board's columns, one line per card.
    pub fn columns(&self, config: &Config, tz: &TimeZone) -> Columns {
        let at = |when: Timestamp| local(when, tz);
        let mut columns = Columns::default();
        let queue = self.last_tick.as_ref().map_or(&[][..], |t| &t.queue[..]);
        for issue in queue {
            // Claimed since the tick listed it: it's under Running now.
            let working = self.unfinished.iter().any(|(run, _)| {
                run.repo == issue.repo
                    && run.issue == issue.number
                    && matches!(run.status, RunStatus::Queued | RunStatus::Running)
            });
            if working {
                continue;
            }
            let paused = self.unfinished.iter().find(|(run, _)| {
                run.repo == issue.repo
                    && run.issue == issue.number
                    && run.suspend_reason == Some(SuspendReason::UsageLimit)
            });
            let mut line = format!("{}#{}  {}", issue.repo, issue.number, issue.title);
            if let Some((run, _)) = paused {
                let _ = write!(line, "  (continues run {}", run.id);
                if let Some(after) = run.resume_after.filter(|after| *after > self.now) {
                    let _ = write!(line, " after {}", at(after));
                }
                line.push(')');
            }
            columns.ready.push(line);
        }
        for (run, latest) in &self.unfinished {
            let queued = queue
                .iter()
                .any(|issue| issue.repo == run.repo && issue.number == run.issue);
            let head = heading(run);
            match (run.status, run.suspend_reason) {
                (RunStatus::Queued | RunStatus::Running, _) => {
                    let stage = run.next_stage.as_deref().unwrap_or("setup");
                    let visit = StageId::new(stage).map_or(0, |id| run.progress.visits(&id));
                    let visit = if visit > 0 {
                        format!(", visit {visit}")
                    } else {
                        String::new()
                    };
                    columns.running.push(format!(
                        "{head}, {stage}{visit}, since {})",
                        at(run.updated_at)
                    ));
                }
                (_, Some(SuspendReason::Approval)) => {
                    let expires = SignedDuration::try_from(config.approval_expiry)
                        .ok()
                        .and_then(|wait| run.updated_at.checked_add(wait).ok());
                    let mut line = format!("{head}, plan posted {}", at(run.updated_at));
                    if let Some(expires) = expires {
                        let _ = write!(line, ", expires {}", at(expires));
                    }
                    line.push(')');
                    columns.awaiting.push(line);
                }
                (_, Some(SuspendReason::UsageLimit)) if !queued => {
                    let after = run
                        .resume_after
                        .map_or_else(String::new, |after| format!(" after {}", at(after)));
                    columns.ready.push(format!(
                        "{head}, paused by a usage limit, continues{after})"
                    ));
                }
                // Marked interrupted by recovery: dsf itself stopped.
                (_, Some(SuspendReason::Interrupted))
                    if latest.as_ref().is_some_and(|a| a.status == "interrupted") =>
                {
                    columns.running.push(format!(
                        "{head}, interrupted, carries on at the next tick in the window)"
                    ));
                }
                // Stopped for a human, e.g. a token Claude refused.
                (_, Some(SuspendReason::Interrupted)) => {
                    let why = latest
                        .as_ref()
                        .and_then(|a| a.failure.clone())
                        .unwrap_or_else(|| "paused".to_owned());
                    columns.blocked.push(format!("{head}, {why})"));
                }
                _ => {}
            }
        }
        for run in &self.finished {
            let head = heading(run);
            let when = at(run.updated_at);
            match run.status {
                RunStatus::Published => columns.review.push(match run.pr_number {
                    Some(pr) => format!("{head}, draft PR #{pr}, {when})"),
                    None => format!("{head}, pushed, {when})"),
                }),
                RunStatus::Blocked | RunStatus::PublishFailed => columns.blocked.push(format!(
                    "{head}, {when}: {})",
                    run.end_reason.as_deref().unwrap_or("no reason recorded")
                )),
                _ => {}
            }
        }
        columns
    }
}

#[derive(Debug, Default)]
pub struct Columns {
    pub ready: Vec<String>,
    pub running: Vec<String>,
    pub awaiting: Vec<String>,
    pub review: Vec<String>,
    pub blocked: Vec<String>,
}

/// The window's hours and when it next opens or closes.
pub fn window_text(config: &Config, tz: &TimeZone, now: Timestamp) -> String {
    let window = &config.window;
    let hours = format!(
        "{}-{}",
        window.start.strftime("%H:%M"),
        window.end.strftime("%H:%M")
    );
    if window.start == window.end {
        "always open".to_owned()
    } else if window.is_open(now, tz) {
        let closes = next_daily(window.end, now, tz);
        format!("{hours}, open until {}", local(closes, tz))
    } else {
        format!(
            "{hours}, closed until {}",
            local(window.next_open(now, tz), tz)
        )
    }
}

/// `owner/repo#N  title  (run N`, closed by the caller.
fn heading(run: &Run) -> String {
    format!(
        "{}#{}  {}  (run {}",
        run.repo, run.issue, run.issue_title, run.id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::QueuedIssue;
    use crate::store::{Advance, AttemptEnd, NewRun};
    use dsf_core::routing::Progress;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn new_run(issue: u64, title: &'static str) -> NewRun<'static> {
        NewRun {
            repo: "octo/calc",
            issue,
            title,
            body: "",
            author: "alice",
            updated_at: None,
            workflow: dsf_core::workflow::DEFAULT_TOML,
            clone_url: "",
            publish_url: "",
            comments: "[]",
            require_approval: false,
        }
    }

    fn suspend(store: &mut Store, run: i64, reason: SuspendReason, failure: Option<&str>) {
        let attempt = store
            .start_attempt(run, "implement", 1, None, at("2026-10-19T22:10:00Z"))
            .unwrap();
        store
            .finish_attempt(
                run,
                attempt,
                &AttemptEnd {
                    failure,
                    ..AttemptEnd::default()
                },
                &Progress::default(),
                None,
                Advance::Suspend {
                    reason,
                    resume_after: Some(at("2026-10-20T01:02:00Z")),
                },
                at("2026-10-19T22:15:00Z"),
            )
            .unwrap();
    }

    fn config() -> Config {
        Config::parse(
            r#"
            timezone = "Europe/London"
            [github]
            allowed_users = ["alice"]
            [[repos]]
            name = "octo/calc"
            [images]
            worker = "w"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn shows_every_column() {
        let mut store = Store::open_in_memory().unwrap();
        let start = at("2026-10-19T22:05:00Z");
        let running = store
            .create_run(&new_run(1, "add() subtracts"), start)
            .unwrap()
            .unwrap();
        store
            .start_attempt(running, "verify", 2, None, at("2026-10-19T22:30:00Z"))
            .unwrap();
        let limited = store
            .create_run(&new_run(2, "mean() of nothing"), start)
            .unwrap()
            .unwrap();
        suspend(&mut store, limited, SuspendReason::UsageLimit, None);
        let waiting = store
            .create_run(&new_run(3, "Rename calc"), start)
            .unwrap()
            .unwrap();
        suspend(&mut store, waiting, SuspendReason::Approval, None);
        let auth = store
            .create_run(&new_run(4, "Faster multiply"), start)
            .unwrap()
            .unwrap();
        suspend(
            &mut store,
            auth,
            SuspendReason::Interrupted,
            Some(
                "Claude authentication failed (authentication_failed); fix the token, then resume",
            ),
        );
        let published = store
            .create_run(&new_run(5, "Docs"), start)
            .unwrap()
            .unwrap();
        store.set_pr(published, 9, start).unwrap();
        store
            .end_run(
                published,
                RunStatus::Published,
                "published",
                at("2026-10-18T23:40:00Z"),
            )
            .unwrap();
        let stuck = store
            .create_run(&new_run(6, "Make it fast"), start)
            .unwrap()
            .unwrap();
        store
            .end_run(
                stuck,
                RunStatus::Blocked,
                "the plan needs a decision",
                at("2026-10-19T00:10:00Z"),
            )
            .unwrap();
        store
            .kv_set(PAUSED, "run 4: Claude authentication failed")
            .unwrap();
        reading(0.62, "2026-10-22T09:00:00Z", Some(0.12))
            .save(&mut store)
            .unwrap();
        let tick = TickRecord {
            at: at("2026-10-19T22:40:00Z"),
            queue: vec![
                QueuedIssue {
                    repo: "octo/calc".into(),
                    number: 2,
                    title: "mean() of nothing".into(),
                },
                QueuedIssue {
                    repo: "octo/calc".into(),
                    number: 7,
                    title: "Support floats".into(),
                },
            ],
            notes: vec!["octo/calc#8: it's blocked by open issue #7".into()],
            problems: vec!["the worker image w isn't on this host".into()],
        };
        store
            .kv_set(LAST_TICK, &serde_json::to_string(&tick).unwrap())
            .unwrap();

        let board = Board::load(&store, at("2026-10-19T22:41:00Z")).unwrap();
        let tz = TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").unwrap();
        insta::assert_snapshot!(board.render(&config(), &tz));
    }

    fn reading(weekly: f64, resets_at: &str, five_hour: Option<f64>) -> UsageReading {
        UsageReading {
            seen_at: at("2026-10-19T22:20:00Z"),
            usage: crate::agent::Usage {
                status: "allowed".to_owned(),
                five_hour: five_hour.map(|utilization| crate::agent::Window {
                    utilization,
                    resets_at: Some(at("2026-10-19T23:00:00Z")),
                }),
                seven_day: Some(crate::agent::Window {
                    utilization: weekly,
                    resets_at: Some(at(resets_at)),
                }),
            },
        }
    }

    #[test]
    fn over_the_usage_cap_nothing_starts_until_the_reset() {
        let mut store = Store::open_in_memory().unwrap();
        reading(0.857, "2026-10-22T09:00:00Z", None)
            .save(&mut store)
            .unwrap();
        let board = Board::load(&store, at("2026-10-19T22:41:00Z")).unwrap();
        let text = board.render(&config(), &TimeZone::UTC);
        assert!(
            text.contains(
                "Usage:    weekly 85% (resets Thu 22 Oct 09:00); cap 80%, read Mon 19 Oct 22:20\n\
                 Claiming: the weekly Claude allowance is 85% used, over the 80% cap, \
                 so nothing starts until Thu 22 Oct 09:02\n"
            ),
            "{text}"
        );

        // After the weekly reset the reading is out of date and stops nothing.
        let board = Board::load(&store, at("2026-10-22T09:30:00Z")).unwrap();
        let text = board.render(&config(), &TimeZone::UTC);
        assert!(text.contains("weekly 85% (reset since); cap 80%"), "{text}");
        assert!(text.contains("Claiming: allowed\n"), "{text}");
    }

    #[test]
    fn an_issue_being_worked_shows_only_as_running() {
        let mut store = Store::open_in_memory().unwrap();
        let start = at("2026-10-19T22:05:00Z");
        let running = store
            .create_run(&new_run(1, "add() subtracts"), start)
            .unwrap()
            .unwrap();
        store
            .start_attempt(running, "plan", 1, None, start)
            .unwrap();
        let tick = TickRecord {
            at: start,
            queue: vec![
                QueuedIssue {
                    repo: "octo/calc".into(),
                    number: 1,
                    title: "add() subtracts".into(),
                },
                QueuedIssue {
                    repo: "octo/calc".into(),
                    number: 7,
                    title: "Support floats".into(),
                },
            ],
            notes: Vec::new(),
            problems: Vec::new(),
        };
        store
            .kv_set(LAST_TICK, &serde_json::to_string(&tick).unwrap())
            .unwrap();
        let board = Board::load(&store, at("2026-10-19T22:06:00Z")).unwrap();
        let text = board.render(&config(), &TimeZone::UTC);
        assert!(
            text.contains("\nReady (1)\n  octo/calc#7  Support floats\n"),
            "{text}"
        );
        assert!(
            text.contains("\nRunning (1)\n  octo/calc#1  add() subtracts  (run 1, plan"),
            "{text}"
        );
    }

    #[test]
    fn an_idle_factory_outside_the_window() {
        let store = Store::open_in_memory().unwrap();
        let board = Board::load(&store, at("2026-10-19T12:00:00Z")).unwrap();
        let text = board.render(&config(), &TimeZone::UTC);
        assert!(
            text.contains("Window:   23:00-07:00, closed until Mon 19 Oct 23:00"),
            "{text}"
        );
        assert!(
            text.contains(
                "Usage:    no reading yet; cap 80% of the weekly allowance\n\
                 Claiming: allowed\nLast tick: never"
            ),
            "{text}"
        );
        assert!(text.contains("\nReady (0)\n\nRunning (0)\n"), "{text}");
    }
}
