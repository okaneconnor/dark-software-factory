//! A fake GitHub (wiremock) with the App installed on octo/calc, and a
//! world for it to act on: a bare repository standing in for
//! github.com/octo/calc, and a state directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dsf::engine::{Config as EngineConfig, Egress, Engine};
use dsf::factory::{Factory, GitHubForge};
use dsf::github::GitHub;
use dsf::github::app::AppKey;
use dsf::store::Store;
use dsf_core::config::{Config, RepoSlug};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{FAKE_IMAGE, TOKEN, app_key_pem, git};

pub const INSTALLATION_TOKEN: &str = "ghs_installationtokenfortestsonly0123456789";
pub const PR_URL: &str = "https://github.com/octo/calc/pull/5";

pub fn repo() -> RepoSlug {
    "octo/calc".parse().unwrap()
}

pub fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

pub fn created(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(201).set_body_json(body)
}

pub fn issue_json(number: u64, author: &str, labels: &[&str], body: &str) -> Value {
    json!({
        "number": number,
        "title": "add() subtracts",
        "body": body,
        "state": "open",
        "user": { "login": author },
        "labels": labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
    })
}

pub fn labelled(actor: &str, label: &str, at: &str) -> Value {
    json!({ "event": "labeled", "actor": { "login": actor }, "label": { "name": label }, "created_at": at })
}

/// A fake GitHub with the App installed on octo/calc.
pub struct Fake {
    pub server: MockServer,
}

impl Fake {
    pub async fn start() -> Self {
        let fake = Self {
            server: MockServer::start().await,
        };
        fake.mount_app().await;
        fake
    }

