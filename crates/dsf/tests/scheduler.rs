//! `dsf tick` scenarios against a fake GitHub, with the clock set by each
//! test: the window, pausing and backoff, putting stale labels right,
//! plans that expire, and (with Docker, `DSF_DOCKER_TESTS=1`) working
//! through the queue, carrying on after a usage limit, stopping at the
//! weekly usage cap, and carrying on after a crash.

// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use dsf::scheduler::{BACKOFF_UNTIL, LAST_TICK, PAUSED, Scheduler, TickRecord};
use dsf::store::{Advance, AttemptEnd, NewRun, RunStatus, Store, SuspendReason};
use dsf_core::routing::Progress;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use support::enabled;
use support::github::{Fake, PR_URL, World, issue_json, labelled};

/// Inside the default 23:00-07:00 window: 23:30 BST.
const NIGHT: &str = "2026-10-19T22:30:00Z";
const ALWAYS_OPEN: &str = r#"window = { start = "00:00", end = "00:00" }"#;
const FIX: &str = "`add(2, 3)` returns -1.\n\nfake-implement: fix-calc";

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn london() -> TimeZone {
    TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").unwrap()
}

fn scheduler(world: &World, fake: &Fake, extra: &str, model: &str, now: Timestamp) -> Scheduler {
    let factory = world
        .factory_with(fake, world.config_with("label", extra), model)
        .with_clock(Arc::new(move || now));
    Scheduler::new(factory, london())
}

/// A trusted issue: alice wrote it and labelled it ready.
async fn ready_issue(fake: &Fake, number: u64, body: &str) -> Value {
    let issue = issue_json(number, "alice", &["factory:ready"], body);
    fake.issue(
        issue.clone(),
        json!([labelled("alice", "factory:ready", "2026-10-18T09:00:00Z")]),
        json!([]),
    )
    .await;
    issue
}

fn seed_run(store: &mut Store, issue: u64) -> i64 {
    store
        .create_run(
            &NewRun {
                repo: "octo/calc",
                issue,
                title: "add() subtracts",
                body: FIX,
                author: "alice",
                updated_at: None,
                workflow: dsf_core::workflow::DEFAULT_TOML,
                clone_url: "",
                publish_url: "",
                comments: "[]",
                require_approval: true,
            },
            at("2026-10-18T09:30:00Z"),
        )
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn outside_the_window_the_queue_is_seen_but_nothing_is_claimed() {
    let world = World::new("window");
    let fake = Fake::start().await;
    let first = ready_issue(&fake, 1, FIX).await;
    let second = ready_issue(&fake, 2, FIX).await;
    // GitHub lists the newest first.
    fake.listing("factory:ready", json!([second, first])).await;
    fake.listing("factory:running", json!([])).await;
    fake.writable().await;

    // Noon BST.
    let record = scheduler(&world, &fake, "", "fake", at("2026-10-19T11:00:00Z"))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes,
        ["outside the window, which opens at Mon 19 Oct 23:00"]
    );
    let queued: Vec<u64> = record.queue.iter().map(|issue| issue.number).collect();
    assert_eq!(queued, [1, 2]);
    assert_eq!(fake.writes().await, Vec::<String>::new());
    let store = world.store();
    assert_eq!(store.unfinished_runs().unwrap(), Vec::new());
    let saved: TickRecord =
        serde_json::from_str(&store.kv_get(LAST_TICK).unwrap().unwrap()).unwrap();
    assert_eq!(saved, record);
}

#[tokio::test]
async fn a_pause_or_a_backoff_stops_claiming_until_it_is_lifted() {
    let world = World::new("pause");
    let fake = Fake::start().await;
    // Untrusted, so a claim attempt shows up as a skip without Docker.
    let issue = issue_json(1, "mallory", &["factory:ready"], FIX);
    fake.issue(issue.clone(), json!([]), json!([])).await;
    fake.listing("factory:ready", json!([issue])).await;
    fake.listing("factory:running", json!([])).await;

    let mut store = world.store();
    store.kv_set(PAUSED, "maintenance").unwrap();
    let tick = |now: &str| scheduler(&world, &fake, "", "fake", at(now));
    assert_eq!(
        tick(NIGHT).tick().await.unwrap().notes,
        ["claiming is paused (maintenance); `dsf unpause` restarts it"]
    );

    store.kv_delete(PAUSED).unwrap();
    store.kv_set(BACKOFF_UNTIL, "2026-10-20T01:02:00Z").unwrap();
    assert_eq!(
        tick(NIGHT).tick().await.unwrap().notes,
        ["backing off after a usage limit until Tue 20 Oct 02:02"]
    );

    assert_eq!(
        tick("2026-10-20T01:03:00Z").tick().await.unwrap().notes,
        ["octo/calc#1: its author, mallory, isn't in github.allowed_users"]
    );
    assert_eq!(fake.writes().await, Vec::<String>::new());
}

