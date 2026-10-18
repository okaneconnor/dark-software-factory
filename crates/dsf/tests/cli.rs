// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use assert_cmd::Command;

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
