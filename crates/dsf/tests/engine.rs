//! The engine end to end, in real Docker, with a fake agent: the workflow
//! loop, publishing, and the hostile-worktree suite. The fake reads its
//! instructions from `fake-<stage>:` lines in the issue (see
//! tests/fake-agent/claude). Needs `DSF_DOCKER_TESTS=1`.

// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

mod support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use dsf::engine::{Config, Egress, Engine, Issue, ResumeOptions, RunEnd};
use dsf::packs::Pack;
use dsf::store::{Store, SuspendReason};
use dsf_core::clamp::Caps;
use support::{FAKE_IMAGE, TOKEN, enabled, git};

/// A repository with a bug and a gate that catches it, a bare remote to
/// publish to, and a state directory.
struct World {
    _dir: tempfile::TempDir,
    origin: PathBuf,
    remote: PathBuf,
    state: PathBuf,
    instance: String,
}

impl World {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let origin = root.join("origin");
        std::fs::create_dir_all(origin.join(".factory")).unwrap();
        std::fs::write(origin.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        let gate = origin.join(".factory/verify.sh");
        std::fs::write(
            &gate,
            "#!/bin/sh\nif grep -q 'return a + b' calc.py; then echo 'ok: add works'; \
             else echo 'FAILED test_add: expected 5, got -1'; exit 1; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&origin, &["init", "--quiet"]);
        git(&origin, &["add", "--all"]);
        git(
            &origin,
            &["commit", "--quiet", "-m", "calculator with a bug"],
        );
        let remote = root.join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--bare"]);
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        let instance = format!("dsftest-{name}-{}", std::process::id());
        Self {
            _dir: dir,
            origin,
            remote,
            state,
            instance,
        }
    }

    fn engine(&self, model: &str) -> Engine {
        self.engine_with_image(model, FAKE_IMAGE)
    }

    fn engine_with_image(&self, model: &str, image: &str) -> Engine {
        let config = Config {
            instance: self.instance.clone(),
            state_dir: self.state.clone(),
            worker_image: image.to_owned(),
            model: model.to_owned(),
            max_turns: 10,
            agent_timeout: Duration::from_secs(120),
            limits: dsf::sandbox::Limits {
                memory_bytes: 256 * 1024 * 1024,
                nano_cpus: 1_000_000_000,
                pids: 128,
            },
            egress: Egress::None,
            caps: Caps::default(),
            extra_protected: Vec::new(),
            weekly_usage_cap: dsf_core::config::DEFAULT_WEEKLY_USAGE_CAP,
            claude_token: TOKEN.to_owned().into(),
        };
        let store = Store::open(&self.state.join("dsf.sqlite")).unwrap();
        let sandbox = dsf::sandbox::Sandbox::connect().unwrap();
        Engine::new(
            config,
            store,
            sandbox,
            dsf::hostgit::Git::locate().unwrap(),
            Box::new(|line| eprintln!("{line}")),
        )
        .unwrap()
    }

    fn issue(&self, directives: &str) -> Issue {
        Issue {
            repo: "local/calc".to_owned(),
            number: 1,
            title: "add() subtracts".to_owned(),
            body: format!("`add(2, 3)` returns -1; it should return 5.\n\n{directives}\n"),
            author: "octocat".to_owned(),
            clone_url: self.origin.display().to_string(),
            publish_url: format!("file://{}", self.remote.display()),
            comments: Vec::new(),
            require_approval: false,
        }
    }

    fn stages(&self, run_id: i64) -> Vec<String> {
        let store = Store::open(&self.state.join("dsf.sqlite")).unwrap();
        store
            .attempts(run_id)
            .unwrap()
            .into_iter()
            .map(|a| a.stage)
            .collect()
    }

    fn published(&self, path: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .current_dir(&self.remote)
            .args(["show", &format!("factory/issue-1:{path}")])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap())
    }

    fn prompt(&self, run_id: i64, stage: &str) -> String {
        let store = Store::open(&self.state.join("dsf.sqlite")).unwrap();
        let attempt = store
            .attempts(run_id)
            .unwrap()
            .into_iter()
            .rfind(|a| a.stage == stage)
            .unwrap();
        std::fs::read_to_string(
            self.state
                .join(format!("runs/{run_id}/attempts/{}/prompt.md", attempt.id)),
        )
        .unwrap()
    }
}

