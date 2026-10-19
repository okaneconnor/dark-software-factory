// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn dsf() -> Command {
    Command::cargo_bin("dsf").unwrap()
}

fn repo_file(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// Writes `contents` to a temporary file named `name`.
fn temp_file(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

fn stdout(command: &mut Command) -> String {
    let output = command.assert().success().get_output().clone();
    String::from_utf8(output.stdout).unwrap()
}

fn stderr(command: &mut Command) -> String {
    let output = command.assert().failure().get_output().clone();
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn version_flag_prints_the_crate_version() {
    dsf()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("dsf {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn workflow_default_prints_the_builtin_workflow() {
    dsf()
        .args(["workflow", "default"])
        .assert()
        .success()
        .stdout(dsf_core::workflow::DEFAULT_TOML);
}

#[test]
fn workflow_validate_accepts_the_builtin_workflow() {
    let stdout = stdout(
        dsf()
            .args(["workflow", "validate"])
            .arg(repo_file("defaults/workflow.toml")),
    );
    assert!(
        stdout.contains("valid, 6 stages, at most 13 stage visits"),
        "{stdout}"
    );
}

#[test]
fn workflow_validate_lists_every_problem() {
    let (_dir, path) = temp_file(
        "workflow.toml",
        r#"
version = 1
start = "implement"

[stages.implement]
kind = "agent"
mode = "write"
prompt = "../../etc/passwd"
on = { success = "verify" }

[stages.verify]
kind = "command"
run = "make test"
on = { success = "@publish", failure = "implement" }
"#,
    );
    let stderr = stderr(dsf().args(["workflow", "validate"]).arg(&path));
    insta::assert_snapshot!(stderr.replace(&path.display().to_string(), "workflow.toml"));
}

#[test]
fn workflow_validate_reports_caps_from_a_config() {
    let config = std::fs::read_to_string(repo_file("examples/config.toml"))
        .unwrap()
        .replace("max_visits = 5", "max_visits = 2");
    let (_dir, config) = temp_file("config.toml", &config);
    let stdout = stdout(
        dsf()
            .args(["workflow", "validate", "--config"])
            .arg(&config)
            .arg(repo_file("defaults/workflow.toml")),
    );
    assert!(
        stdout.contains("note: stage `verify`: max_visits 5 lowered to 2"),
        "{stdout}"
    );
    assert!(stdout.contains("at most 9 stage visits"), "{stdout}");
}

#[test]
fn config_validate_accepts_the_example() {
    let stdout = stdout(
        dsf()
            .args(["config", "validate"])
            .arg(repo_file("examples/config.toml")),
    );
    assert!(
        stdout.contains("valid, 1 repo, 1 worker, window 23:00-07:00 Europe/London"),
        "{stdout}"
    );
}

#[test]
fn config_validate_lists_every_problem() {
    let (_dir, path) = temp_file(
        "config.toml",
        r#"
timezone = "Europe/London"
workers = 4
[github]
allowed_users = []
[images]
worker = "dsf-worker:local"
"#,
    );
    let stderr = stderr(dsf().args(["config", "validate"]).arg(&path));
    insta::assert_snapshot!(stderr.replace(&path.display().to_string(), "config.toml"));
}

#[test]
fn config_validate_rejects_unknown_time_zones() {
    let config = std::fs::read_to_string(repo_file("examples/config.toml"))
        .unwrap()
        .replace("Europe/London", "Europe/Atlantis");
    let (_dir, path) = temp_file("config.toml", &config);
    let stderr = stderr(dsf().args(["config", "validate"]).arg(&path));
    assert!(
        stderr.contains("unknown time zone `Europe/Atlantis`"),
        "{stderr}"
    );
}

#[test]
fn missing_files_name_the_path() {
    let stderr = stderr(dsf().args(["workflow", "validate", "/nonexistent/workflow.toml"]));
    assert!(
        stderr.starts_with("error: reading /nonexistent/workflow.toml"),
        "{stderr}"
    );
}

#[test]
fn run_takes_an_issue_as_owner_repo_number() {
    for (args, expected) in [
        (&["run", "octo"][..], "`octo` is not owner/repo#N"),
        (&["run", "octo/calc#0"], "`0` is not an issue number"),
        (&["run", "octo/calc#x"], "`x` is not an issue number"),
        (&["run", "not a repo#1"], "`not a repo` is not owner/name"),
        (
            &["run", "octo/calc#1", "--local", "."],
            "cannot be used with",
        ),
        (&["run"], "required"),
    ] {
        let stderr = stderr(dsf().args(args));
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}

/// A config for octo/calc and a GitHub App key, as an operator has them.
fn github_setup(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
            timezone = "Europe/London"
            state_dir = "{}"
            [github]
            allowed_users = ["alice"]
            app_id = 1
            [[repos]]
            name = "octo/calc"
            [images]
            worker = "dsf-worker:dev"
            "#,
            dir.join("state").display()
        ),
    )
    .unwrap();
    let key = dir.join("app.pem");
    std::fs::write(&key, support::app_key_pem()).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    (config, key)
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_init_creates_the_labels_as_the_app() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 7 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({ "token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z" }),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/calc/labels"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
        .expect(8)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (config, key) = github_setup(dir.path());
    let api = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        stdout(
            dsf()
                .args(["labels", "init", "OCTO/calc", "--config"])
                .arg(&config)
                .arg("--github-key")
                .arg(&key)
                .args(["--github-api", &api]),
        )
    })
    .await
    .unwrap();
    assert!(
        output.starts_with("octo/calc:\n  created   factory:draft\n"),
        "{output}"
    );
    assert_eq!(output.lines().count(), 9);
}

#[test]
fn labels_init_only_touches_configured_repos() {
    let dir = tempfile::tempdir().unwrap();
    let (config, key) = github_setup(dir.path());
    let stderr = stderr(
        dsf()
            .args(["labels", "init", "octo/other", "--config"])
            .arg(&config)
            .arg("--github-key")
            .arg(&key),
    );
    assert!(
        stderr.contains("octo/other is not one of the configured repos"),
        "{stderr}"
    );
}

#[test]
fn status_pause_and_unpause_need_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = github_setup(dir.path());
    let status = || stdout(dsf().arg("status").arg("--config").arg(&config));
    let idle = status();
    assert!(
        idle.contains("Claiming: allowed\nLast tick: never\n"),
        "{idle}"
    );
    assert!(idle.contains("\nReady (0)\n"), "{idle}");

    let paused = stdout(
        dsf()
            .args(["pause", "upgrading the VM", "--config"])
            .arg(&config),
    );
    assert!(paused.starts_with("paused:"), "{paused}");
    let shown = status();
    assert!(
        shown.contains("Claiming: paused (upgrading the VM); `dsf unpause` restarts it"),
        "{shown}"
    );

    stdout(dsf().arg("unpause").arg("--config").arg(&config));
    assert!(status().contains("Claiming: allowed"));
}

