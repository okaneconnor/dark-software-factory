//! `dsf doctor`: checks a deployment can do its job before it's trusted
//! with a night's queue. Each check says what it found and, when something
//! is wrong, what to do about it. It changes nothing on GitHub or in
//! Docker, and never prints a secret.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use dsf_core::config::{Config, RepoSlug};
use dsf_core::labels;

use crate::github::GitHub;
use crate::packs::Pack;
use crate::sandbox::Sandbox;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Worth knowing, not a problem.
    Note,
    /// dsf works, but something is off.
    Warn,
    /// dsf can't do its job until this is fixed.
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub subject: String,
    pub detail: String,
}

impl Check {
    pub fn new(level: Level, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level,
            subject: subject.into(),
            detail: detail.into(),
        }
    }
}

pub fn failures(checks: &[Check]) -> usize {
    checks
        .iter()
        .filter(|check| check.level == Level::Fail)
        .count()
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let level = match check.level {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        let _ = writeln!(out, "{level:<5} {}: {}", check.subject, check.detail);
    }
    let warnings = checks.iter().filter(|c| c.level == Level::Warn).count();
    let _ = match (failures(checks), warnings) {
        (0, 0) => writeln!(out, "\nall checks passed"),
        (0, n) => writeln!(out, "\nno failures, {n} warning(s)"),
        (n, _) => writeln!(
            out,
            "\n{n} check(s) failed; dsf can't work until they're fixed"
        ),
    };
    out
}

/// A secret file dsf reads: there, not empty, and readable only by its
/// owner. `optional` says what happens without it, for files dsf can do
/// without.
pub fn secret_file(subject: &str, path: &Path, optional: Option<&str>) -> Check {
    let shown = path.display();
    let Ok(metadata) = std::fs::metadata(path) else {
        return match optional {
            Some(without) => Check::new(Level::Note, subject, format!("none, so {without}")),
            None => Check::new(Level::Fail, subject, format!("{shown} is missing")),
        };
    };
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.is_file() {
        Check::new(Level::Fail, subject, format!("{shown} is not a file"))
    } else if metadata.len() == 0 {
        Check::new(Level::Fail, subject, format!("{shown} is empty"))
    } else if mode & 0o077 != 0 {
        Check::new(
            Level::Fail,
            subject,
            format!("{shown} can be read by other users (mode {mode:o}); chmod 600 it"),
        )
    } else {
        Check::new(Level::Ok, subject, shown.to_string())
    }
}

/// The digest webhook file, if there is one: private, and an HTTPS URL.
pub fn webhook_file(path: &Path) -> Check {
    let subject = "digest webhook";
    let check = secret_file(
        subject,
        path,
        Some("the digest is printed to the journal instead of posted"),
    );
    if check.level != Level::Ok {
        return check;
    }
    match std::fs::read_to_string(path)
        .map_err(anyhow::Error::from)
        .and_then(|text| crate::digest::webhook_url(&text))
    {
        Ok(url) => Check::new(
            Level::Ok,
            subject,
            format!(
                "{}; the digest is posted to {}",
                path.display(),
                crate::digest::Channel::of(&url).name()
            ),
        ),
        Err(error) => Check::new(Level::Fail, subject, format!("{}: {error}", path.display())),
    }
}

/// The config file parsed: a summary of what it sets up.
pub fn config(file: &Path, config: &Config) -> Check {
    let repos = config.repos.len();
    let workers = config.workers;
    Check::new(
        Level::Ok,
        "config",
        format!(
            "{}: {repos} {}, {workers} {}, weekly usage cap {}%",
            file.display(),
            if repos == 1 { "repo" } else { "repos" },
            if workers == 1 { "worker" } else { "workers" },
            config.weekly_usage_cap,
        ),
    )
}