fn blocked_reason(end: RunEnd) -> String {
    match end {
        RunEnd::Blocked { reason, .. } => reason,
        other => panic!("expected the run to be blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn a_fixed_bug_is_verified_reviewed_and_published() {
    if !enabled() {
        return;
    }
    let world = World::new("happy");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: fix-calc"))
        .await
        .unwrap();
    let RunEnd::Published { run_id, branch, .. } = end else {
        panic!("{end:?}")
    };
    assert_eq!(branch, "factory/issue-1");
    assert_eq!(
        world.stages(run_id),
        ["plan", "approve", "implement", "verify", "review"]
    );
    assert_eq!(
        world.published("calc.py").unwrap(),
        "def add(a, b):\n    return a + b\n"
    );
    assert!(
        world
            .prompt(run_id, "implement")
            .contains("Make add() add."),
        "the plan reaches implement"
    );
    // The branch holds the work, so the clone is gone; the logs stay.
    let dir = world.state.join(format!("runs/{run_id}"));
    assert!(!dir.join("work").exists() && !dir.join("repo.git").exists());
    assert!(dir.join("attempts").is_dir());
}

/// When the host fails under a stage, the run is suspended to try again,
/// not blocked: the issue did nothing wrong.
#[tokio::test]
async fn a_fault_under_a_stage_suspends_the_run_to_retry() {
    if !enabled() {
        return;
    }
    let world = World::new("fault");
    let issue = world.issue("fake-implement: fix-calc");
    let end = world
        .engine_with_image("fake", "dsf-test-no-such-image:missing")
        .start(&issue)
        .await
        .unwrap();
    let RunEnd::Faulted {
        run_id,
        detail,
        retry_after,
    } = end
    else {
        panic!("{end:?}")
    };
    assert!(detail.starts_with("plan could not run"), "{detail}");
    assert!(retry_after > jiff::Timestamp::now());

    let end = world
        .engine("fake")
        .resume(run_id, ResumeOptions::default())
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
    assert_eq!(
        world.stages(run_id),
        ["plan", "plan", "approve", "implement", "verify", "review"]
    );
}

/// A run whose setup failed starts again from a fresh clone.
#[tokio::test]
async fn a_fault_during_setup_sets_the_run_up_again() {
    if !enabled() {
        return;
    }
    let world = World::new("setupfault");
    let readable = std::fs::metadata(&world.origin).unwrap().permissions();
    std::fs::set_permissions(&world.origin, std::fs::Permissions::from_mode(0o000)).unwrap();
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: fix-calc"))
        .await;
    std::fs::set_permissions(&world.origin, readable).unwrap();
    let RunEnd::Faulted { run_id, detail, .. } = end.unwrap() else {
        panic!("expected a fault")
    };
    assert!(detail.starts_with("setting up the run failed"), "{detail}");

    let end = world
        .engine("fake")
        .resume(run_id, ResumeOptions::default())
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
}

/// A run whose files are gone can't continue, so it is blocked rather than
/// retried every tick.
#[tokio::test]
async fn a_run_that_cant_be_picked_up_again_is_blocked() {
    if !enabled() {
        return;
    }
    let world = World::new("lost");
    let end = world
        .engine("limited")
        .start(&world.issue("fake-implement: limited"))
        .await
        .unwrap();
    let RunEnd::Suspended { run_id, .. } = end else {
        panic!("{end:?}")
    };
    // Moved rather than deleted: Docker Desktop can hold the worktree's
    // mount point for a moment after a container goes.
    let dir = world.state.join(format!("runs/{run_id}"));
    std::fs::rename(&dir, dir.with_extension("gone")).unwrap();
    let end = world
        .engine("fake")
        .resume(run_id, ResumeOptions::default())
        .await
        .unwrap();
    let reason = blocked_reason(end);
    assert!(
        reason.contains("couldn't pick the run up again"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_failing_gate_feeds_its_output_to_fix() {
    if !enabled() {
        return;
    }
    let world = World::new("fixloop");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: noop\nfake-fix: fix-calc"))
        .await
        .unwrap();
    let RunEnd::Published { run_id, .. } = end else {
        panic!("{end:?}")
    };
    assert_eq!(
        world.stages(run_id),
        [
            "plan",
            "approve",
            "implement",
            "verify",
            "fix",
            "verify",
            "review"
        ]
    );
    assert!(
        world
            .prompt(run_id, "fix")
            .contains("FAILED test_add: expected 5, got -1")
    );
}

#[tokio::test]
async fn the_same_failure_twice_blocks_instead_of_burning_visits() {
    if !enabled() {
        return;
    }
    let world = World::new("breaker");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: noop\nfake-fix: noop"))
        .await
        .unwrap();
    assert!(blocked_reason(end).contains("failed the same way twice"));
    assert_eq!(world.published("calc.py"), None);
}

#[tokio::test]
async fn requested_changes_go_back_through_fix_and_verify() {
    if !enabled() {
        return;
    }
    let world = World::new("review");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: fix-calc\nfake-review: changes_requested,approve"))
        .await
        .unwrap();
    let RunEnd::Published { run_id, .. } = end else {
        panic!("{end:?}")
    };
    assert_eq!(
        world.stages(run_id),
        [
            "plan",
            "approve",
            "implement",
            "verify",
            "review",
            "fix",
            "verify",
            "review"
        ]
    );
    assert!(
        world
            .prompt(run_id, "fix")
            .contains("Add a regression test for add()")
    );
}

#[tokio::test]
async fn an_ambiguous_issue_is_blocked_with_the_plans_questions() {
    if !enabled() {
        return;
    }
    let world = World::new("ambiguous");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-plan: needs_decision"))
        .await
        .unwrap();
    let RunEnd::Blocked { run_id, reason } = end else {
        panic!("{end:?}")
    };
    assert!(reason.contains("needs_decision"), "{reason}");
    let store = Store::open(&world.state.join("dsf.sqlite")).unwrap();
    assert!(
        store
            .run(run_id)
            .unwrap()
            .plan
            .unwrap()
            .contains("Should add() accept strings?")
    );
}

/// Each hostile action must block the run, with nothing published.
#[tokio::test]
async fn hostile_worktrees_are_refused() {
    if !enabled() {
        return;
    }
    let cases = [
        (
            "touch-factory",
            "changed protected paths: .factory/verify.sh",
        ),
        (
            "touch-github",
            "changed protected paths: .github/workflows/evil.yml",
        ),
        ("escape-symlink", "points outside the repository"),
        ("nested-repo", "nested git repository at `vendor/lib/.git`"),
        ("gitmodules", "a .gitmodules file"),
        ("leak-token", "looks like a secret"),
    ];
    for (action, expected) in cases {
        let world = World::new(action);
        let end = world
            .engine("fake")
            .start(&world.issue(&format!("fake-implement: {action}")))
            .await
            .unwrap();
        let reason = blocked_reason(end);
        assert!(reason.contains(expected), "{action}: {reason}");
        assert_eq!(
            world.published("calc.py"),
            None,
            "{action} must not publish"
        );
    }
}

/// Only the final change is published, as one commit: a secret one stage
/// wrote and a later stage removed is in no commit that leaves the host.
#[tokio::test]
async fn the_branch_is_one_commit_without_the_runs_history() {
    if !enabled() {
        return;
    }
    let world = World::new("squash");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: leak-token-only\nfake-fix: unleak-fix-calc"))
        .await
        .unwrap();
    let RunEnd::Published { run_id, sha, .. } = end else {
        panic!("{end:?}")
    };
    assert_eq!(
        world.stages(run_id),
        [
            "plan",
            "approve",
            "implement",
            "verify",
            "fix",
            "verify",
            "review"
        ]
    );
    let base = git(&world.origin, &["rev-parse", "HEAD"]);
    let pushed = git(
        &world.remote,
        &["rev-list", &format!("{}..factory/issue-1", base.trim())],
    );
    assert_eq!(pushed.trim(), sha);
    let history = git(&world.remote, &["log", "--all", "--patch"]);
    assert!(!history.contains("ghp_"), "the token reached the remote");
    assert_eq!(world.published("settings.py"), None);
}

/// The code under test can't rewrite the gate it runs under.
#[tokio::test]
async fn verify_cannot_change_its_own_gate() {
    if !enabled() {
        return;
    }
    let world = World::new("rogate");
    std::fs::write(
        world.origin.join(".factory/verify.sh"),
        "#!/bin/sh\nif echo 'exit 0' >> .factory/verify.sh; then echo 'the gate is writable'; \
         exit 1; fi\ngrep -q 'return a + b' calc.py\n",
    )
    .unwrap();
    git(
        &world.origin,
        &["commit", "--quiet", "-am", "gate tries itself"],
    );
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: fix-calc"))
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
}

#[tokio::test]
async fn agents_get_only_their_allowlisted_environment_and_cannot_commit() {
    if !enabled() {
        return;
    }
    let world = World::new("env");
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: env-names"))
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
    let names = world.published("env-names.txt").unwrap();
    let names: Vec<&str> = names.lines().collect();
    assert_eq!(
        names,
        [
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "DISABLE_AUTOUPDATER",
            "HOME",
            "HOSTNAME",
            "PATH"
        ]
    );
    assert_eq!(
        world.published("commit-result.txt").unwrap(),
        "commit exit 128\n"
    );
    let base = git(&world.origin, &["rev-parse", "HEAD"]);
    let authors = git(
        &world.remote,
        &[
            "log",
            "--format=%an",
            &format!("{}..factory/issue-1", base.trim()),
        ],
    );
    assert!(
        authors.lines().all(|author| author == "dsf"),
        "only host git commits: {authors}"
    );
}

#[tokio::test]
async fn a_usage_limit_suspends_the_run_and_resume_carries_on() {
    if !enabled() {
        return;
    }
    let world = World::new("limit");
    let end = world
        .engine("limited")
        .start(&world.issue("fake-implement: limited"))
        .await
        .unwrap();
    let RunEnd::Suspended {
        run_id,
        reason,
        resume_after,
        ..
    } = end
    else {
        panic!("{end:?}")
    };
    assert_eq!(reason, SuspendReason::UsageLimit);
    assert!(resume_after.is_some());

    let end = world
        .engine("fake")
        .resume(run_id, ResumeOptions::default())
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
    // The limited attempt didn't use up implement's single visit.
    assert_eq!(
        world.stages(run_id),
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

const WORKFLOW_WITHOUT_A_GATE: &str = r#"
version = 1
start = "implement"
[stages.implement]
kind = "agent"
mode = "write"
prompt = "implement.md.j2"
on = { success = "verify", failure = "@blocked" }
[stages.verify]
kind = "command"
run = ".factory/verify.sh"
max_visits = 50
on = { success = "@publish", failure = "@blocked" }
"#;

#[tokio::test]
async fn a_repo_can_bring_its_own_workflow() {
    if !enabled() {
        return;
    }
    let world = World::new("workflow");
    std::fs::write(
        world.origin.join(".factory/workflow.toml"),
        WORKFLOW_WITHOUT_A_GATE,
    )
    .unwrap();
    git(&world.origin, &["add", "--all"]);
    git(&world.origin, &["commit", "--quiet", "-m", "own workflow"]);
    let end = world
        .engine("fake")
        .start(&world.issue("fake-implement: fix-calc"))
        .await
        .unwrap();
    let RunEnd::Published { run_id, .. } = end else {
        panic!("{end:?}")
    };
    assert_eq!(world.stages(run_id), ["implement", "verify"]);

    // The same workflow can't run an issue that needs plan approval: it
    // would change code before anyone saw a plan.
    let world = World::new("nogate");
    std::fs::write(
        world.origin.join(".factory/workflow.toml"),
        WORKFLOW_WITHOUT_A_GATE,
    )
    .unwrap();
    git(&world.origin, &["add", "--all"]);
    git(&world.origin, &["commit", "--quiet", "-m", "own workflow"]);
    let issue = Issue {
        require_approval: true,
        ..world.issue("fake-implement: fix-calc")
    };
    let end = world.engine("fake").start(&issue).await.unwrap();
    let reason = blocked_reason(end);
    assert!(
        reason.contains("needs plan approval") && reason.contains("implement"),
        "{reason}"
    );
    assert_eq!(world.published("calc.py"), None);
}

/// The repository's instructions and the operator's packs reach the
/// agent: the system-prompt file, each pack as a plugin, the Skill tool.
#[tokio::test]
async fn agents_get_the_repos_instructions_and_the_operators_packs() {
    if !enabled() {
        return;
    }
    let world = World::new("packs");
    std::fs::write(
        world.origin.join("CLAUDE.md"),
        "Always write tests. (house-rule-7)\n",
    )
    .unwrap();
    git(&world.origin, &["add", "--all"]);
    git(&world.origin, &["commit", "--quiet", "-m", "instructions"]);
    let packs = tempfile::tempdir().unwrap();
    let pack = packs.path().canonicalize().unwrap().join("house");
    std::fs::create_dir_all(pack.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(pack.join("skills/style")).unwrap();
    std::fs::write(
        pack.join(".claude-plugin/plugin.json"),
        r#"{"name": "house"}"#,
    )
    .unwrap();
    std::fs::write(
        pack.join("rules.md"),
        "Name things clearly. (pack-rule-3)\n",
    )
    .unwrap();
    std::fs::write(
        pack.join("skills/style/SKILL.md"),
        "---\nname: style\n---\nBe clear.\n",
    )
    .unwrap();

    let end = world
        .engine("fake")
        .with_packs(HashMap::from([(
            "local/calc".to_owned(),
            vec![Pack::at(&pack).unwrap()],
        )]))
        .start(&world.issue("fake-implement: record-context"))
        .await
        .unwrap();
    assert!(matches!(end, RunEnd::Published { .. }), "{end:?}");
    let context = world.published("context.txt").unwrap();
    for expected in [
        "--tools Read,Grep,Glob,Edit,Write,Bash,Skill",
        "--append-system-prompt-file /dsf/system.md",
        "--plugin-dir /dsf/packs/house",
        "--add-dir /dsf/packs/house",
        "# Repository instructions",
        "## CLAUDE.md\n\nAlways write tests. (house-rule-7)",
        "# Standards: house\n\nName things clearly. (pack-rule-3)",
        "named `house:<skill>`",
        "--- /dsf/packs\nhouse",
    ] {
        assert!(
            context.contains(expected),
            "{expected:?} not in:\n{context}"
        );
    }
}

/// kill -9 the CLI mid-stage, then resume: the orphaned container is
/// removed before the worktree is reset, and the run completes.
#[test]
fn a_run_killed_mid_stage_resumes_from_its_checkpoint() {
    use std::io::{BufRead, BufReader};

    if !enabled() {
        return;
    }
    let world = World::new("kill");
    let issue_file = world.state.join("issue.md");
    std::fs::write(
        &issue_file,
        "# add() subtracts\n\nfake-implement: slow-fix-calc\n",
    )
    .unwrap();
    let token_file = world.state.join("token");
    std::fs::write(&token_file, TOKEN).unwrap();
    let dsf = assert_cmd::cargo::cargo_bin("dsf");
    let common = |command: &mut std::process::Command| {
        command
            .args(["--state-dir"])
            .arg(&world.state)
            .args(["--instance", &world.instance])
            .args(["--egress", "none", "--image", FAKE_IMAGE, "--token-file"])
            .arg(&token_file);
    };

    let mut first = std::process::Command::new(&dsf);
    first
        .args(["run", "--local"])
        .arg(&world.origin)
        .arg("--issue-file")
        .arg(&issue_file)
        .args([
            "--publish-to",
            &format!("file://{}", world.remote.display()),
        ]);
    common(&mut first);
    let mut child = first.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines() {
        if line.unwrap().contains("implement (visit 1)") {
            break;
        }
    }
    std::thread::sleep(Duration::from_secs(3));
    child.kill().unwrap(); // SIGKILL: no cleanup runs
    child.wait().unwrap();

    let mut resume = std::process::Command::new(&dsf);
    resume.args(["run", "--resume", "1", "--model", "fast"]);
    common(&mut resume);
    let output = resume.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("removed 1 leftover container(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("published factory/issue-1"), "{stdout}");
    assert_eq!(
        world.published("calc.py").unwrap(),
        "def add(a, b):\n    return a + b\n"
    );
    let stages = world.stages(1);
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
