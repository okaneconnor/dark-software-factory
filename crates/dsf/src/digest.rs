//! The morning digest: what the factory finished since the last digest,
//! what's waiting on a person, and where things stand. Built from the
//! store alone, then posted to a Slack or Teams channel through an incoming
//! webhook or, with none, printed for the journal.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use dsf_core::config::Config;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use reqwest::Url;
use serde_json::{Value, json};

use crate::factory::GIT_BASE;
use crate::scheduler::local;
use crate::status::Board;
use crate::store::{Run, RunStatus, Store};

/// `kv` key: when the last digest went out.
pub const LAST_DIGEST: &str = "last_digest";
/// How far back the first digest looks.
const FIRST_LOOKBACK: SignedDuration = SignedDuration::from_hours(24);
/// A digest never reaches back further than this, however long it's been.
const MAX_LOOKBACK: SignedDuration = SignedDuration::from_hours(7 * 24);
/// Ticks run every few minutes; a last tick older than this means the
/// timer has stopped.
const STALE_TICK: SignedDuration = SignedDuration::from_hours(1);
const POST_TIMEOUT: Duration = Duration::from_secs(30);
/// Slack's limits on a section's text and a header's.
const SLACK_SECTION_MAX: usize = 3000;
const SLACK_HEADER_MAX: usize = 150;

/// Where a webhook delivers the digest, told apart by its host: Slack's
/// incoming webhooks all live on one, and anything else is taken to be a
/// Teams Workflows webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Slack,
    Teams,
}

impl Channel {
    pub fn of(webhook: &Url) -> Self {
        if webhook.host_str() == Some("hooks.slack.com") {
            Self::Slack
        } else {
            Self::Teams
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Slack => "Slack",
            Self::Teams => "Teams",
        }
    }
}

pub struct Digest {
    since: Timestamp,
    board: Board,
    /// Runs finished since `since`, most recent first.
    finished: Vec<Run>,
}

/// A digest ready to print or post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub title: String,
    pub period: String,
    /// Problems that need a person, such as the timer having stopped.
    pub warnings: Vec<String>,
    pub sections: Vec<Section>,
    pub facts: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// e.g. `octo/calc#1`.
    pub issue: String,
    pub link: Option<String>,
    pub text: String,
}

impl Digest {
    pub fn load(store: &Store, now: Timestamp) -> Result<Self> {
        let earliest = now.checked_sub(MAX_LOOKBACK).unwrap_or(now);
        let since = store
            .kv_get(LAST_DIGEST)?
            .and_then(|text| text.parse::<Timestamp>().ok())
            .unwrap_or_else(|| now.checked_sub(FIRST_LOOKBACK).unwrap_or(now))
            .max(earliest);
        Ok(Self {
            since,
            board: Board::load(store, now)?,
            finished: store.finished_since(since)?,
        })
    }

    /// Records that this digest went out, so the next starts from here.
    pub fn sent(store: &mut Store, at: Timestamp) -> Result<()> {
        store.kv_set(LAST_DIGEST, &at.to_string())
    }