#[tokio::test]
async fn over_the_usage_cap_claiming_waits_unless_the_cap_is_raised() {
    let world = World::new("capped");
    let fake = Fake::start().await;
    // Untrusted, so a claim attempt shows up as a skip without Docker.
    let issue = issue_json(1, "mallory", &["factory:ready"], FIX);
    fake.issue(issue.clone(), json!([]), json!([])).await;
    fake.listing("factory:ready", json!([issue])).await;
    fake.listing("factory:running", json!([])).await;
    let reading = json!({
        "seen_at": "2026-10-19T22:10:00Z",
        "usage": {
            "status": "allowed",
            "five_hour": null,
            "seven_day": {"utilization": 0.85, "resets_at": "2026-10-22T09:00:00Z"}
        }
    });
    world
        .store()
        .kv_set(dsf::usage::USAGE, &reading.to_string())
        .unwrap();

    let tick = |extra: &str, now: &str| scheduler(&world, &fake, extra, "fake", at(now));
    assert_eq!(
        tick("", NIGHT).tick().await.unwrap().notes,
        [
            "the weekly Claude allowance is 85% used, over the 80% cap, \
             so nothing starts until Thu 22 Oct 10:02"
        ]
    );
    let skipped = ["octo/calc#1: its author, mallory, isn't in github.allowed_users"];
    assert_eq!(
        tick("weekly_usage_cap = 90", NIGHT)
            .tick()
            .await
            .unwrap()
            .notes,
        skipped
    );
    // After the weekly reset the old reading stops nothing.
    assert_eq!(
        tick("", "2026-10-22T22:30:00Z").tick().await.unwrap().notes,
        skipped
    );
    assert_eq!(fake.writes().await, Vec::<String>::new());
}

#[tokio::test]
async fn issues_left_labelled_running_are_put_right() {
    let world = World::new("repair");
    let fake = Fake::start().await;
    // #4 has no run at all. #5's run opened PR #9, but dsf stopped before
    // it could move the issue on.
    let orphan = issue_json(4, "alice", &["factory:running"], "x");
    let finished = issue_json(5, "alice", &["factory:running"], "x");
    fake.issue(orphan.clone(), json!([]), json!([])).await;
    fake.issue(finished.clone(), json!([]), json!([])).await;
    fake.listing("factory:running", json!([orphan, finished]))
        .await;
    fake.listing("factory:ready", json!([])).await;
    fake.writable().await;
    let mut store = world.store();
    let run = seed_run(&mut store, 5);
    store.set_pr(run, 9, at(NIGHT)).unwrap();
    store
        .end_run(run, RunStatus::Published, "published", at(NIGHT))
        .unwrap();

    let record = scheduler(&world, &fake, "", "fake", at(NIGHT))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes,
        [
            "octo/calc#4: labelled factory:running with no run, so moved to factory:blocked",
            "octo/calc#5: labels brought up to date with how its last run ended",
        ]
    );
    let requests = fake.requests().await;
    let posted = |path: &str| -> Vec<Value> {
        requests
            .iter()
            .filter(|(method, p, _)| method == "POST" && p == path)
            .map(|(_, _, body)| body.clone())
            .collect()
    };
    assert_eq!(
        posted("/repos/octo/calc/issues/4/labels"),
        [json!({ "labels": ["factory:blocked"] })]
    );
    assert_eq!(
        posted("/repos/octo/calc/issues/5/labels"),
        [json!({ "labels": ["factory:review"] })]
    );
    let comment = &posted("/repos/octo/calc/issues/5/comments")[0];
    assert!(
        comment["body"]
            .as_str()
            .unwrap()
            .contains("https://github.com/octo/calc/pull/9"),
        "{comment}"
    );
}

