//! Running Claude Code for a stage: the command line it gets, and reading
//! its stream-json output into an outcome. The flags and message shapes
//! come from the spikes (docs/design.md, Spike findings).

use anyhow::{Result, ensure};
use dsf_core::workflow::{AgentMode, Schema};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const READ_TOOLS: &str = "Read,Grep,Glob";
pub const WRITE_TOOLS: &str = "Read,Grep,Glob,Edit,Write,Bash";

/// The prompt travels as one argument, and Linux caps a single argument
/// at 128 KiB.
pub const MAX_PROMPT_BYTES: usize = 120 * 1024;

/// API errors that mean the credentials themselves are the problem.
const AUTH_ERRORS: [&str; 5] = [
    "authentication_failed",
    "oauth_org_not_allowed",
    "account_on_hold",
    "verification_required",
    "billing_error",
];

/// API errors worth waiting out: limits, throttles and server trouble.
const WAIT_ERRORS: [&str; 3] = ["rate_limit", "overloaded", "server_error"];

#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    pub mode: AgentMode,
    pub prompt: &'a str,
    pub model: &'a str,
    pub schema: Option<Schema>,
    pub max_turns: u32,
}

/// The argv for `claude` in an agent container. Every stage runs
/// restricted, with an explicit tool list, and the prompt goes last after
/// `--`, so nothing in it can be read as a flag.
pub fn command_line(invocation: &Invocation<'_>) -> Result<Vec<String>> {
    ensure!(
        invocation.prompt.len() <= MAX_PROMPT_BYTES,
        "prompt is {} bytes; the limit is {MAX_PROMPT_BYTES}",
        invocation.prompt.len()
    );
    let mut argv: Vec<String> = [
        "claude",
        "--print",
        "--restricted",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        invocation.model,
        "--max-turns",
    ]
    .map(str::to_owned)
    .into();
    argv.push(invocation.max_turns.to_string());
    match invocation.mode {
        AgentMode::Read => argv.extend(["--tools", READ_TOOLS].map(str::to_owned)),
        AgentMode::Write => argv.extend(
            [
                "--tools",
                WRITE_TOOLS,
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash",
            ]
            .map(str::to_owned),
        ),
    }
    if let Some(schema) = invocation.schema {
        argv.push("--json-schema".to_owned());
        argv.push(json_schema(schema).to_owned());
    }
    argv.push("--".to_owned());
    argv.push(invocation.prompt.to_owned());
    Ok(argv)
}

/// The JSON Schema Claude's structured output must match. Its `outcome`
/// enum is the schema's outcome labels.
pub const fn json_schema(schema: Schema) -> &'static str {
    match schema {
        Schema::Plan => concat!(
            r#"{"type":"object","additionalProperties":false,"required":["outcome","summary","steps"],"#,
            r#""properties":{"outcome":{"type":"string","enum":["ready","needs_decision"]},"#,
            r#""summary":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}},"#,
            r#""questions":{"type":"array","items":{"type":"string"}}}}"#
        ),
        Schema::Review => concat!(
            r#"{"type":"object","additionalProperties":false,"required":["outcome","summary"],"#,
            r#""properties":{"outcome":{"type":"string","enum":["approve","changes_requested"]},"#,
            r#""summary":{"type":"string"},"issues":{"type":"array","items":{"type":"string"}}}}"#
        ),
    }
}

/// How a Claude run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// Claude finished normally. What that means for the stage, such as the
    /// structured outcome, is the engine's call.
    Completed(Completion),
    /// A usage limit, throttle or server trouble. Try again after
    /// `resets_at`, or after backing off if it's unknown.
    Limited {
        resets_at: Option<Timestamp>,
        detail: String,
    },
    /// The token or account doesn't work. Nothing will until a human acts.
    AuthFailed(String),
    /// The run ended without a usable result.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    pub structured: Option<Value>,
    pub turns: u64,
    /// Notional on a subscription, but a useful measure in the digest.
    pub cost_usd: f64,
    pub usage: Value,
    pub permission_denials: usize,
}

/// Subscription usage, as last reported on the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// The fraction of the window used, usually 0 to 1.
    pub utilization: f64,
    pub resets_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub outcome: AgentOutcome,
    pub usage: Option<Usage>,
}

