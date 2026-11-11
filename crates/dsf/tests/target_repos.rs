// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use dsf::prompts::{Context, Issue, Prompts};
use dsf_core::workflow::{StageId, StageKind, Target, Terminal, Workflow};

fn kits_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/target-repos")
}

fn defaults_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../defaults")
}

/// Every kit that ships a workflow.
fn kits() -> Vec<PathBuf> {
    let mut kits: Vec<PathBuf> = std::fs::read_dir(kits_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join(".factory/workflow.toml").is_file())
        .collect();
    kits.sort();
    kits
}

fn load(kit: &Path) -> Workflow {
    let text = std::fs::read_to_string(kit.join(".factory/workflow.toml")).unwrap();
    Workflow::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", kit.display()))
}

fn id(name: &str) -> StageId {
    StageId::new(name).unwrap()
}

#[test]
fn every_kit_workflow_validates_and_its_prompts_render() {
    let kits = kits();
    assert!(
        !kits.is_empty(),
        "no kits found in {}",
        kits_dir().display()
    );

    let protected = [".factory/", ".github/", ".claude/", ".mcp.json"].map(String::from);
    for kit in kits {
        let workflow = load(&kit);
        // Prompts::from_snapshot falls back to the built-in templates, so a
        // typo would render the wrong prompt. A kit may rely on a built-in
        // (the terraform kit does) but the name must be one that exists.
        let prompts = Prompts::from_snapshot(&kit).unwrap();
        for (stage_id, stage) in workflow.stages() {
            let StageKind::Agent { prompt, .. } = &stage.kind else {
                continue;
            };
            let in_kit = kit.join(".factory/prompts").join(prompt).is_file();
            let built_in = defaults_dir().join("prompts").join(prompt).is_file();
            assert!(
                in_kit || built_in,
                "{}: stage {stage_id} names prompt {prompt}, which is neither in the kit nor built in",
                kit.display()
            );
            let context = Context {
                visit: 1,
                issue: Issue {
                    repo: "example/repo",
                    number: 7,
                    title: "Add a thing",
                    body: "Do the thing.".to_owned(),
                    comments: Vec::new(),
                    omitted_comments: 0,
                },
                plan: Some("1. Do it"),
                protected: &protected,
                diff_stat: Some(" a.rs | 1 +"),
                diff: Some("+let a = 1;".to_owned()),
                verify_output: Some("it failed".to_owned()),
                review: Some("fix it"),
            };
            let rendered = prompts
                .render(prompt, &context)
                .unwrap_or_else(|e| panic!("{}: {prompt}: {e:#}", kit.display()));
            assert!(
                !rendered.trim().is_empty(),
                "{}: {prompt} rendered empty",
                kit.display()
            );
        }
    }
}

#[test]
fn roles_kit_routes_and_limits() {
    let kit = kits_dir().join("roles");
    let workflow = load(&kit);
    let stage = |name: &str| workflow.stage(&id(name)).unwrap();
    let route = |name: &str, outcome: &str| stage(name).on[outcome].clone();
    let to = |name: &str| Target::Stage(id(name));

    // The roles kit carries all of its own prompts rather than borrowing
    // the built-in ones.
    for (stage_id, stage) in workflow.stages() {
        if let StageKind::Agent { prompt, .. } = &stage.kind {
            assert!(
                kit.join(".factory/prompts").join(prompt).is_file(),
                "{stage_id}: {prompt} is missing from the kit"
            );
        }
    }

    assert_eq!(workflow.start(), &id("plan"));
    assert_eq!(route("plan", "ready"), to("approve"));
    assert_eq!(route("approve", "approved"), to("implement"));
    assert_eq!(route("implement", "success"), to("verify"));
    assert_eq!(route("verify", "success"), to("scope_review"));
    assert_eq!(route("verify", "failure"), to("fix"));
    assert_eq!(route("scope_review", "approve"), to("quality_review"));
    assert_eq!(route("scope_review", "changes_requested"), to("fix"));
    assert_eq!(
        route("quality_review", "approve"),
        Target::End(Terminal::Publish)
    );
    assert_eq!(route("quality_review", "changes_requested"), to("fix"));
    assert_eq!(route("fix", "success"), to("verify"));

    assert_eq!(stage("verify").max_visits, 5);
    assert_eq!(stage("fix").max_visits, 4);
    for review in ["scope_review", "quality_review"] {
        assert_eq!(stage(review).max_visits, 2, "{review}");
        assert_eq!(stage(review).exhausted, Terminal::Publish, "{review}");
    }
}

#[test]
fn roles_kit_placeholder_verify_fails() {
    let script = kits_dir().join("roles/.factory/verify.sh");
    let status = std::process::Command::new("sh")
        .arg(&script)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}