    pub fn summarise(&self, config: &Config, tz: &TimeZone) -> Summary {
        let mut published = Vec::new();
        let mut blocked = Vec::new();
        let mut canceled = Vec::new();
        for run in &self.finished {
            let reason = run.end_reason.as_deref().unwrap_or("no reason recorded");
            match run.status {
                RunStatus::Published => published.push(match run.pr_number {
                    Some(pr) => Item {
                        issue: issue_ref(run),
                        link: github_link(&run.repo, &format!("pull/{pr}")),
                        text: format!("{}: draft PR #{pr}", run.issue_title),
                    },
                    None => Item {
                        issue: issue_ref(run),
                        link: None,
                        text: format!("{}: branch pushed, no pull request", run.issue_title),
                    },
                }),
                RunStatus::Blocked => blocked.push(issue_item(run, reason)),
                RunStatus::PublishFailed => {
                    blocked.push(issue_item(run, &format!("couldn't publish: {reason}")));
                }
                RunStatus::Canceled => canceled.push(issue_item(run, reason)),
                _ => {}
            }
        }
        let columns = self.board.columns(config, tz);
        let plain = |lines: Vec<String>| -> Vec<Item> {
            lines
                .into_iter()
                .map(|text| Item {
                    issue: String::new(),
                    link: None,
                    text,
                })
                .collect()
        };

        let mut counts = Vec::new();
        if !published.is_empty() {
            counts.push(count(published.len(), "draft PR", "draft PRs"));
        }
        if !blocked.is_empty() {
            counts.push(format!("{} blocked", blocked.len()));
        }
        if !columns.awaiting.is_empty() {
            counts.push(format!("{} awaiting approval", columns.awaiting.len()));
        }
        let title = if counts.is_empty() {
            "dsf: nothing finished".to_owned()
        } else {
            format!("dsf: {}", counts.join(", "))
        };

        let mut warnings = Vec::new();
        let stale_after = self
            .board
            .now
            .checked_sub(STALE_TICK)
            .unwrap_or(self.board.now);
        match &self.board.last_tick {
            None => warnings.push("dsf has never ticked; is dsf-tick.timer enabled?".to_owned()),
            Some(tick) if tick.at < stale_after => warnings.push(format!(
                "no tick since {}; is dsf-tick.timer running?",
                local(tick.at, tz)
            )),
            Some(_) => {}
        }
        if let Some(reason) = &self.board.paused {
            warnings.push(format!(
                "claiming is paused ({reason}); `dsf unpause` restarts it"
            ));
        }

        let sections = [
            ("Draft PRs", published),
            ("Blocked", blocked),
            ("Canceled", canceled),
            ("Awaiting plan approval", plain(columns.awaiting)),
            ("Running", plain(columns.running)),
            ("Ready", plain(columns.ready)),
        ]
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(title, items)| Section { title, items })
        .collect();

        let cap = config.weekly_usage_cap;
        Summary {
            title,
            period: format!("Since {} ({})", local(self.since, tz), config.timezone),
            warnings,
            sections,
            facts: vec![
                ("Usage", self.board.usage_text(cap, tz)),
                ("Claiming", self.board.claiming_text(config, tz)),
                (
                    "Window",
                    crate::status::window_text(config, tz, self.board.now),
                ),
                ("Last tick", self.board.last_tick_text(tz)),
            ],
        }
    }
}

impl Summary {
    /// For stdout and the journal.
    pub fn text(&self) -> String {
        let mut out = format!("{}\n{}\n", self.title, self.period);
        for warning in &self.warnings {
            let _ = writeln!(out, "Warning: {warning}");
        }
        for section in &self.sections {
            let _ = writeln!(out, "\n{} ({})", section.title, section.items.len());
            for item in &section.items {
                let _ = match (item.issue.is_empty(), &item.link) {
                    (true, _) => writeln!(out, "  {}", item.text),
                    (false, Some(link)) => {
                        writeln!(out, "  {}  {}  {link}", item.issue, item.text)
                    }
                    (false, None) => writeln!(out, "  {}  {}", item.issue, item.text),
                };
            }
        }
        out.push('\n');
        for (name, value) in &self.facts {
            let _ = writeln!(out, "{:<10} {value}", format!("{name}:"));
        }
        out
    }

    /// The message `channel`'s webhook expects.
    pub fn message(&self, channel: Channel) -> Value {
        match channel {
            Channel::Slack => self.slack(),
            Channel::Teams => self.card(),
        }
    }

    /// A Slack incoming-webhook message in Block Kit. `text` is what
    /// notifications show.
    pub fn slack(&self) -> Value {
        let mut blocks = vec![
            json!({"type": "header", "text": {"type": "plain_text", "text": clip(&self.title, SLACK_HEADER_MAX)}}),
            json!({"type": "context", "elements": [{"type": "mrkdwn", "text": slack_escape(&self.period)}]}),
        ];
        for warning in &self.warnings {
            blocks.push(json!({"type": "section", "text": {"type": "mrkdwn", "text": format!(":warning: {}", slack_escape(warning))}}));
        }
        for section in &self.sections {
            let mut text = format!("*{}* ({})", section.title, section.items.len());
            for item in &section.items {
                let issue = slack_escape(&item.issue);
                let line = match (item.issue.is_empty(), &item.link) {
                    (true, _) => slack_escape(&item.text),
                    (false, Some(link)) => format!("<{link}|{issue}> {}", slack_escape(&item.text)),
                    (false, None) => format!("{issue} {}", slack_escape(&item.text)),
                };
                let _ = write!(text, "\n• {line}");
            }
            blocks.push(json!({"type": "section", "text": {"type": "mrkdwn", "text": clip(&text, SLACK_SECTION_MAX)}}));
        }
        let fields: Vec<Value> = self
            .facts
            .iter()
            .map(|(name, value)| json!({"type": "mrkdwn", "text": format!("*{name}*\n{}", slack_escape(value))}))
            .collect();
        blocks.push(json!({"type": "section", "fields": fields}));
        json!({"text": self.title, "blocks": blocks})
    }

