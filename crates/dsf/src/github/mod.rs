//! The GitHub REST API, as the GitHub App. Every call uses an installation
//! token that lasts an hour and is narrowed to one repository and the
//! permissions that kind of call needs (see [`Scope`]): claiming an issue
//! can't push, and publishing can't reach another repository.

pub mod app;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use dsf_core::config::RepoSlug;
use dsf_core::labels::Label as LabelSpec;
use jiff::{SignedDuration, Timestamp};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, LINK};
use reqwest::{Method, Response, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};

use self::app::AppKey;

pub const API: &str = "https://api.github.com/";

const API_VERSION: &str = "2022-11-28";
const TIMEOUT: Duration = Duration::from_secs(30);
/// 100 items a page; anything longer than this is treated as an error
/// rather than silently cut short.
const MAX_PAGES: usize = 20;
/// Installation tokens last an hour; one is replaced this long before it
/// expires so it can't lapse mid-request.
const TOKEN_MARGIN: SignedDuration = SignedDuration::from_mins(5);

/// What an installation token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read issues, edit their labels and comment.
    Issues,
    /// Clone the repository.
    Read,
    /// Push the run's branch and open its pull request.
    Publish,
}

impl Scope {
    fn permissions(self) -> Value {
        match self {
            Self::Issues => json!({ "issues": "write", "metadata": "read" }),
            Self::Read => json!({ "contents": "read", "metadata": "read" }),
            Self::Publish => {
                json!({ "contents": "write", "pull_requests": "write", "metadata": "read" })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub user: User,
    pub labels: Vec<Label>,
    /// Present when the "issue" is a pull request.
    #[serde(default)]
    pub pull_request: Option<Value>,
}

impl Issue {
    pub fn label_names(&self) -> Vec<String> {
        self.labels.iter().map(|label| label.name.clone()).collect()
    }

    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

/// An entry from an issue's events. Only label changes matter to dsf.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueEvent {
    pub event: String,
    #[serde(default)]
    pub actor: Option<User>,
    #[serde(default)]
    pub label: Option<EventLabel>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventLabel {
    pub name: String,
}

impl IssueEvent {
    /// The event as a label change, if it is one.
    pub fn as_label_event(&self) -> Option<dsf_core::issue::LabelEvent<'_>> {
        let added = match self.event.as_str() {
            "labeled" => true,
            "unlabeled" => false,
            _ => return None,
        };
        Some(dsf_core::issue::LabelEvent {
            actor: &self.actor.as_ref()?.login,
            label: &self.label.as_ref()?.name,
            added,
            at: self.created_at,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub user: User,
    #[serde(default)]
    pub body: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub default_branch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
}

#[derive(Deserialize)]
struct AccessToken {
    token: SecretString,
    expires_at: Timestamp,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Deserialize)]
struct AppInfo {
    slug: String,
}

struct CachedToken {
    token: SecretString,
    expires_at: Timestamp,
}

/// A pull request to open, or to update if the branch already has one.
#[derive(Debug, Clone, Copy)]
pub struct NewPullRequest<'a> {
    pub title: &'a str,
    pub head: &'a str,
    pub body: &'a str,
}

pub struct GitHub {
    http: reqwest::Client,
    api: Url,
    key: AppKey,
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<(String, Scope), CachedToken>>,
    login: OnceCell<String>,
}

impl GitHub {
    /// `api` is [`API`] except in tests, which point it at a fake server.
    pub fn new(key: AppKey, api: &str) -> Result<Self> {
        install_crypto();
        let mut api = Url::parse(api).with_context(|| format!("`{api}` is not a URL"))?;
        if !api.path().ends_with('/') {
            api.set_path(&format!("{}/", api.path()));
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, "application/vnd.github+json".parse()?);
        headers.insert("x-github-api-version", API_VERSION.parse()?);
        let http = reqwest::Client::builder()
            .user_agent(concat!("dsf/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(api.scheme() == "https")
            .build()?;
        Ok(Self {
            http,
            api,
            key,
            installations: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            login: OnceCell::new(),
        })
    }

    /// The app's own login, e.g. `dsf-example[bot]`: who its labels and
    /// comments appear to come from.
    pub async fn login(&self) -> Result<&str> {
        let login = self
            .login
            .get_or_try_init(|| async {
                let jwt = self.key.jwt(Timestamp::now())?;
                let app: AppInfo = self
                    .send(Method::GET, self.url(&["app"]), &jwt, None)
                    .await?
                    .json()
                    .await?;
                Ok::<_, anyhow::Error>(format!("{}[bot]", app.slug))
            })
            .await?;
        Ok(login)
    }

    /// A token for `repo` that can do what `scope` allows, minted on first
    /// use and reused until shortly before it expires.
    pub async fn token(&self, repo: &RepoSlug, scope: Scope) -> Result<SecretString> {
        let key = (repo.to_string().to_ascii_lowercase(), scope);
        let mut tokens = self.tokens.lock().await;
        if let Some(cached) = tokens.get(&key)
            && cached.expires_at > Timestamp::now() + TOKEN_MARGIN
        {
            return Ok(cached.token.clone());
        }
        let installation = self.installation(repo).await?;
        let jwt = self.key.jwt(Timestamp::now())?;
        let url = self.url(&[
            "app",
            "installations",
            &installation.to_string(),
            "access_tokens",
        ]);
        let body = json!({ "repositories": [repo.name], "permissions": scope.permissions() });
        let minted: AccessToken = self
            .send(Method::POST, url, &jwt, Some(&body))
            .await
            .with_context(|| format!("getting a token for {repo}"))?
            .json()
            .await?;
        let token = minted.token.clone();
        tokens.insert(
            key,
            CachedToken {
                token: minted.token,
                expires_at: minted.expires_at,
            },
        );
        Ok(token)
    }

    async fn installation(&self, repo: &RepoSlug) -> Result<u64> {
        let key = repo.to_string().to_ascii_lowercase();
        if let Some(id) = self.installations.lock().await.get(&key) {
            return Ok(*id);
        }
        let jwt = self.key.jwt(Timestamp::now())?;
        let url = self.url(&["repos", &repo.owner, &repo.name, "installation"]);
        let response = self.request(Method::GET, url, &jwt, None).await?;
        if response.status() == StatusCode::NOT_FOUND {
            bail!(
                "the GitHub App (ID {}) is not installed on {repo}; install it there first",
                self.key.app_id()
            );
        }
        let installation: Installation = check(response).await?.json().await?;
        self.installations.lock().await.insert(key, installation.id);
        Ok(installation.id)
    }

    pub async fn repository(&self, repo: &RepoSlug) -> Result<Repository> {
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.url(&["repos", &repo.owner, &repo.name]);
        Ok(self
            .send(Method::GET, url, &token, None)
            .await?
            .json()
            .await?)
    }

    pub async fn issue(&self, repo: &RepoSlug, number: u64) -> Result<Issue> {
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.issue_url(repo, number, &[]);
        self.send(Method::GET, url, &token, None)
            .await
            .with_context(|| format!("reading {repo}#{number}"))?
            .json()
            .await
            .with_context(|| format!("reading {repo}#{number}"))
    }

    /// Open issues labelled `label`, not counting pull requests. A listing
    /// can be stale; act only on what [`Self::issue`] says.
    pub async fn open_issues_labelled(&self, repo: &RepoSlug, label: &str) -> Result<Vec<Issue>> {
        let token = self.token(repo, Scope::Issues).await?;
        let mut url = self.url(&["repos", &repo.owner, &repo.name, "issues"]);
        url.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("labels", label);
        let issues: Vec<Issue> = self.pages(url, &token).await?;
        Ok(issues
            .into_iter()
            .filter(|issue| !issue.is_pull_request())
            .collect())
    }

    /// Every event on an issue, oldest first.
    pub async fn issue_events(&self, repo: &RepoSlug, number: u64) -> Result<Vec<IssueEvent>> {
        let token = self.token(repo, Scope::Issues).await?;
        self.pages(self.issue_url(repo, number, &["events"]), &token)
            .await
    }

    /// Every comment on an issue, oldest first.
    pub async fn comments(&self, repo: &RepoSlug, number: u64) -> Result<Vec<Comment>> {
        let token = self.token(repo, Scope::Issues).await?;
        self.pages(self.issue_url(repo, number, &["comments"]), &token)
            .await
    }

    pub async fn comment(&self, repo: &RepoSlug, id: u64) -> Result<Comment> {
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.url(&[
            "repos",
            &repo.owner,
            &repo.name,
            "issues",
            "comments",
            &id.to_string(),
        ]);
        Ok(self
            .send(Method::GET, url, &token, None)
            .await?
            .json()
            .await?)
    }

    pub async fn post_comment(&self, repo: &RepoSlug, number: u64, body: &str) -> Result<Comment> {
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.issue_url(repo, number, &["comments"]);
        Ok(self
            .send(Method::POST, url, &token, Some(&json!({ "body": body })))
            .await?
            .json()
            .await?)
    }

    pub async fn add_labels(&self, repo: &RepoSlug, number: u64, labels: &[&str]) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.issue_url(repo, number, &["labels"]);
        self.send(
            Method::POST,
            url,
            &token,
            Some(&json!({ "labels": labels })),
        )
        .await?;
        Ok(())
    }

    /// Removes a label from an issue. `false` means it wasn't there, which
    /// for `factory:ready` means someone else changed the issue first.
    pub async fn remove_label(&self, repo: &RepoSlug, number: u64, label: &str) -> Result<bool> {
        let token = self.token(repo, Scope::Issues).await?;
        let url = self.issue_url(repo, number, &["labels", label]);
        let response = self.request(Method::DELETE, url, &token, None).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        check(response).await?;
        Ok(true)
    }

    pub async fn labels(&self, repo: &RepoSlug) -> Result<Vec<Label>> {
        let token = self.token(repo, Scope::Issues).await?;
        self.pages(
            self.url(&["repos", &repo.owner, &repo.name, "labels"]),
            &token,
        )
        .await
    }

    /// Creates `label` in the repository, or updates it to match if a label
    /// with the same name (in any case) exists.
    pub async fn put_label(&self, repo: &RepoSlug, label: &LabelSpec, exists: bool) -> Result<()> {
        let token = self.token(repo, Scope::Issues).await?;
        let body = json!({
            "name": label.name,
            "color": label.color,
            "description": label.description,
        });
        let (method, url) = if exists {
            let mut segments = vec!["repos", &repo.owner, &repo.name, "labels"];
            segments.push(label.name);
            (Method::PATCH, self.url(&segments))
        } else {
            (
                Method::POST,
                self.url(&["repos", &repo.owner, &repo.name, "labels"]),
            )
        };
        self.send(method, url, &token, Some(&body)).await?;
        Ok(())
    }

    /// Opens a draft pull request for `request.head`, or updates the open
    /// one that already exists for it, so publishing twice is harmless.
    pub async fn open_pull_request(
        &self,
        repo: &RepoSlug,
        request: &NewPullRequest<'_>,
    ) -> Result<PullRequest> {
        let token = self.token(repo, Scope::Publish).await?;
        let pulls = self.url(&["repos", &repo.owner, &repo.name, "pulls"]);
        let mut find = pulls.clone();
        find.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("head", &format!("{}:{}", repo.owner, request.head));
        let open: Vec<PullRequest> = self
            .send(Method::GET, find, &token, None)
            .await?
            .json()
            .await?;
        if let Some(existing) = open.into_iter().next() {
            let url = self.url(&[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &existing.number.to_string(),
            ]);
            let body = json!({ "title": request.title, "body": request.body });
            return Ok(self
                .send(Method::PATCH, url, &token, Some(&body))
                .await?
                .json()
                .await?);
        }
        let base = self.repository(repo).await?.default_branch;
        let body = json!({
            "title": request.title,
            "head": request.head,
            "base": base,
            "body": request.body,
            "draft": true,
            "maintainer_can_modify": true,
        });
        Ok(self
            .send(Method::POST, pulls, &token, Some(&body))
            .await
            .with_context(|| format!("opening a draft pull request on {repo}"))?
            .json()
            .await?)
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.api.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    fn issue_url(&self, repo: &RepoSlug, number: u64, rest: &[&str]) -> Url {
        let number = number.to_string();
        let mut segments = vec!["repos", &repo.owner, &repo.name, "issues", &number];
        segments.extend_from_slice(rest);
        self.url(&segments)
    }

    /// Follows `Link: rel="next"` through every page, staying on the API.
    async fn pages<T: DeserializeOwned>(
        &self,
        mut url: Url,
        token: &SecretString,
    ) -> Result<Vec<T>> {
        url.query_pairs_mut().append_pair("per_page", "100");
        let mut items = Vec::new();
        for _ in 0..MAX_PAGES {
            let response = self.send(Method::GET, url.clone(), token, None).await?;
            let next = next_page(response.headers());
            items.extend(response.json::<Vec<T>>().await?);
            match next {
                None => return Ok(items),
                Some(next) => {
                    let next = Url::parse(&next)?;
                    if next.origin() != self.api.origin()
                        || !next.path().starts_with(self.api.path())
                    {
                        bail!("GitHub's next page `{next}` is outside the API");
                    }
                    url = next;
                }
            }
        }
        bail!("{url} has more than {MAX_PAGES} pages")
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        token: &SecretString,
        body: Option<&Value>,
    ) -> Result<Response> {
        check(self.request(method, url, token, body).await?).await
    }

    /// One API call, retried once if GitHub is briefly unavailable.
    async fn request(
        &self,
        method: Method,
        url: Url,
        token: &SecretString,
        body: Option<&Value>,
    ) -> Result<Response> {
        let mut retried = false;
        loop {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .header(AUTHORIZATION, format!("Bearer {}", token.expose_secret()));
            if let Some(body) = body {
                request = request.json(body);
            }
            let result = request.send().await;
            let transient = match &result {
                Ok(response) => matches!(
                    response.status(),
                    StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::GATEWAY_TIMEOUT
                ),
                Err(error) => error.is_connect() || error.is_timeout(),
            };
            if transient && !retried {
                retried = true;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            return result.map_err(|error| anyhow!("{method} {}: {error}", url.path()));
        }
    }
}

/// The response if it succeeded, or an error with GitHub's message.
async fn check(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let path = response.url().path().to_owned();
    let limited = response
        .headers()
        .get("x-ratelimit-remaining")
        .is_some_and(|remaining| remaining == "0");
    let reset = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| Timestamp::from_second(seconds).ok());
    let body = response.json::<Value>().await.unwrap_or_default();
    let mut message = body["message"].as_str().unwrap_or_default().to_owned();
    // Validation failures (422) explain themselves in `errors`.
    for error in body["errors"].as_array().into_iter().flatten() {
        if let Some(detail) = error["message"].as_str().or(error["code"].as_str()) {
            message.push_str("; ");
            message.push_str(detail);
        }
    }
    if limited || status == StatusCode::TOO_MANY_REQUESTS {
        match reset {
            Some(reset) => bail!("GitHub rate limit reached until {reset} ({path})"),
            None => bail!("GitHub rate limit reached ({path})"),
        }
    }
    bail!("GitHub said {status} for {path}: {message}")
}

/// The `rel="next"` URL from a `Link` header.
fn next_page(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|param| param.trim() == r#"rel="next""#)
            .then(|| {
                target
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

/// reqwest is built without a default crypto provider; dsf uses ring's.
fn install_crypto() {
    // Fails only if a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_next_page_link() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_page(&headers).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        headers.insert(
            LINK,
            r#"<https://api.github.com/x?page=1>; rel="prev""#.parse().unwrap(),
        );
        assert_eq!(next_page(&headers), None);
        assert_eq!(next_page(&HeaderMap::new()), None);
    }

    #[test]
    fn label_events_come_from_labeled_and_unlabeled_only() {
        let events: Vec<IssueEvent> = serde_json::from_value(json!([
            { "event": "labeled", "actor": { "login": "alice" }, "label": { "name": "factory:ready" }, "created_at": "2026-10-18T10:00:00Z" },
            { "event": "unlabeled", "actor": { "login": "dsf[bot]" }, "label": { "name": "factory:ready" }, "created_at": "2026-10-18T11:00:00Z" },
            { "event": "renamed", "actor": { "login": "alice" }, "created_at": "2026-10-18T12:00:00Z" },
            { "event": "labeled", "actor": null, "label": { "name": "factory:ready" }, "created_at": "2026-10-18T13:00:00Z" }
        ]))
        .unwrap();
        let label_events: Vec<_> = events
            .iter()
            .filter_map(IssueEvent::as_label_event)
            .collect();
        assert_eq!(label_events.len(), 2);
        assert_eq!(label_events[0].actor, "alice");
        assert!(label_events[0].added);
        assert!(!label_events[1].added);
    }
}