    pub async fn mount_app(&self) {
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(ok(json!({ "slug": "dsf-test" })))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/calc/installation"))
            .respond_with(ok(json!({ "id": 7 })))
            .mount(&self.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/installations/7/access_tokens"))
            .respond_with(created(json!({
                "token": INSTALLATION_TOKEN,
                "expires_at": "2099-01-01T00:00:00Z",
            })))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/calc"))
            .respond_with(ok(json!({ "default_branch": "main" })))
            .mount(&self.server)
            .await;
    }

    pub async fn issue(&self, issue: Value, events: Value, comments: Value) {
        let number = issue["number"].as_u64().unwrap();
        Mock::given(method("GET"))
            .and(path(format!("/repos/octo/calc/issues/{number}")))
            .respond_with(ok(issue))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/octo/calc/issues/{number}/events")))
            .respond_with(ok(events))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/octo/calc/issues/{number}/comments")))
            .respond_with(ok(comments))
            .mount(&self.server)
            .await;
    }

    /// What listing the open issues labelled `label` returns.
    pub async fn listing(&self, label: &str, issues: Value) {
        Mock::given(method("GET"))
            .and(path("/repos/octo/calc/issues"))
            .and(query_param("labels", label))
            .respond_with(ok(issues))
            .mount(&self.server)
            .await;
    }

    /// Label edits and comments succeed; pull requests open as #5.
    pub async fn writable(&self) {
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/repos/octo/calc/issues/\d+/labels/.+$"))
            .respond_with(ok(json!([])))
            .mount(&self.server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/repos/octo/calc/issues/\d+/labels$"))
            .respond_with(ok(json!([])))
            .mount(&self.server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/repos/octo/calc/issues/\d+/comments$"))
            .respond_with(created(json!({
                "id": 99,
                "user": { "login": "dsf-test[bot]" },
                "body": "posted",
                "created_at": "2026-10-18T10:00:00Z",
            })))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/calc/issues/comments/99"))
            .respond_with(ok(json!({
                "id": 99,
                "user": { "login": "dsf-test[bot]" },
                "body": "posted",
                "created_at": "2026-10-18T10:00:00Z",
            })))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/calc/pulls"))
            .respond_with(ok(json!([])))
            .mount(&self.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/octo/calc/pulls"))
            .respond_with(created(json!({ "number": 5, "html_url": PR_URL })))
            .mount(&self.server)
            .await;
    }

    /// Starts over with a new view of issue #1: what a later poll sees.
    pub async fn remount(&self, labels: &[&str], body: &str, events: Value) {
        self.server.reset().await;
        self.mount_app().await;
        self.writable().await;
        self.issue(issue_json(1, "alice", labels, body), events, json!([]))
            .await;
    }

    pub fn github(&self) -> Arc<GitHub> {
        let key = AppKey::from_pem(1, app_key_pem()).unwrap();
        Arc::new(GitHub::new(key, &self.server.uri()).unwrap())
    }

    /// Every request that changed something, as `METHOD path`.
    pub async fn writes(&self) -> Vec<String> {
        self.requests()
            .await
            .into_iter()
            .filter(|(method, path, _)| method != "GET" && !path.ends_with("/access_tokens"))
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect()
    }

    pub async fn requests(&self) -> Vec<(String, String, Value)> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|request| {
                let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                (
                    request.method.to_string(),
                    request.url.path().to_owned(),
                    body,
                )
            })
            .collect()
    }
}

/// A bare repository standing in for github.com/octo/calc, and a state
/// directory.
pub struct World {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub state: PathBuf,
    pub instance: String,
}

impl World {
    pub fn new(name: &str) -> Self {
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
        let bare = root.join("remotes/octo/calc.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--quiet", "--bare"]);
        git(
            &origin,
            &["push", "--quiet", bare.to_str().unwrap(), "main"],
        );
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        Self {
            _dir: dir,
            root,
            state,
            instance: format!("dsftest-gh-{name}-{}", std::process::id()),
        }
    }

    /// A config for octo/calc with `approval`, plus any top-level `extra`
    /// settings such as a window.
    pub fn config_with(&self, approval: &str, extra: &str) -> Config {
        Config::parse(&format!(
            r#"
            timezone = "Europe/London"
            state_dir = "{}"
            {extra}
            [github]
            allowed_users = ["alice"]
            app_id = 1
            [[repos]]
            name = "octo/calc"
            approval = "{approval}"
            [images]
            worker = "{FAKE_IMAGE}"
            "#,
            self.state.display()
        ))
        .unwrap()
    }

    pub fn config(&self, approval: &str) -> Config {
        self.config_with(approval, "")
    }

    pub fn factory(&self, fake: &Fake, approval: &str) -> Factory {
        self.factory_with(fake, self.config(approval), "fake")
    }

    /// A factory whose fake agent runs as `model`, which some of its
    /// actions read: `limited` hits a usage limit, `fast` skips sleeps.
    pub fn factory_with(&self, fake: &Fake, config: Config, model: &str) -> Factory {
        let engine_config = EngineConfig {
            instance: self.instance.clone(),
            state_dir: self.state.clone(),
            worker_image: FAKE_IMAGE.to_owned(),
            model: model.to_owned(),
            max_turns: 10,
            agent_timeout: Duration::from_secs(180),
            limits: dsf::sandbox::Limits {
                memory_bytes: 256 * 1024 * 1024,
                nano_cpus: 1_000_000_000,
                pids: 128,
            },
            egress: Egress::None,
            caps: dsf_core::clamp::Caps::default(),
            extra_protected: Vec::new(),
            claude_token: TOKEN.to_owned().into(),
        };
        let github = fake.github();
        let engine = Engine::new(
            engine_config,
            Store::open(&self.state.join("dsf.sqlite")).unwrap(),
            dsf::sandbox::Sandbox::connect().unwrap(),
            dsf::hostgit::Git::locate().unwrap(),
            Box::new(|line| eprintln!("{line}")),
        )
        .unwrap()
        .with_forge(Box::new(GitHubForge::new(github.clone())));
        let git_base = format!("{}/", self.root.join("remotes").display());
        Factory::new(engine, github, config, &git_base)
    }

    pub fn store(&self) -> Store {
        Store::open(&self.state.join("dsf.sqlite")).unwrap()
    }

    pub fn prompt(&self, run_id: i64, stage: &str) -> String {
        let attempt = self
            .store()
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

    /// A file on the branch dsf published for `issue`, if there is one.
    pub fn published_for(&self, issue: u64, path: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .current_dir(self.root.join("remotes/octo/calc.git"))
            .args(["show", &format!("factory/issue-{issue}:{path}")])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap())
    }

    pub fn published(&self, path: &str) -> Option<String> {
        self.published_for(1, path)
    }
}

/// Every file under `dir`, as text, for checking nothing leaked into it.
pub fn all_text(dir: &Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            text.push_str(&all_text(&entry.path()));
        } else if kind.is_file() {
            text.push_str(&String::from_utf8_lossy(
                &std::fs::read(entry.path()).unwrap(),
            ));
        }
    }
    text
}
