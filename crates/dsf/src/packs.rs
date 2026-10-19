//! What agents are told to follow: the repository's own instructions and
//! the operator's standards packs.
//!
//! Agents run Claude Code in restricted mode, which loads no `CLAUDE.md`,
//! rules or skills from the repository or HOME, and no hooks or settings a
//! repository could plant. So dsf delivers instructions itself:
//!
//! - The repository's `AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md` and
//!   `.claude/rules/*.md`, as they are on the run's base commit, plus each
//!   pack's `rules.md`, are appended to the system prompt.
//! - Each pack is a Claude Code plugin, mounted read-only and passed with
//!   `--plugin-dir`, so its skills (through the Skill tool) and hooks reach
//!   the agent, and with `--add-dir`, so the agent may read the reference
//!   pages its skills point to.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// A pack's always-on rules, at its root. The rest of the pack is a plain
/// Claude Code plugin.
pub const RULES_FILE: &str = "rules.md";
/// The plugin manifest Claude Code needs.
pub const MANIFEST: &str = ".claude-plugin/plugin.json";
/// Where packs appear inside agent containers.
pub const CONTAINER_ROOT: &str = "/dsf/packs";
/// Where the system-prompt addition appears inside agent containers.
pub const SYSTEM_PROMPT_PATH: &str = "/dsf/system.md";
/// Repository instructions beyond this are cut.
const REPO_BUDGET: usize = 32 * 1024;
/// Each pack's `rules.md` beyond this is cut: long guidance belongs in its
/// skills, which load only when relevant.
const RULES_BUDGET: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    pub name: String,
    pub dir: PathBuf,
}

impl Pack {
    /// The pack `name` in the operator's `packs_dir`.
    pub fn named(packs_dir: &Path, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            dir: packs_dir.join(name),
        }
    }

    /// A pack from any directory, named after it: for local runs.
    pub fn at(dir: &Path) -> Result<Self> {
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} has no usable name", dir.display()))?;
        Ok(Self {
            name: name.to_owned(),
            dir: dir.to_owned(),
        })
    }

    pub fn container_dir(&self) -> String {
        format!("{CONTAINER_ROOT}/{}", self.name)
    }

    /// Checks the pack is a usable plugin directory.
    pub fn check(&self) -> Result<()> {
        let shown = self.dir.display();
        let metadata = std::fs::symlink_metadata(&self.dir)
            .with_context(|| format!("pack {}: {shown} doesn't exist", self.name))?;
        if !metadata.is_dir() {
            bail!("pack {}: {shown} is not a directory", self.name);
        }
        if !self.dir.join(MANIFEST).is_file() {
            bail!(
                "pack {}: {shown} has no {MANIFEST}, so Claude Code won't load it",
                self.name
            );
        }
        Ok(())
    }

    /// The pack's `rules.md`, if it has one.
    pub fn rules(&self) -> Result<Option<String>> {
        let path = self.dir.join(RULES_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Whether the pack has skills for the Skill tool to load.
    pub fn has_skills(&self) -> bool {
        self.dir.join("skills").is_dir()
    }
}

/// The repository's instruction files as one text, each under its path,
/// within the budget. `base` is the commit they were read from.
pub fn repo_instructions(files: &[(String, Vec<u8>)], base: &str) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut body = String::new();
    for (path, contents) in files {
        let _ = write!(
            body,
            "\n## {path}\n\n{}\n",
            String::from_utf8_lossy(contents).trim_end()
        );
    }
    format!(
        "# Repository instructions\n\n\
         The repository's maintainers wrote these for agents working on it. They are \
         the files below as they are on the default branch at commit {base}. Follow \
         them; where they differ from your own habits, they win.\n{}",
        clip(&body, REPO_BUDGET)
    )
}

