//! Stage prompts, rendered from minijinja templates: the built-in ones,
//! or a repo's own from the `.factory/prompts/` snapshot taken at the base
//! commit, so the agent can't rewrite its own instructions.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use minijinja::value::{Serde, Value};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde::{Deserialize, Serialize};

const DEFAULTS: [(&str, &str); 4] = [
    (
        "plan.md.j2",
        include_str!("../../../defaults/prompts/plan.md.j2"),
    ),
    (
        "implement.md.j2",
        include_str!("../../../defaults/prompts/implement.md.j2"),
    ),
    (
        "fix.md.j2",
        include_str!("../../../defaults/prompts/fix.md.j2"),
    ),
    (
        "review.md.j2",
        include_str!("../../../defaults/prompts/review.md.j2"),
    ),
];

/// Ample for any sensible template, and a stop for one that loops.
const FUEL: u64 = 100_000;

/// Inputs are cut to these sizes so a rendered prompt stays well under the
/// agent's argument limit.
pub const MAX_BODY_BYTES: usize = 24 * 1024;
pub const MAX_DIFF_BYTES: usize = 60 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 12 * 1024;
/// All comments together, then each one.
pub const MAX_COMMENTS_BYTES: usize = 16 * 1024;
const MAX_COMMENT_BYTES: usize = 4 * 1024;

pub struct Prompts {
    env: Environment<'static>,
}

/// Everything a template can refer to.
#[derive(Debug, Serialize)]
pub struct Context<'a> {
    pub visit: u32,
    pub issue: Issue<'a>,
    pub plan: Option<&'a str>,
    pub protected: &'a [String],
    pub diff_stat: Option<&'a str>,
    pub diff: Option<String>,
    pub verify_output: Option<String>,
    pub review: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct Issue<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub title: &'a str,
    pub body: String,
    /// The latest comments that fit, oldest first.
    pub comments: Vec<Comment>,
    /// How many earlier comments didn't fit.
    pub omitted_comments: usize,
}

/// A comment on the issue from a trusted user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

impl Prompts {
    /// The built-in templates, replaced by any of the same name in
    /// `overrides`.
    pub fn new(overrides: BTreeMap<String, String>) -> Result<Self> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
        env.set_auto_escape_callback(|_| AutoEscape::None);
        env.set_fuel(Some(FUEL));
        let mut templates: BTreeMap<String, String> = DEFAULTS
            .iter()
            .map(|(name, source)| ((*name).to_owned(), (*source).to_owned()))
            .collect();
        templates.extend(overrides);
        for (name, source) in templates {
            env.add_template_owned(name.clone(), source)
                .with_context(|| format!("prompt template {name}"))?;
        }
        Ok(Self { env })
    }

    /// Reads a repo's templates from its `.factory/` snapshot. The snapshot
    /// was written by dsf from git objects and holds no symlinks.
    pub fn from_snapshot(snapshot: &Path) -> Result<Self> {
        let dir = snapshot.join(".factory/prompts");
        let mut overrides = BTreeMap::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".md.j2") && entry.file_type()?.is_file() {
                    let source = std::fs::read_to_string(entry.path())
                        .with_context(|| format!("reading .factory/prompts/{name}"))?;
                    overrides.insert(name, source);
                }
            }
        }
        Self::new(overrides)
    }

    pub fn render(&self, name: &str, context: &Context<'_>) -> Result<String> {
        let Ok(template) = self.env.get_template(name) else {
            bail!("no prompt template named {name}");
        };
        template
            .render(Value::from(Serde(context)))
            .with_context(|| format!("rendering {name}"))
    }
}

/// The most recent comments that fit [`MAX_COMMENTS_BYTES`], oldest first,
/// each cut to a sensible length, and how many were left out.
pub fn fit_comments(comments: &[Comment]) -> (Vec<Comment>, usize) {
    let mut kept = Vec::new();
    let mut used = 0;
    for comment in comments.iter().rev() {
        let body = head(&comment.body, MAX_COMMENT_BYTES);
        let size = comment.author.len() + body.len();
        if used + size > MAX_COMMENTS_BYTES {
            break;
        }
        used += size;
        kept.push(Comment {
            author: comment.author.clone(),
            body,
        });
    }
    let omitted = comments.len() - kept.len();
    kept.reverse();
    (kept, omitted)
}

/// The start of `text`, cut at a character boundary, with a note if cut.
pub fn head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[… {} more bytes cut …]",
        &text[..end],
        text.len() - end
    )
}