/// Reads a whole stream-json transcript. Lines that aren't JSON, and
/// message types this version doesn't know, are skipped: the CLI adds new
/// ones over time.
pub fn summarise(stream: &str) -> Summary {
    let mut usage = None;
    let mut limited: Option<(Option<Timestamp>, String)> = None;
    let mut auth_failed = None;
    let mut result = None;

    for line in stream.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match message["type"].as_str() {
            Some("rate_limit_event") => {
                let info = &message["rate_limit_info"];
                if info["status"] == "rejected" {
                    limited = Some((reset_time(&info["resetsAt"]), limit_detail(info)));
                }
                usage = Some(read_usage(info));
            }
            Some("assistant") => {
                if message["api_error"] == "usage_limit_reached" {
                    let info = &message["api_error_params"]["rate_limit_info"];
                    limited = Some((reset_time(&info["resetsAt"]), limit_detail(info)));
                } else if let Some(error) = message["error"].as_str() {
                    if AUTH_ERRORS.contains(&error) {
                        auth_failed = Some(error.to_owned());
                    } else if WAIT_ERRORS.contains(&error) && limited.is_none() {
                        limited = Some((None, wait_detail(error)));
                    }
                }
            }
            Some("result") => result = Some(message),
            _ => {}
        }
    }

    let outcome = match (result, auth_failed, limited) {
        (Some(result), _, _) if result["subtype"] == "success" && result["is_error"] == false => {
            AgentOutcome::Completed(completion(&result))
        }
        (_, Some(error), _) => AgentOutcome::AuthFailed(error),
        (_, None, Some((resets_at, detail))) => AgentOutcome::Limited { resets_at, detail },
        (Some(result), None, None) => AgentOutcome::Failed(failure_reason(&result)),
        (None, None, None) => AgentOutcome::Failed("the stream ended without a result".to_owned()),
    };
    Summary { outcome, usage }
}

fn completion(result: &Value) -> Completion {
    Completion {
        text: result["result"].as_str().unwrap_or_default().to_owned(),
        structured: result
            .get("structured_output")
            .filter(|v| !v.is_null())
            .cloned(),
        turns: result["num_turns"].as_u64().unwrap_or(0),
        cost_usd: result["total_cost_usd"].as_f64().unwrap_or(0.0),
        usage: result["usage"].clone(),
        permission_denials: result["permission_denials"].as_array().map_or(0, Vec::len),
    }
}

fn failure_reason(result: &Value) -> String {
    let subtype = result["subtype"].as_str().unwrap_or("unknown");
    match result["result"].as_str() {
        Some(text) if !text.is_empty() => format!("{subtype}: {}", first_line(text)),
        _ => subtype.to_owned(),
    }
}

fn first_line(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((cut, _)) => &line[..cut],
        None => line,
    }
}

fn reset_time(value: &Value) -> Option<Timestamp> {
    value
        .as_i64()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
}

fn limit_detail(info: &Value) -> String {
    match info["rateLimitType"].as_str() {
        Some(kind) => format!("the Claude usage limit was reached ({kind})"),
        None => "the Claude usage limit was reached".to_owned(),
    }
}

fn wait_detail(error: &str) -> String {
    match error {
        "rate_limit" => "Claude is rate-limiting requests".to_owned(),
        "overloaded" => "Claude is overloaded".to_owned(),
        "server_error" => "Claude had a server error".to_owned(),
        other => format!("Claude reported {other}"),
    }
}