/// The text appended to every agent's system prompt: the repository's
/// instructions, then each pack's rules and a pointer to its skills.
pub fn system_prompt(repo_instructions: &str, packs: &[Pack]) -> Result<String> {
    let mut text = repo_instructions.trim_end().to_owned();
    for pack in packs {
        let rules = pack.rules()?;
        if rules.is_none() && !pack.has_skills() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let _ = writeln!(text, "# Standards: {}", pack.name);
        if let Some(rules) = rules {
            let _ = write!(text, "\n{}\n", clip(rules.trim_end(), RULES_BUDGET));
        }
        if pack.has_skills() {
            let _ = write!(
                text,
                "\nThe `{name}` pack's skills are available through the Skill tool, named \
                 `{name}:<skill>`. Load the ones that cover your task before you write or \
                 review code, and follow them.\n",
                name = pack.name
            );
        }
    }
    Ok(text)
}

/// At most `budget` bytes, cut at a character boundary, saying how much
/// was left out.
fn clip(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut cut = budget;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n(Cut here: {} more bytes left out.)",
        &text[..cut],
        text.len() - cut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(dir: &Path, name: &str, rules: Option<&str>, skills: bool) -> Pack {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(root.join(MANIFEST), format!(r#"{{"name": "{name}"}}"#)).unwrap();
        if let Some(rules) = rules {
            std::fs::write(root.join(RULES_FILE), rules).unwrap();
        }
        if skills {
            std::fs::create_dir_all(root.join("skills/guide")).unwrap();
        }
        Pack::named(dir, name)
    }

    #[test]
    fn repository_instructions_say_where_they_came_from() {
        let files = vec![
            ("AGENTS.md".to_owned(), b"Use tabs.\n".to_vec()),
            ("CLAUDE.md".to_owned(), b"Write tests.\n\n".to_vec()),
        ];
        let text = repo_instructions(&files, "abc123");
        assert!(text.starts_with("# Repository instructions\n"), "{text}");
        assert!(text.contains("at commit abc123"), "{text}");
        assert!(
            text.ends_with("\n## AGENTS.md\n\nUse tabs.\n\n## CLAUDE.md\n\nWrite tests.\n"),
            "{text:?}"
        );
        assert_eq!(repo_instructions(&[], "abc123"), "");
    }

    #[test]
    fn long_instructions_are_cut_with_a_note() {
        let files = vec![("CLAUDE.md".to_owned(), "é".repeat(REPO_BUDGET).into_bytes())];
        let text = repo_instructions(&files, "abc");
        assert!(text.len() < REPO_BUDGET + 1024);
        assert!(text.contains("more bytes left out."), "cut note missing");
    }

    #[test]
    fn packs_add_their_rules_and_point_at_their_skills() {
        let dir = tempfile::tempdir().unwrap();
        let both = pack(
            dir.path(),
            "terraform-azure",
            Some("Tag everything.\n"),
            true,
        );
        let rules_only = pack(dir.path(), "house", Some("Be brief.\n"), false);
        let empty = pack(dir.path(), "empty", None, false);
        let text = system_prompt(
            "# Repository instructions\n\n...\n",
            &[both, rules_only, empty],
        )
        .unwrap();
        assert!(text.starts_with("# Repository instructions"), "{text}");
        assert!(
            text.contains("# Standards: terraform-azure\n\nTag everything.\n"),
            "{text}"
        );
        assert!(text.contains("named `terraform-azure:<skill>`"), "{text}");
        assert!(text.contains("# Standards: house\n\nBe brief.\n"), "{text}");
        assert!(!text.contains("house:<skill>"), "{text}");
        assert!(!text.contains("empty"), "{text}");
        assert_eq!(system_prompt("", &[]).unwrap(), "");
    }

    #[test]
    fn a_pack_must_be_a_plugin_directory() {
        let dir = tempfile::tempdir().unwrap();
        let good = pack(dir.path(), "good", None, true);
        good.check().unwrap();
        assert_eq!(good.container_dir(), "/dsf/packs/good");
        let missing = Pack::named(dir.path(), "missing").check().unwrap_err();
        assert!(missing.to_string().contains("doesn't exist"), "{missing}");
        std::fs::create_dir(dir.path().join("bare")).unwrap();
        let bare = Pack::named(dir.path(), "bare").check().unwrap_err();
        assert!(bare.to_string().contains("plugin.json"), "{bare}");
        assert_eq!(Pack::at(&dir.path().join("good")).unwrap().name, "good");
    }
}