    /// The message a Teams Workflows webhook expects: one Adaptive Card.
    pub fn card(&self) -> Value {
        let mut body = vec![
            json!({"type": "TextBlock", "text": self.title, "weight": "Bolder", "size": "Medium", "wrap": true}),
            json!({"type": "TextBlock", "text": self.period, "isSubtle": true, "spacing": "None", "wrap": true}),
        ];
        for warning in &self.warnings {
            body.push(json!({"type": "TextBlock", "text": format!("⚠ {warning}"), "color": "Attention", "wrap": true}));
        }
        for section in &self.sections {
            body.push(json!({
                "type": "TextBlock",
                "text": format!("{} ({})", section.title, section.items.len()),
                "weight": "Bolder",
                "spacing": "Medium",
                "wrap": true
            }));
            let lines: Vec<String> = section
                .items
                .iter()
                .map(|item| match (item.issue.is_empty(), &item.link) {
                    (true, _) => format!("- {}", item.text),
                    (false, Some(link)) => format!("- [{}]({link}) {}", item.issue, item.text),
                    (false, None) => format!("- {} {}", item.issue, item.text),
                })
                .collect();
            body.push(json!({"type": "TextBlock", "text": lines.join("\n"), "spacing": "Small", "wrap": true}));
        }
        let facts: Vec<Value> = self
            .facts
            .iter()
            .map(|(title, value)| json!({"title": title, "value": value}))
            .collect();
        body.push(json!({"type": "FactSet", "facts": facts, "spacing": "Medium"}));
        json!({
            "type": "message",
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "contentUrl": null,
                "content": {
                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                    "type": "AdaptiveCard",
                    "version": "1.4",
                    "body": body
                }
            }]
        })
    }
}