fn read_usage(info: &Value) -> Usage {
    let window = |name: &str| {
        let window = &info["unifiedWindows"][name];
        window["utilization"].as_f64().map(|utilization| Window {
            utilization,
            resets_at: reset_time(&window["resetsAt"]),
        })
    };
    Usage {
        status: info["status"].as_str().unwrap_or("unknown").to_owned(),
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = include_str!("../tests/fixtures/stream-json/basic.jsonl");
    const PLAN: &str =
        include_str!("../tests/fixtures/stream-json/read-stage-structured-output.jsonl");
    const WRITE: &str =
        include_str!("../tests/fixtures/stream-json/write-stage-readonly-git.jsonl");

    fn completed(stream: &str) -> Completion {
        match summarise(stream).outcome {
            AgentOutcome::Completed(completion) => completion,
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_basic_run() {
        let summary = summarise(BASIC);
        let AgentOutcome::Completed(completion) = &summary.outcome else {
            panic!("{:?}", summary.outcome);
        };
        assert_eq!(completion.text, "pong");
        assert_eq!(completion.structured, None);
        assert_eq!(completion.turns, 1);
        assert!(completion.cost_usd > 0.0);
        let usage = summary.usage.unwrap();
        assert_eq!(usage.status, "allowed");
        assert_eq!(usage.five_hour.unwrap().utilization, 0.1);
        assert_eq!(
            usage.seven_day.unwrap().resets_at,
            Some(Timestamp::from_second(1_793_300_000).unwrap())
        );
    }

    #[test]
    fn reads_structured_output_from_a_read_stage() {
        let plan = completed(PLAN).structured.unwrap();
        assert_eq!(plan["outcome"], "ready");
        assert!(
            plan["steps"]
                .as_array()
                .is_some_and(|steps| !steps.is_empty())
        );
    }

    #[test]
    fn a_failed_commit_inside_the_run_is_still_a_completed_run() {
        let completion = completed(WRITE);
        assert_eq!(completion.permission_denials, 0);
        assert!(completion.text.contains("Read-only file system"));
    }

    #[test]
    fn usage_limit_error_carries_its_reset_time() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[]},"error":"rate_limit","is_api_error_message":true,"api_error":"usage_limit_reached","api_error_params":{"rate_limit_info":{"status":"rejected","resetsAt":1792800000,"rateLimitType":"five_hour","isUsingOverage":false}}}
{"type":"result","subtype":"success","is_error":true,"result":"You've hit your usage limit"}"#;
        assert_eq!(
            summarise(stream).outcome,
            AgentOutcome::Limited {
                resets_at: Some(Timestamp::from_second(1_792_800_000).unwrap()),
                detail: "the Claude usage limit was reached (five_hour)".into(),
            }
        );
    }

    #[test]
    fn a_rejected_rate_limit_event_also_counts_as_limited() {
        let stream = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1792800000,"rateLimitType":"seven_day"}}"#;
        let summary = summarise(stream);
        assert!(matches!(
            summary.outcome,
            AgentOutcome::Limited { resets_at: Some(_), ref detail } if detail == "the Claude usage limit was reached (seven_day)"
        ));
        assert_eq!(summary.usage.unwrap().status, "rejected");
    }

    #[test]
    fn throttles_without_a_reset_time_wait_too() {
        let stream = r#"{"type":"assistant","message":{"content":[]},"error":"overloaded","is_api_error_message":true}
{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        assert_eq!(
            summarise(stream).outcome,
            AgentOutcome::Limited {
                resets_at: None,
                detail: "Claude is overloaded".into()
            }
        );
    }

    #[test]
    fn credential_problems_are_reported_as_such() {
        let stream = r#"{"type":"assistant","message":{"content":[]},"error":"authentication_failed","is_api_error_message":true}
{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key"}"#;
        assert_eq!(
            summarise(stream).outcome,
            AgentOutcome::AuthFailed("authentication_failed".into())
        );
    }

    #[test]
    fn errors_and_truncated_streams_fail() {
        let max_turns =
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":30}"#;
        assert_eq!(
            summarise(max_turns).outcome,
            AgentOutcome::Failed("error_max_turns".into())
        );
        let cut_off = BASIC.lines().take(2).collect::<Vec<_>>().join("\n");
        assert_eq!(
            summarise(&cut_off).outcome,
            AgentOutcome::Failed("the stream ended without a result".into())
        );
        assert!(matches!(
            summarise("not json\n\n{]").outcome,
            AgentOutcome::Failed(_)
        ));
    }

    #[test]
    fn a_success_wins_over_a_warning_seen_mid_run() {
        let warned = format!(
            "{}\n{BASIC}",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","utilization":0.9}}"#
        );
        assert!(matches!(
            summarise(&warned).outcome,
            AgentOutcome::Completed(_)
        ));
    }

    #[test]
    fn read_stages_get_only_read_tools() {
        let argv = command_line(&Invocation {
            mode: AgentMode::Read,
            prompt: "--dangerously-skip-permissions",
            model: "claude-sonnet-5-5",
            schema: Some(Schema::Plan),
            max_turns: 30,
        })
        .unwrap();
        let tools = argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(argv[tools + 1], READ_TOOLS);
        assert!(argv.contains(&"--restricted".to_owned()));
        assert!(
            !argv
                .iter()
                .any(|a| a == "--permission-mode" || a == "--bare")
        );
        // The prompt is last, after `--`, so it can never become a flag.
        assert_eq!(
            &argv[argv.len() - 2..],
            ["--", "--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn write_stages_accept_edits_and_allow_bash() {
        let argv = command_line(&Invocation {
            mode: AgentMode::Write,
            prompt: "fix it",
            model: "claude-sonnet-5-5",
            schema: None,
            max_turns: 60,
        })
        .unwrap()
        .join(" ");
        assert!(argv.contains("--tools Read,Grep,Glob,Edit,Write,Bash --permission-mode acceptEdits --allowedTools Bash"));
        assert!(!argv.contains("--json-schema"));
    }

    #[test]
    fn oversized_prompts_are_refused() {
        let prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        let invocation = Invocation {
            mode: AgentMode::Read,
            prompt: &prompt,
            model: "m",
            schema: None,
            max_turns: 1,
        };
        assert!(command_line(&invocation).is_err());
    }

    #[test]
    fn schema_outcomes_match_the_workflow_labels() {
        for schema in [Schema::Plan, Schema::Review] {
            let json: Value = serde_json::from_str(json_schema(schema)).unwrap();
            let labels: Vec<&str> = json["properties"]["outcome"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(labels, schema.outcomes());
            assert_eq!(json["required"][0], "outcome");
        }
    }
}