#[test]
fn a_tick_does_nothing_while_another_dsf_holds_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let (config, key) = github_setup(dir.path());
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let _held = dsf::rundir::StateLock::acquire(&state).unwrap();
    let output = stdout(
        dsf()
            .arg("tick")
            .arg("--config")
            .arg(&config)
            .arg("--github-key")
            .arg(&key),
    );
    assert!(output.starts_with("another dsf is working in"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tick_outside_the_window_records_the_queue_for_status() {
    if !support::enabled() {
        return;
    }
    let fake = support::github::Fake::start().await;
    let issue = support::github::issue_json(3, "alice", &["factory:ready"], "x");
    fake.listing("factory:ready", json!([issue])).await;
    fake.listing("factory:running", json!([])).await;
    let dir = tempfile::tempdir().unwrap();
    let (config, key) = github_setup(dir.path());
    let api = fake.server.uri();
    let tick_config = config.clone();
    let output = tokio::task::spawn_blocking(move || {
        stdout(
            dsf()
                .arg("tick")
                .arg("--config")
                .arg(&tick_config)
                .arg("--github-key")
                .arg(&key)
                .args(["--github-api", &api, "--egress", "none"])
                .args(["--now", "2026-10-19T11:00:00Z"]),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        output,
        "outside the window, which opens at Mon 19 Oct 23:00\n"
    );
    let status = stdout(dsf().arg("status").arg("--config").arg(&config));
    assert!(
        status.contains("\nReady (1)\n  octo/calc#3  add() subtracts\n"),
        "{status}"
    );
    assert_eq!(fake.writes().await, Vec::<String>::new());
}

#[test]
fn digest_prints_without_a_webhook_and_records_when_it_went_out() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = github_setup(dir.path());
    let digest = |dry_run: bool| {
        let mut command = dsf();
        command
            .arg("digest")
            .arg("--config")
            .arg(&config)
            .env("HOME", dir.path())
            .env_remove("CREDENTIALS_DIRECTORY");
        if dry_run {
            command.arg("--dry-run");
        }
        stdout(&mut command)
    };
    let preview = digest(true);
    assert!(
        preview.starts_with("dsf: nothing finished\nSince "),
        "{preview}"
    );
    assert!(
        preview.contains("Warning: dsf has never ticked; is dsf-tick.timer enabled?"),
        "{preview}"
    );
    let state = dir.path().join("state/dsf.sqlite");
    let recorded = || {
        dsf::store::Store::open(&state)
            .unwrap()
            .kv_get(dsf::digest::LAST_DIGEST)
            .unwrap()
    };
    assert_eq!(recorded(), None, "a dry run records nothing");

    assert_eq!(digest(false), preview);
    assert!(recorded().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_each_problem_with_its_fix() {
    let server = MockServer::start().await;
    let ok = |body| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ok(json!({ "slug": "dsf-test" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc/installation"))
        .respond_with(ok(json!({ "id": 7 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({ "token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z" }),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc"))
        .respond_with(ok(json!({ "default_branch": "main" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc/labels"))
        .respond_with(ok(json!([{ "name": "factory:ready" }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/calc/contents/.factory/verify.sh"))
        .respond_with(ok(json!({ "type": "file" })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (config, key) = github_setup(dir.path());
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = dsf()
        .arg("doctor")
        .arg("--config")
        .arg(&config)
        .arg("--github-key")
        .arg(&key)
        .arg("--github-api")
        .arg(server.uri())
        .env("HOME", dir.path())
        .env_remove("CREDENTIALS_DIRECTORY")
        .assert()
        .failure()
        .get_output()
        .clone();
    let report = String::from_utf8(output.stdout).unwrap();
    let token = dir.path().join(".config/dsf/claude-oauth-token");
    for line in [
        "ok    config: ".to_owned(),
        "ok    state directory: ".to_owned(),
        format!("FAIL  Claude token: {} is missing", token.display()),
        "ok    GitHub App key: ".to_owned(),
        "note  digest webhook: none, so the digest is printed to the journal".to_owned(),
        "ok    GitHub App: acting as dsf-test[bot]".to_owned(),
        "warn  octo/calc: missing labels factory:draft, factory:running".to_owned(),
    ] {
        assert!(report.contains(&line), "{line:?} not in:\n{report}");
    }
    assert!(
        report.contains("run `dsf labels init octo/calc`"),
        "{report}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("check(s) failed"), "{stderr}");
}