/// Posts the digest to a Slack or Teams webhook, in the form that one
/// expects. The URL is a secret, so it never appears in an error.
pub async fn post(webhook: &Url, summary: &Summary) -> Result<Channel> {
    let channel = Channel::of(webhook);
    let to = channel.name();
    crate::github::install_crypto();
    let client = reqwest::Client::builder()
        .user_agent(concat!("dsf/", env!("CARGO_PKG_VERSION")))
        .timeout(POST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(webhook.scheme() == "https")
        .build()?;
    let response = client
        .post(webhook.clone())
        .json(&summary.message(channel))
        .send()
        .await
        .map_err(|error| anyhow!("posting the digest to {to}: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        bail!("posting the digest to {to}: the webhook answered {status}");
    }
    Ok(channel)
}

/// A webhook URL from its credential file, refused unless it's HTTPS.
pub fn webhook_url(text: &str) -> Result<Url> {
    let url = Url::parse(text.trim()).map_err(|_| anyhow!("the digest webhook is not a URL"))?;
    if url.scheme() != "https" {
        bail!("the digest webhook must be an https:// URL");
    }
    Ok(url)
}

/// Slack's mrkdwn treats these three as markup.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// At most `max` characters, ending in an ellipsis if cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max - 1).collect();
    clipped.push('…');
    clipped
}

fn issue_ref(run: &Run) -> String {
    format!("{}#{}", run.repo, run.issue)
}

fn issue_item(run: &Run, reason: &str) -> Item {
    Item {
        issue: issue_ref(run),
        link: github_link(&run.repo, &format!("issues/{}", run.issue)),
        text: format!("{}: {reason}", run.issue_title),
    }
}

/// Local runs (`local/...`) have nowhere to link to.
fn github_link(repo: &str, path: &str) -> Option<String> {
    (!repo.starts_with("local/")).then(|| format!("{GIT_BASE}{repo}/{path}"))
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{LAST_TICK, PAUSED, QueuedIssue, TickRecord};
    use crate::store::{Advance, AttemptEnd, NewRun, SuspendReason};
    use dsf_core::routing::Progress;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn london() -> TimeZone {
        TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").unwrap()
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
            require_approval: true,
        }
    }

    fn tick(store: &mut Store, when: &str) {
        let record = TickRecord {
            at: at(when),
            queue: vec![QueuedIssue {
                repo: "octo/calc".into(),
                number: 7,
                title: "Support floats".into(),
            }],
            notes: Vec::new(),
        };
        store
            .kv_set(LAST_TICK, &serde_json::to_string(&record).unwrap())
            .unwrap();
    }

    /// A night's work: two PRs, one blocked, one that couldn't publish,
    /// one waiting for approval, and one from before the last digest.
    fn night() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let start = at("2026-10-19T22:05:00Z");
        let old = store
            .create_run(&new_run(1, "Old news"), start)
            .unwrap()
            .unwrap();
        store
            .end_run(
                old,
                RunStatus::Published,
                "published",
                at("2026-10-19T05:00:00Z"),
            )
            .unwrap();
        for (issue, title, pr, ended) in [
            (2, "add() subtracts", 9, "2026-10-19T23:40:00Z"),
            (3, "mean() of nothing", 10, "2026-10-20T01:15:00Z"),
        ] {
            let run = store
                .create_run(&new_run(issue, title), start)
                .unwrap()
                .unwrap();
            store.set_pr(run, pr, at(ended)).unwrap();
            store
                .end_run(run, RunStatus::Published, "published", at(ended))
                .unwrap();
        }
        let stuck = store
            .create_run(&new_run(4, "Make it fast"), start)
            .unwrap()
            .unwrap();
        store
            .end_run(
                stuck,
                RunStatus::Blocked,
                "the plan needs a decision: which inputs are slow?",
                at("2026-10-20T02:00:00Z"),
            )
            .unwrap();
        let unpublished = store
            .create_run(&new_run(5, "Docs"), start)
            .unwrap()
            .unwrap();
        store
            .end_run(
                unpublished,
                RunStatus::PublishFailed,
                "the push was rejected",
                at("2026-10-20T03:00:00Z"),
            )
            .unwrap();
        let waiting = store
            .create_run(&new_run(6, "Rename calc"), start)
            .unwrap()
            .unwrap();
        let attempt = store
            .start_attempt(waiting, "approve", 1, None, at("2026-10-20T04:00:00Z"))
            .unwrap();
        store
            .finish_attempt(
                waiting,
                attempt,
                &AttemptEnd::default(),
                &Progress::default(),
                None,
                Advance::Suspend {
                    reason: SuspendReason::Approval,
                    resume_after: None,
                },
                at("2026-10-20T04:00:00Z"),
            )
            .unwrap();
        store.kv_set(LAST_DIGEST, "2026-10-19T06:00:00Z").unwrap();
        tick(&mut store, "2026-10-20T05:55:00Z");
        store
    }

    #[test]
    fn summarises_the_night_since_the_last_digest() {
        let store = night();
        let digest = Digest::load(&store, at("2026-10-20T06:00:00Z")).unwrap();
        let summary = digest.summarise(&config(), &london());
        insta::assert_snapshot!("text", summary.text());
        insta::assert_snapshot!(
            "card",
            serde_json::to_string_pretty(&summary.card()).unwrap()
        );
        insta::assert_snapshot!(
            "slack",
            serde_json::to_string_pretty(&summary.slack()).unwrap()
        );
    }

    #[test]
    fn a_quiet_night_and_a_stopped_timer_are_both_reported() {
        let mut store = Store::open_in_memory().unwrap();
        let now = at("2026-10-20T06:00:00Z");
        let summary = Digest::load(&store, now)
            .unwrap()
            .summarise(&config(), &london());
        assert_eq!(summary.title, "dsf: nothing finished");
        // The first digest looks back a day.
        assert_eq!(summary.period, "Since Mon 19 Oct 07:00 (Europe/London)");
        assert_eq!(
            summary.warnings,
            ["dsf has never ticked; is dsf-tick.timer enabled?"]
        );

        tick(&mut store, "2026-10-20T03:00:00Z");
        store.kv_set(PAUSED, "maintenance").unwrap();
        Digest::sent(&mut store, at("2026-10-01T06:00:00Z")).unwrap();
        let summary = Digest::load(&store, now)
            .unwrap()
            .summarise(&config(), &london());
        assert_eq!(
            summary.warnings,
            [
                "no tick since Tue 20 Oct 04:00; is dsf-tick.timer running?",
                "claiming is paused (maintenance); `dsf unpause` restarts it"
            ]
        );
        // Never more than a week back.
        assert_eq!(summary.period, "Since Tue 13 Oct 07:00 (Europe/London)");
    }

    #[test]
    fn only_https_webhooks_are_accepted() {
        assert!(webhook_url("https://example.invalid/workflows/abc?sig=x\n").is_ok());
        let plain = webhook_url("http://example.invalid/x").unwrap_err();
        assert_eq!(
            plain.to_string(),
            "the digest webhook must be an https:// URL"
        );
        let junk = webhook_url("sig=secret-value").unwrap_err().to_string();
        assert!(!junk.contains("secret-value"), "{junk}");
    }

    #[test]
    fn slack_webhooks_get_slack_messages() {
        let url = |text: &str| Url::parse(text).unwrap();
        assert_eq!(
            Channel::of(&url("https://hooks.slack.com/services/T0/B0/x")),
            Channel::Slack
        );
        assert_eq!(
            Channel::of(&url(
                "https://prod-1.westeurope.logic.azure.com/workflows/x"
            )),
            Channel::Teams
        );
        let summary = Summary {
            title: "dsf: 1 blocked".to_owned(),
            period: "Since Mon".to_owned(),
            warnings: Vec::new(),
            sections: vec![Section {
                title: "Blocked",
                items: vec![Item {
                    issue: "octo/calc#1".to_owned(),
                    link: Some("https://github.com/octo/calc/issues/1".to_owned()),
                    text: "Fix <b> & co: x".to_owned(),
                }],
            }],
            facts: vec![("Usage", "a > b".to_owned())],
        };
        let message = summary.message(Channel::Slack);
        assert_eq!(message["text"], "dsf: 1 blocked");
        assert_eq!(
            message["blocks"][2]["text"]["text"],
            "*Blocked* (1)\n• <https://github.com/octo/calc/issues/1|octo/calc#1> Fix &lt;b&gt; &amp; co: x"
        );
        assert_eq!(
            message["blocks"][3]["fields"][0]["text"],
            "*Usage*\na &gt; b"
        );
        assert_eq!(clip(&"x".repeat(10), 5), "xxxx…");
    }

    #[tokio::test]
    async fn posts_the_card_and_keeps_the_url_out_of_errors() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workflows/accepted"))
            .and(body_partial_json(json!({"type": "message"})))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/workflows/refused"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let summary = Digest::load(&night(), at("2026-10-20T06:00:00Z"))
            .unwrap()
            .summarise(&config(), &london());

        let accepted =
            Url::parse(&format!("{}/workflows/accepted?sig=s3cret", server.uri())).unwrap();
        assert_eq!(post(&accepted, &summary).await.unwrap(), Channel::Teams);

        let refused =
            Url::parse(&format!("{}/workflows/refused?sig=s3cret", server.uri())).unwrap();
        let error = post(&refused, &summary).await.unwrap_err().to_string();
        assert_eq!(
            error,
            "posting the digest to Teams: the webhook answered 403 Forbidden"
        );
        // Nothing listening: the connection error must not show the URL.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let gone = Url::parse(&format!("http://127.0.0.1:{port}/workflows?sig=s3cret")).unwrap();
        let error = post(&gone, &summary).await.unwrap_err().to_string();
        assert!(
            error.starts_with("posting the digest to Teams: "),
            "{error}"
        );
        assert!(!error.contains("s3cret"), "{error}");
    }
}