/// Where runs live: there, writable, and its database usable.
pub fn state_dir(dir: &Path) -> Check {
    let subject = "state directory";
    let shown = dir.display();
    if !dir.is_dir() {
        return Check::new(
            Level::Fail,
            subject,
            format!("{shown} doesn't exist; create it, owned by the user dsf runs as"),
        );
    }
    let probe = dir.join(format!(".dsf-doctor-{}", std::process::id()));
    let writable = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    if !writable {
        return Check::new(
            Level::Fail,
            subject,
            format!("{shown} isn't writable by this user"),
        );
    }
    if let Err(error) = Store::open(&dir.join("dsf.sqlite")) {
        return Check::new(Level::Fail, subject, format!("{shown}: {error:#}"));
    }
    let mode = std::fs::metadata(dir).map_or(0, |m| m.permissions().mode() & 0o777);
    if mode & 0o077 != 0 {
        return Check::new(
            Level::Warn,
            subject,
            format!("{shown} can be read by other users (mode {mode:o}); chmod 700 it"),
        );
    }
    Check::new(Level::Ok, subject, format!("{shown}, database ready"))
}

/// Free space where runs live. Below `min_gb`, ticks hold work back; below
/// twice that, it's worth knowing before a night's runs fill it.
pub fn disk(dir: &Path, min_gb: u32) -> Check {
    let subject = "disk space";
    match crate::rundir::free_space(dir) {
        Ok(free) => disk_check(free, min_gb),
        Err(error) => Check::new(Level::Fail, subject, format!("{error:#}")),
    }
}

fn disk_check(free: u64, min_gb: u32) -> Check {
    let subject = "disk space";
    let min = u64::from(min_gb) * 1_000_000_000;
    let shown = crate::rundir::size_text(free);
    if free < min {
        Check::new(
            Level::Fail,
            subject,
            format!("{shown} free, below min_free_disk_gb ({min_gb} GB), so no run will start"),
        )
    } else if free < min.saturating_mul(2) {
        Check::new(
            Level::Warn,
            subject,
            format!("{shown} free, close to min_free_disk_gb ({min_gb} GB)"),
        )
    } else {
        Check::new(Level::Ok, subject, format!("{shown} free"))
    }
}

/// Each repo's standards packs: there, and usable as Claude Code plugins.
pub fn packs(config: &Config) -> Vec<Check> {
    let Some(dir) = &config.packs_dir else {
        return Vec::new();
    };
    config
        .repos
        .iter()
        .filter(|repo| !repo.packs.is_empty())
        .map(|repo| {
            let subject = format!("{} packs", repo.slug);
            let mut found = Vec::new();
            let mut problems = Vec::new();
            for name in &repo.packs {
                let pack = Pack::named(dir, name);
                match pack.check() {
                    Ok(()) => found.push(describe(&pack)),
                    Err(error) => problems.push(format!("{error:#}")),
                }
            }
            if problems.is_empty() {
                Check::new(Level::Ok, subject, found.join(", "))
            } else {
                Check::new(Level::Fail, subject, problems.join("; "))
            }
        })
        .collect()
}

/// e.g. `terraform-azure (rules, 2 skills, hooks)`.
fn describe(pack: &Pack) -> String {
    let skills = std::fs::read_dir(pack.dir.join("skills")).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|e| e.path().join("SKILL.md").is_file())
            .count()
    });
    let mut parts = Vec::new();
    if pack.dir.join(crate::packs::RULES_FILE).is_file() {
        parts.push("rules".to_owned());
    }
    match skills {
        0 => {}
        1 => parts.push("1 skill".to_owned()),
        n => parts.push(format!("{n} skills")),
    }
    if pack.dir.join("hooks/hooks.json").is_file() {
        parts.push("hooks".to_owned());
    }
    if parts.is_empty() {
        pack.name.clone()
    } else {
        format!("{} ({})", pack.name, parts.join(", "))
    }
}

