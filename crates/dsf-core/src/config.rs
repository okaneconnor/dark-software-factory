//! The operator's configuration file, conventionally `/etc/dsf/config.toml`.
//!
//! Secrets never live here. The Claude token, the GitHub App's private key
//! and the digest webhook come from files or systemd credentials, so this
//! file can be world-readable and kept in version control.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use jiff::civil::Time;
use serde::Deserialize;

use crate::clamp::Caps;
use crate::duration;
use crate::issue::{Allowlist, ApprovalPolicy};
use crate::protect::{self, ProtectedPaths};
use crate::schedule::Window;

/// The agreed ceiling: more parallel agents than this stops being one
/// person's reviewable output.
pub const MAX_WORKERS: usize = 2;

/// The agreed share of the weekly Claude allowance the factory may use,
/// leaving the rest for the operator's own work.
pub const DEFAULT_WEEKLY_USAGE_CAP: u8 = 80;

/// Hosts every agent container needs, whatever the operator adds.
pub const BUILTIN_EGRESS: &[&str] = &["api.anthropic.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// An IANA time zone name. The binary resolves it; this crate can't
    /// read the tz database.
    pub timezone: String,
    pub workers: usize,
    pub window: Window,
    /// How long a plan waits for approval before the run is blocked.
    pub approval_expiry: Duration,
    /// Percent of the weekly allowance after which no agent stage starts
    /// until it resets. 100 leaves it to the real limit.
    pub weekly_usage_cap: u8,
    pub state_dir: PathBuf,
    pub allowlist: Allowlist,
    /// The GitHub App dsf acts as. Only `--local` runs work without one.
    pub github_app_id: Option<u64>,
    pub repos: Vec<RepoConfig>,
    pub caps: Caps,
    /// Every host agent containers may reach: the built-ins plus the
    /// operator's, sorted and deduplicated.
    pub egress: Vec<String>,
    pub worker_image: String,
    /// dsf's own image, which the egress proxy sidecar runs. `None` leaves
    /// it to the command line, whose default suits development.
    pub dsf_image: Option<String>,
    pub protected: ProtectedPaths,
    /// The operator's own protected paths, as written.
    pub protect_extra: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub slug: RepoSlug,
    pub approval: ApprovalPolicy,
}

/// `owner/name` on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoSlug {
    pub owner: String,
    pub name: String,
}