#[tokio::test]
async fn a_plan_no_one_approves_expires_even_outside_the_window() {
    let world = World::new("expiry");
    let fake = Fake::start().await;
    fake.issue(
        issue_json(1, "alice", &["factory:awaiting-approval"], FIX),
        json!([labelled("alice", "factory:ready", "2026-10-18T09:00:00Z")]),
        json!([]),
    )
    .await;
    fake.listing("factory:ready", json!([])).await;
    fake.listing("factory:running", json!([])).await;
    // The plan comment, #99, was posted at 2026-10-18T10:00:00Z.
    fake.writable().await;
    let mut store = world.store();
    let run = seed_run(&mut store, 1);
    let attempt = store
        .start_attempt(run, "approve", 1, None, at("2026-10-18T09:59:00Z"))
        .unwrap();
    store
        .finish_attempt(
            run,
            attempt,
            &AttemptEnd::default(),
            &Progress::default(),
            None,
            Advance::Suspend {
                reason: SuspendReason::Approval,
                resume_after: None,
            },
            at("2026-10-18T09:59:00Z"),
        )
        .unwrap();
    store
        .set_approval_comment(run, 99, at("2026-10-18T10:00:00Z"))
        .unwrap();

    // Three days less a minute: still waiting, and the window is shut.
    let record = scheduler(&world, &fake, "", "fake", at("2026-10-21T09:59:00Z"))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes,
        [
            "outside the window, which opens at Wed 21 Oct 23:00",
            "octo/calc#1: run 1 is waiting for plan approval",
        ]
    );
    assert_eq!(fake.writes().await, Vec::<String>::new());

    let record = scheduler(&world, &fake, "", "fake", at("2026-10-21T10:00:00Z"))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes[1],
        "octo/calc#1: run 1 blocked: no one approved the plan within 3d"
    );
    let run = world.store().run(run).unwrap();
    assert_eq!(run.status, RunStatus::Blocked);
    assert_eq!(
        fake.writes().await,
        [
            "DELETE /repos/octo/calc/issues/1/labels/factory:awaiting-approval",
            "POST /repos/octo/calc/issues/1/labels",
            "POST /repos/octo/calc/issues/1/comments",
        ]
    );
}

#[tokio::test]
async fn a_tick_works_through_the_queue_oldest_first() {
    if !enabled() {
        return;
    }
    let world = World::new("queue");
    let fake = Fake::start().await;
    let first = ready_issue(&fake, 1, FIX).await;
    let second = ready_issue(&fake, 2, FIX).await;
    fake.listing("factory:ready", json!([second, first])).await;
    fake.listing("factory:running", json!([])).await;
    fake.writable().await;

    let record = scheduler(&world, &fake, "", "fake", at(NIGHT))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes,
        [
            format!("octo/calc#1: run 1 opened {PR_URL}"),
            format!("octo/calc#2: run 2 opened {PR_URL}"),
        ]
    );
    for issue in [1, 2] {
        let calc = world.published_for(issue, "calc.py").unwrap();
        assert!(calc.contains("a + b"), "{calc}");
    }
    let store = world.store();
    assert_eq!(store.run(1).unwrap().issue, 1);
    assert_eq!(store.run(2).unwrap().issue, 2);
}