/// The GitHub App, and what it can see of each configured repo.
pub async fn github(github: &GitHub, config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    match github.login().await {
        Ok(login) => checks.push(Check::new(
            Level::Ok,
            "GitHub App",
            format!("acting as {login}"),
        )),
        Err(error) => {
            checks.push(Check::new(
                Level::Fail,
                "GitHub App",
                format!("{error:#}; check github.app_id and the key"),
            ));
            return checks;
        }
    }
    for repo in &config.repos {
        checks.push(repo_check(github, &repo.slug).await);
    }
    checks
}

async fn repo_check(github: &GitHub, repo: &RepoSlug) -> Check {
    let subject = repo.to_string();
    let branch = match github.repository(repo).await {
        Ok(found) => found.default_branch,
        Err(error) => return Check::new(Level::Fail, subject, format!("{error:#}")),
    };
    let missing: Vec<&str> = match github.labels(repo).await {
        Ok(existing) => labels::ALL
            .iter()
            .map(|label| label.name)
            .filter(|name| {
                !existing
                    .iter()
                    .any(|label| label.name.eq_ignore_ascii_case(name))
            })
            .collect(),
        Err(error) => return Check::new(Level::Fail, subject, format!("{error:#}")),
    };
    match github.has_file(repo, ".factory/verify.sh", &branch).await {
        Ok(true) => {}
        Ok(false) => {
            return Check::new(
                Level::Fail,
                subject,
                format!("no .factory/verify.sh on {branch}, so every run would block; add one"),
            );
        }
        Err(error) => return Check::new(Level::Fail, subject, format!("{error:#}")),
    }
    if missing.is_empty() {
        Check::new(
            Level::Ok,
            subject,
            format!("app installed, labels in place, .factory/verify.sh on {branch}"),
        )
    } else {
        Check::new(
            Level::Warn,
            subject.clone(),
            format!(
                "missing labels {}; run `dsf labels init {subject}`",
                missing.join(", ")
            ),
        )
    }
}