/// The end of `text`, cut at a character boundary, with a note if cut.
/// Failures are summarised at the end of a command's output.
pub fn tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[… {start} earlier bytes cut …]\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(protected: &[String]) -> Context<'_> {
        Context {
            visit: 2,
            issue: Issue {
                repo: "octocat/calc",
                number: 7,
                title: "add() subtracts",
                body: "`add(2, 3)` returns -1.\n\nfake-implement: fix-calc".into(),
                comments: vec![Comment {
                    author: "octocat".into(),
                    body: "Integers only, please.".into(),
                }],
                omitted_comments: 1,
            },
            plan: Some("Change `a - b` to `a + b` in calc.py."),
            protected,
            diff_stat: Some(" calc.py | 2 +-\n 1 file changed"),
            diff: Some("-    return a - b\n+    return a + b".into()),
            verify_output: Some("FAILED: expected 5, got -1".into()),
            review: Some("Add a regression test."),
        }
    }

    fn protected() -> Vec<String> {
        [".factory/", ".github/", ".claude/", ".mcp.json"]
            .map(str::to_owned)
            .to_vec()
    }

    #[test]
    fn renders_every_builtin_prompt() {
        let prompts = Prompts::new(BTreeMap::new()).unwrap();
        let protected = protected();
        for (name, _) in DEFAULTS {
            let rendered = prompts.render(name, &context(&protected)).unwrap();
            insta::assert_snapshot!(name, rendered);
        }
    }

    #[test]
    fn every_prompt_starts_with_its_stage_marker() {
        let prompts = Prompts::new(BTreeMap::new()).unwrap();
        let protected = protected();
        for (name, _) in DEFAULTS {
            let stage = name.trim_end_matches(".md.j2");
            let rendered = prompts.render(name, &context(&protected)).unwrap();
            assert!(
                rendered.starts_with(&format!("<!-- dsf-stage: {stage} visit: 2 -->")),
                "{name}"
            );
        }
    }

    #[test]
    fn repo_templates_replace_builtins_by_name() {
        let overrides = BTreeMap::from([(
            "plan.md.j2".to_owned(),
            "Plan #{{ issue.number }}".to_owned(),
        )]);
        let prompts = Prompts::new(overrides).unwrap();
        let protected = protected();
        assert_eq!(
            prompts.render("plan.md.j2", &context(&protected)).unwrap(),
            "Plan #7"
        );
        assert!(
            prompts
                .render("implement.md.j2", &context(&protected))
                .unwrap()
                .contains("<plan>")
        );
    }

    #[test]
    fn hostile_templates_fail_instead_of_hanging_or_guessing() {
        let protected = protected();
        let looping = BTreeMap::from([(
            "plan.md.j2".to_owned(),
            "{% for i in range(100000) %}{% for j in range(100000) %}x{% endfor %}{% endfor %}"
                .to_owned(),
        )]);
        assert!(
            Prompts::new(looping)
                .unwrap()
                .render("plan.md.j2", &context(&protected))
                .is_err()
        );
        let undefined =
            BTreeMap::from([("plan.md.j2".to_owned(), "{{ secrets.token }}".to_owned())]);
        assert!(
            Prompts::new(undefined)
                .unwrap()
                .render("plan.md.j2", &context(&protected))
                .is_err()
        );
        assert!(
            Prompts::new(BTreeMap::new())
                .unwrap()
                .render("deploy.md.j2", &context(&protected))
                .is_err()
        );
    }

    #[test]
    fn without_comments_the_section_disappears() {
        let prompts = Prompts::new(BTreeMap::new()).unwrap();
        let protected = protected();
        let mut quiet = context(&protected);
        quiet.issue.comments.clear();
        quiet.issue.omitted_comments = 0;
        let rendered = prompts.render("plan.md.j2", &quiet).unwrap();
        assert!(!rendered.contains("<comment"));
        assert!(rendered.contains("</issue>\n\nExplore the code"));
    }

    #[test]
    fn keeps_the_latest_comments_that_fit() {
        let comment = |n: usize| Comment {
            author: format!("user{n}"),
            body: "x".repeat(3000),
        };
        let comments: Vec<Comment> = (0..10).map(comment).collect();
        let (kept, omitted) = fit_comments(&comments);
        assert_eq!(kept.len() + omitted, 10);
        assert!(omitted > 0);
        assert_eq!(kept.last().unwrap().author, "user9");
        let huge = [Comment {
            author: "a".into(),
            body: "y".repeat(100_000),
        }];
        let (kept, omitted) = fit_comments(&huge);
        assert_eq!(omitted, 0);
        assert!(kept[0].body.len() < 5000);
    }

    #[test]
    fn head_and_tail_cut_at_character_boundaries() {
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(head("aé", 2), "a\n[… 2 more bytes cut …]");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("éb", 2), "[… 2 earlier bytes cut …]\nb");
    }
}