#[tokio::test]
async fn after_a_usage_limit_claiming_waits_then_the_run_carries_on() {
    if !enabled() {
        return;
    }
    let world = World::new("limit");
    let fake = Fake::start().await;
    let body = "`add(2, 3)` returns -1.\n\nfake-implement: limited";
    let issue = ready_issue(&fake, 1, body).await;
    fake.listing("factory:ready", json!([issue])).await;
    fake.listing("factory:running", json!([])).await;
    fake.writable().await;
    // The fake limit resets in 2100, so dsf waits its maximum of 8 days.
    let now = Timestamp::now();
    let later = |hours: i64| now.checked_add(SignedDuration::from_hours(hours)).unwrap();

    let record = scheduler(&world, &fake, ALWAYS_OPEN, "limited", now)
        .tick()
        .await
        .unwrap();
    assert!(
        record.notes[0].starts_with("octo/calc#1: run 1 paused until"),
        "{:?}",
        record.notes
    );
    let run = world.store().run(1).unwrap();
    assert_eq!(run.suspend_reason, Some(SuspendReason::UsageLimit));
    let backoff: Timestamp = world
        .store()
        .kv_get(BACKOFF_UNTIL)
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(Some(backoff), run.resume_after);
    assert!(backoff > later(8 * 24 - 1), "{backoff}");
    let writes = fake.writes().await.len();

    let record = scheduler(&world, &fake, ALWAYS_OPEN, "fake", later(1))
        .tick()
        .await
        .unwrap();
    assert!(
        record.notes[0].starts_with("backing off after a usage limit until"),
        "{:?}",
        record.notes
    );
    assert_eq!(fake.writes().await.len(), writes);

    let record = scheduler(&world, &fake, ALWAYS_OPEN, "fake", later(9 * 24))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes.last().unwrap(),
        &format!("octo/calc#1: run 1 opened {PR_URL}")
    );
    assert!(world.published("calc.py").unwrap().contains("a + b"));
    let stages: Vec<String> = world
        .store()
        .attempts(1)
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.stage)
        .collect();
    assert_eq!(
        stages,
        [
            "plan",
            "approve",
            "implement",
            "implement",
            "verify",
            "review"
        ]
    );
}

#[tokio::test]
async fn over_the_usage_cap_runs_pause_until_the_weekly_reset() {
    if !enabled() {
        return;
    }
    let world = World::new("cap");
    let fake = Fake::start().await;
    let first = ready_issue(&fake, 1, FIX).await;
    let second = ready_issue(&fake, 2, FIX).await;
    fake.listing("factory:ready", json!([first, second])).await;
    fake.listing("factory:running", json!([])).await;
    fake.writable().await;
    let now = Timestamp::now();
    // Whole seconds, as Claude reports it.
    let reset = Timestamp::from_second(now.as_second() + 3 * 24 * 60 * 60).unwrap();
    // Every agent stage reports 85% of the weekly allowance used.
    let model = format!("weekly:85:{}", reset.as_second());

    let record = scheduler(&world, &fake, ALWAYS_OPEN, &model, now)
        .tick()
        .await
        .unwrap();
    let why = "the weekly Claude allowance is 85% used, over the 80% cap";
    assert_eq!(record.notes.len(), 2, "{:?}", record.notes);
    assert_eq!(
        record.notes[0],
        format!("octo/calc#1: run 1 paused: {why} until it resets at {reset}")
    );
    assert!(
        record.notes[1].starts_with(&format!("stopped claiming: {why}, so nothing starts until")),
        "{:?}",
        record.notes
    );
    // Planning reported the usage; implement never started.
    // The cap holds the run, not a fixed time, so raising it takes effect.
    let run = world.store().run(1).unwrap();
    assert_eq!(run.suspend_reason, Some(SuspendReason::UsageLimit));
    assert_eq!(run.resume_after, None);
    assert_eq!(world.store().kv_get(BACKOFF_UNTIL).unwrap(), None);
    assert_eq!(run.next_stage.as_deref(), Some("implement"));
    let stages: Vec<String> = world
        .store()
        .attempts(1)
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.stage)
        .collect();
    assert_eq!(stages, ["plan", "approve"]);
    assert_eq!(world.store().active_run("octo/calc", 2).unwrap(), None);
    let comments: Vec<String> = fake
        .requests()
        .await
        .into_iter()
        .filter(|(method, path, _)| method == "POST" && path.ends_with("/issues/1/comments"))
        .map(|(_, _, body)| body["body"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        comments.iter().any(|comment| comment
            == &format!(
                "dsf paused this (run 1): {why} until it resets at {reset}. \
                 It's back in the queue and continues from where it stopped."
            )),
        "{comments:?}"
    );

    // A day later it's still over the cap, so nothing changes.
    let writes = fake.writes().await.len();
    let tomorrow = now.checked_add(SignedDuration::from_hours(24)).unwrap();
    let record = scheduler(&world, &fake, ALWAYS_OPEN, "fake", tomorrow)
        .tick()
        .await
        .unwrap();
    assert!(
        record.notes[0].starts_with(&format!("{why}, so nothing starts until")),
        "{:?}",
        record.notes
    );
    assert_eq!(fake.writes().await.len(), writes);

    // After the weekly reset the paused run carries on from implement.
    // Only issue 1 is queued now, to keep the test short.
    fake.server.reset().await;
    fake.mount_app().await;
    fake.writable().await;
    let first = ready_issue(&fake, 1, FIX).await;
    fake.listing("factory:ready", json!([first])).await;
    fake.listing("factory:running", json!([])).await;
    let after = reset.checked_add(SignedDuration::from_mins(3)).unwrap();
    let record = scheduler(&world, &fake, ALWAYS_OPEN, "fake", after)
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes.last().unwrap(),
        &format!("octo/calc#1: run 1 opened {PR_URL}"),
        "{:?}",
        record.notes
    );
    assert!(world.published("calc.py").unwrap().contains("a + b"));
    let stages: Vec<String> = world
        .store()
        .attempts(1)
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.stage)
        .collect();
    assert_eq!(stages, ["plan", "approve", "implement", "verify", "review"]);
}