/// Docker, and the images dsf runs. `images` pairs what each is for with
/// its reference.
pub async fn docker(sandbox: &Sandbox, images: &[(&str, &str)]) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Err(error) = sandbox.ping().await {
        checks.push(Check::new(Level::Fail, "Docker", format!("{error:#}")));
        return checks;
    }
    let version = sandbox
        .version()
        .await
        .unwrap_or_else(|_| "unknown".to_owned());
    checks.push(Check::new(Level::Ok, "Docker", format!("engine {version}")));
    for (role, image) in images {
        let subject = format!("{role} image");
        checks.push(match sandbox.has_image(image).await {
            Ok(true) => Check::new(Level::Ok, subject, *image),
            Ok(false) => Check::new(
                Level::Fail,
                subject,
                format!("{image} isn't on this host; pull or load it (dsf never pulls)"),
            ),
            Err(error) => Check::new(Level::Fail, subject, format!("{error:#}")),
        });
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn secret_files_must_be_there_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let level = |path: &Path| secret_file("token", path, None).level;
        assert_eq!(level(&dir.path().join("absent")), Level::Fail);
        assert_eq!(level(&write(dir.path(), "empty", "", 0o600)), Level::Fail);
        let shared = secret_file("token", &write(dir.path(), "shared", "x", 0o644), None);
        assert_eq!(shared.level, Level::Fail);
        assert!(
            shared.detail.ends_with("(mode 644); chmod 600 it"),
            "{shared:?}"
        );
        assert_eq!(level(&write(dir.path(), "good", "x", 0o600)), Level::Ok);
        // Read-only for the owner, as systemd hands credentials over.
        assert_eq!(level(&write(dir.path(), "cred", "x", 0o400)), Level::Ok);
    }

    #[test]
    fn the_webhook_is_optional_but_must_be_https() {
        let dir = tempfile::tempdir().unwrap();
        let absent = webhook_file(&dir.path().join("digest-webhook-url"));
        assert_eq!(absent.level, Level::Note);
        let plain = webhook_file(&write(
            dir.path(),
            "plain",
            "http://example.invalid/x?sig=s3cret",
            0o600,
        ));
        assert_eq!(plain.level, Level::Fail);
        assert!(!plain.detail.contains("s3cret"), "{plain:?}");
        let good = webhook_file(&write(
            dir.path(),
            "good",
            "https://example.invalid/x?sig=s3cret\n",
            0o600,
        ));
        assert_eq!(good.level, Level::Ok);
        assert!(!good.detail.contains("s3cret"), "{good:?}");
    }

    #[test]
    fn the_state_directory_must_exist_and_take_a_database() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(state_dir(&dir.path().join("absent")).level, Level::Fail);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let ready = state_dir(dir.path());
        assert_eq!(ready.level, Level::Ok, "{ready:?}");
        assert!(dir.path().join("dsf.sqlite").is_file());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(state_dir(dir.path()).level, Level::Warn);
    }

    #[test]
    fn low_disk_fails_and_nearly_low_warns() {
        assert_eq!(disk_check(4_000_000_000, 5).level, Level::Fail);
        assert_eq!(disk_check(9_000_000_000, 5).level, Level::Warn);
        let fine = disk_check(52_000_000_000, 5);
        assert_eq!(fine.level, Level::Ok);
        assert_eq!(fine.detail, "52 GB free");
        let dir = tempfile::tempdir().unwrap();
        assert_ne!(disk(dir.path(), 1).detail, "");
    }

    #[test]
    fn packs_are_checked_and_described() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("terraform-azure");
        std::fs::create_dir_all(pack.join(".claude-plugin")).unwrap();
        std::fs::write(pack.join(".claude-plugin/plugin.json"), "{}").unwrap();
        std::fs::write(pack.join("rules.md"), "x").unwrap();
        for skill in ["naming", "testing"] {
            std::fs::create_dir_all(pack.join("skills").join(skill)).unwrap();
            std::fs::write(pack.join("skills").join(skill).join("SKILL.md"), "x").unwrap();
        }
        std::fs::create_dir_all(pack.join("hooks")).unwrap();
        std::fs::write(pack.join("hooks/hooks.json"), "{}").unwrap();
        let config = Config::parse(&format!(
            r#"
            timezone = "Europe/London"
            [packs]
            dir = "{}"
            [github]
            allowed_users = ["octocat"]
            [[repos]]
            name = "octo/good"
            packs = ["terraform-azure"]
            [[repos]]
            name = "octo/bad"
            packs = ["missing"]
            [[repos]]
            name = "octo/none"
            [images]
            worker = "w"
            "#,
            dir.path().display()
        ))
        .unwrap();
        let checks = packs(&config);
        assert_eq!(checks.len(), 2);
        assert_eq!(
            (checks[0].level, checks[0].detail.as_str()),
            (Level::Ok, "terraform-azure (rules, 2 skills, hooks)")
        );
        assert_eq!(checks[1].level, Level::Fail);
        assert!(
            checks[1].detail.contains("doesn't exist"),
            "{:?}",
            checks[1]
        );
    }

    #[test]
    fn the_report_ends_with_a_verdict() {
        let ok = Check::new(Level::Ok, "Docker", "engine 27.3.1");
        let warn = Check::new(Level::Warn, "octo/calc", "missing labels factory:ready");
        let fail = Check::new(Level::Fail, "Claude token", "/x is missing");
        assert_eq!(
            render(&[ok.clone(), warn.clone(), fail]),
            "ok    Docker: engine 27.3.1\n\
             warn  octo/calc: missing labels factory:ready\n\
             FAIL  Claude token: /x is missing\n\
             \n1 check(s) failed; dsf can't work until they're fixed\n"
        );
        assert!(render(&[ok.clone(), warn]).ends_with("\nno failures, 1 warning(s)\n"));
        assert!(render(&[ok]).ends_with("\nall checks passed\n"));
    }
}