impl RepoSlug {
    /// GitHub compares repo names case-insensitively.
    pub fn matches(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl FromStr for RepoSlug {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (owner, name) = text
            .split_once('/')
            .ok_or_else(|| format!("`{text}` is not owner/name"))?;
        if !is_login(owner) {
            return Err(format!("`{owner}` is not a valid GitHub owner"));
        }
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !name_ok {
            return Err(format!("`{name}` is not a valid repository name"));
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("not a valid config file: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid config:{}", crate::bullets(.0))]
    Invalid(Vec<Problem>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{field}`: {message}")]
pub struct Problem {
    pub field: String,
    pub message: String,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        raw.validate().map_err(ConfigError::Invalid)
    }

    pub fn repo(&self, slug: &RepoSlug) -> Option<&RepoConfig> {
        self.repos.iter().find(|repo| repo.slug.matches(slug))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    timezone: String,
    workers: Option<usize>,
    approval_expiry: Option<String>,
    weekly_usage_cap: Option<u8>,
    state_dir: Option<PathBuf>,
    window: Option<RawWindow>,
    github: RawGithub,
    #[serde(default)]
    repos: Vec<RawRepo>,
    #[serde(default)]
    caps: RawCaps,
    #[serde(default)]
    egress: RawEgress,
    images: RawImages,
    #[serde(default)]
    protect: RawProtect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    start: String,
    end: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGithub {
    allowed_users: Vec<String>,
    app_id: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    name: String,
    #[serde(default)]
    approval: ApprovalPolicy,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaps {
    max_visits: Option<u32>,
    command_timeout: Option<String>,
    max_stages: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEgress {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImages {
    worker: String,
    dsf: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProtect {
    #[serde(default)]
    extra: Vec<String>,
}

/// Collects problems so the operator sees all of them at once.
#[derive(Default)]
struct Checker {
    problems: Vec<Problem>,
}

impl Checker {
    fn fail(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.problems.push(Problem {
            field: field.into(),
            message: message.into(),
        });
    }

    fn time(&mut self, field: &str, text: &str) -> Time {
        text.parse().unwrap_or_else(|_| {
            self.fail(field, format!("`{text}` is not a time of day like 23:00"));
            Time::MIN
        })
    }

    fn duration(
        &mut self,
        field: &str,
        text: Option<&str>,
        default: Duration,
        range: (Duration, Duration),
    ) -> Duration {
        let Some(text) = text else {
            return default;
        };
        match duration::parse(text) {
            Ok(value) if value < range.0 || value > range.1 => {
                self.fail(
                    field,
                    format!(
                        "must be between {} and {}",
                        duration::format(range.0),
                        duration::format(range.1)
                    ),
                );
                default
            }
            Ok(value) => value,
            Err(error) => {
                self.fail(field, error.to_string());
                default
            }
        }
    }

    fn number<T: PartialOrd + fmt::Display + Copy>(
        &mut self,
        field: &str,
        value: Option<T>,
        default: T,
        range: (T, T),
    ) -> T {
        match value {
            None => default,
            Some(value) if value < range.0 || value > range.1 => {
                self.fail(
                    field,
                    format!("must be between {} and {}", range.0, range.1),
                );
                default
            }
            Some(value) => value,
        }
    }
}

const MINUTE: Duration = Duration::from_mins(1);
const HOUR: Duration = Duration::from_hours(1);
const DAY: Duration = Duration::from_hours(24);

impl RawConfig {
    #[allow(clippy::too_many_lines)] // One linear pass over every field.
    fn validate(self) -> Result<Config, Vec<Problem>> {
        let mut check = Checker::default();
        let defaults = Caps::default();

        let timezone = self.timezone.trim().to_owned();
        let timezone_ok = !timezone.is_empty()
            && timezone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'));
        if !timezone_ok {
            check.fail("timezone", "expected an IANA name like Europe/London");
        }

        let workers = check.number("workers", self.workers, 1, (1, MAX_WORKERS));
        let approval_expiry = check.duration(
            "approval_expiry",
            self.approval_expiry.as_deref(),
            3 * DAY,
            (HOUR, 30 * DAY),
        );
        let weekly_usage_cap = check.number(
            "weekly_usage_cap",
            self.weekly_usage_cap,
            DEFAULT_WEEKLY_USAGE_CAP,
            (1, 100),
        );

        let state_dir = self
            .state_dir
            .unwrap_or_else(|| PathBuf::from("/var/lib/dsf"));
        if !state_dir.is_absolute() {
            check.fail("state_dir", "must be an absolute path");
        }

        let window = match &self.window {
            Some(raw) => Window::new(
                check.time("window.start", &raw.start),
                check.time("window.end", &raw.end),
            ),
            None => Window::new(
                jiff::civil::time(23, 0, 0, 0),
                jiff::civil::time(7, 0, 0, 0),
            ),
        };

        if self.github.allowed_users.is_empty() {
            check.fail("github.allowed_users", "list at least one GitHub login");
        }
        for login in &self.github.allowed_users {
            if !is_login(login) {
                check.fail(
                    "github.allowed_users",
                    format!("`{login}` is not a valid GitHub login"),
                );
            }
        }
        let allowlist = Allowlist::new(&self.github.allowed_users);
        if self.github.app_id == Some(0) {
            check.fail(
                "github.app_id",
                "expected the App ID from the app's settings page",
            );
        }

        if self.repos.is_empty() {
            check.fail("repos", "configure at least one repo");
        }
        let mut repos: Vec<RepoConfig> = Vec::new();
        for raw in self.repos {
            match raw.name.parse::<RepoSlug>() {
                Ok(slug) if repos.iter().any(|repo| repo.slug.matches(&slug)) => {
                    check.fail("repos", format!("`{slug}` is listed twice"));
                }
                Ok(slug) => repos.push(RepoConfig {
                    slug,
                    approval: raw.approval,
                }),
                Err(message) => check.fail("repos", message),
            }
        }

        let caps = Caps {
            max_visits: check.number(
                "caps.max_visits",
                self.caps.max_visits,
                defaults.max_visits,
                (1, 20),
            ),
            command_timeout: check.duration(
                "caps.command_timeout",
                self.caps.command_timeout.as_deref(),
                defaults.command_timeout,
                (MINUTE, DAY),
            ),
            max_stages: check.number(
                "caps.max_stages",
                self.caps.max_stages,
                defaults.max_stages,
                (1, 64),
            ),
        };

        let mut egress: Vec<String> = BUILTIN_EGRESS.iter().map(ToString::to_string).collect();
        for host in self.egress.allow {
            let host = host.to_ascii_lowercase();
            if is_hostname(&host) {
                egress.push(host);
            } else {
                check.fail(
                    "egress.allow",
                    format!(
                        "`{host}` is not a plain host name (no scheme, port, path or wildcard)"
                    ),
                );
            }
        }
        egress.sort();
        egress.dedup();

        let image = |check: &mut Checker, field: &str, text: &str| {
            let image = text.trim().to_owned();
            if image.is_empty() || image.contains(char::is_whitespace) {
                check.fail(field, "expected an image reference");
            }
            image
        };
        let worker_image = image(&mut check, "images.worker", &self.images.worker);
        let dsf_image = self
            .images
            .dsf
            .as_deref()
            .map(|text| image(&mut check, "images.dsf", text));

        for entry in &self.protect.extra {
            if !protect::is_valid_entry(entry) {
                check.fail(
                    "protect.extra",
                    format!("`{entry}` must be a repo-relative path"),
                );
            }
        }
        let protected = ProtectedPaths::new(&self.protect.extra);

        if !check.problems.is_empty() {
            return Err(check.problems);
        }
        Ok(Config {
            timezone,
            workers,
            window,
            approval_expiry,
            weekly_usage_cap,
            state_dir,
            allowlist,
            github_app_id: self.github.app_id,
            repos,
            caps,
            egress,
            worker_image,
            dsf_image,
            protected,
            protect_extra: self.protect.extra,
        })
    }
}

/// GitHub logins: 1–39 ASCII letters, digits or single hyphens, not
/// starting or ending with a hyphen.
fn is_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A lowercase DNS name with at least two labels.
fn is_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/config.toml");

    const MINIMAL: &str = r#"
        timezone = "Europe/London"
        [github]
        allowed_users = ["octocat"]
        [[repos]]
        name = "octocat/hello-world"
        [images]
        worker = "dsf-worker:local"
    "#;

    fn problems(toml: &str) -> Vec<Problem> {
        match Config::parse(toml) {
            Err(ConfigError::Invalid(problems)) => problems,
            other => panic!("expected problems, got {other:?}"),
        }
    }

    fn fields(toml: &str) -> Vec<String> {
        problems(toml)
            .into_iter()
            .map(|problem| problem.field)
            .collect()
    }

    #[test]
    fn example_config_is_valid() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.workers, 1);
        assert_eq!(
            config.window,
            Window::new(
                jiff::civil::time(23, 0, 0, 0),
                jiff::civil::time(7, 0, 0, 0)
            )
        );
        assert!(config.egress.iter().any(|host| host == "api.anthropic.com"));
        assert_eq!(
            config.dsf_image.as_deref(),
            Some("ghcr.io/your-github-login/dsf:v0.1.0")
        );
    }

    #[test]
    fn minimal_config_gets_the_agreed_defaults() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.workers, 1);
        assert_eq!(config.window.start, jiff::civil::time(23, 0, 0, 0));
        assert_eq!(config.window.end, jiff::civil::time(7, 0, 0, 0));
        assert_eq!(config.approval_expiry, 3 * DAY);
        assert_eq!(config.weekly_usage_cap, 80);
        assert_eq!(config.state_dir, PathBuf::from("/var/lib/dsf"));
        assert_eq!(config.caps, Caps::default());
        assert_eq!(config.egress, ["api.anthropic.com"]);
        assert_eq!(config.dsf_image, None);
        assert_eq!(config.repos[0].approval, ApprovalPolicy::Label);
        assert!(config.allowlist.contains("OctoCat"));
        assert!(config.protected.is_protected(".github/workflows/ci.yml"));
    }

    #[test]
    fn finds_repos_case_insensitively() {
        let config = Config::parse(MINIMAL).unwrap();
        assert!(
            config
                .repo(&"OctoCat/Hello-World".parse().unwrap())
                .is_some()
        );
        assert!(config.repo(&"octocat/other".parse().unwrap()).is_none());
    }

    #[test]
    fn rejects_unknown_keys_including_misplaced_secrets() {
        for extra in ["github_token = \"x\"", "[claude]\ntoken = \"x\""] {
            let toml = format!("{extra}\n{MINIMAL}");
            assert!(
                matches!(Config::parse(&toml), Err(ConfigError::Toml(_))),
                "{extra}"
            );
        }
    }

    #[test]
    fn workers_are_capped_at_two() {
        assert_eq!(fields(&format!("workers = 3\n{MINIMAL}")), ["workers"]);
        assert_eq!(fields(&format!("workers = 0\n{MINIMAL}")), ["workers"]);
        assert_eq!(
            Config::parse(&format!("workers = 2\n{MINIMAL}"))
                .unwrap()
                .workers,
            2
        );
    }

    #[test]
    fn the_usage_cap_is_a_percentage() {
        assert_eq!(
            fields(&format!("weekly_usage_cap = 0\n{MINIMAL}")),
            ["weekly_usage_cap"]
        );
        assert_eq!(
            fields(&format!("weekly_usage_cap = 101\n{MINIMAL}")),
            ["weekly_usage_cap"]
        );
        let config = Config::parse(&format!("weekly_usage_cap = 100\n{MINIMAL}")).unwrap();
        assert_eq!(config.weekly_usage_cap, 100);
    }

    #[test]
    fn reports_every_problem_at_once() {
        let toml = r#"
            timezone = ""
            approval_expiry = "soon"
            state_dir = "relative/dir"
            window = { start = "23:00", end = "25:00" }
            [github]
            allowed_users = ["-bad-"]
            [[repos]]
            name = "not-a-slug"
            [[repos]]
            name = "a/b"
            [[repos]]
            name = "A/B"
            [caps]
            max_visits = 50
            [egress]
            allow = ["https://pypi.org", "*.npmjs.org"]
            [images]
            worker = " "
            dsf = ""
            [protect]
            extra = ["../outside"]
        "#;
        assert_eq!(
            fields(toml),
            [
                "timezone",
                "approval_expiry",
                "state_dir",
                "window.end",
                "github.allowed_users",
                "repos",
                "repos",
                "caps.max_visits",
                "egress.allow",
                "egress.allow",
                "images.worker",
                "images.dsf",
                "protect.extra",
            ]
        );
    }

    #[test]
    fn problem_messages_name_the_field() {
        let message =
            ConfigError::Invalid(problems(&format!("workers = 9\n{MINIMAL}"))).to_string();
        assert_eq!(
            message,
            "invalid config:\n  - `workers`: must be between 1 and 2"
        );
    }

    #[test]
    fn egress_hosts_are_lowercased_sorted_and_deduplicated() {
        let toml = format!(
            "{MINIMAL}\n[egress]\nallow = [\"Registry.npmjs.org\", \"api.anthropic.com\", \"pypi.org\"]"
        );
        assert_eq!(
            Config::parse(&toml).unwrap().egress,
            ["api.anthropic.com", "pypi.org", "registry.npmjs.org"]
        );
    }

    #[test]
    fn repo_slugs_and_logins_follow_github_rules() {
        assert!("octocat/hello.world_2-x".parse::<RepoSlug>().is_ok());
        for bad in [
            "octocat",
            "octocat/",
            "/repo",
            "octo--cat/x",
            "octocat/..",
            "octocat/a b",
            "a/b/c",
        ] {
            assert!(bad.parse::<RepoSlug>().is_err(), "{bad}");
        }
        assert_eq!(
            "octocat/x".parse::<RepoSlug>().unwrap().to_string(),
            "octocat/x"
        );
    }
}