#[tokio::test]
async fn a_run_interrupted_mid_stage_carries_on_at_the_next_tick() {
    if !enabled() {
        return;
    }
    let world = World::new("crash");
    let fake = Fake::start().await;
    let body = "`add(2, 3)` returns -1.\n\nfake-implement: slow-fix-calc";
    let issue = ready_issue(&fake, 1, body).await;
    fake.listing("factory:ready", json!([issue])).await;
    fake.listing("factory:running", json!([])).await;
    fake.writable().await;

    // dsf stops while implement is running: its agent sleeps for two
    // minutes, and the tick is dropped long before that.
    let first = scheduler(&world, &fake, ALWAYS_OPEN, "fake", at(NIGHT));
    let stopped = tokio::time::timeout(Duration::from_secs(25), first.tick()).await;
    assert!(stopped.is_err(), "the tick should still be running");
    drop(first);
    // The queue was saved before the work began, so `dsf status` could
    // show it while the tick ran.
    let saved: TickRecord =
        serde_json::from_str(&world.store().kv_get(LAST_TICK).unwrap().unwrap()).unwrap();
    assert_eq!(
        saved
            .queue
            .iter()
            .map(|issue| issue.number)
            .collect::<Vec<_>>(),
        [1]
    );
    let attempts = world.store().attempts(1).unwrap();
    let last = attempts.last().unwrap();
    assert_eq!(
        (last.stage.as_str(), last.status.as_str()),
        ("implement", "running")
    );

    // GitHub now shows the issue as claimed.
    fake.server.reset().await;
    fake.mount_app().await;
    fake.writable().await;
    let claimed = issue_json(1, "alice", &["factory:running"], body);
    fake.issue(
        claimed.clone(),
        json!([labelled("alice", "factory:ready", "2026-10-18T09:00:00Z")]),
        json!([]),
    )
    .await;
    fake.listing("factory:ready", json!([])).await;
    fake.listing("factory:running", json!([claimed])).await;

    let record = scheduler(&world, &fake, ALWAYS_OPEN, "fast", at(NIGHT))
        .tick()
        .await
        .unwrap();
    assert_eq!(
        record.notes,
        [
            "run 1 was interrupted; it continues from its last checkpoint".to_owned(),
            format!("octo/calc#1: run 1 opened {PR_URL}"),
        ]
    );
    assert!(world.published("calc.py").unwrap().contains("a + b"));
    let events = world.store().events(1).unwrap();
    assert!(
        events.iter().any(|(kind, message)| kind == "interrupted"
            && message.contains("removed 1 leftover container(s)")),
        "{events:?}"
    );
    let statuses: Vec<String> = world
        .store()
        .attempts(1)
        .unwrap()
        .into_iter()
        .map(|attempt| format!("{} {}", attempt.stage, attempt.status))
        .collect();
    assert!(
        statuses.contains(&"implement interrupted".to_owned()),
        "{statuses:?}"
    );
}
