//! Every git command the host runs, and the checks that stop a run's
//! worktree from turning into code execution on the host.
//!
//! The agent controls the worktree, so the host treats it as hostile:
//! - git runs with a fixed argument list, a cleared environment, and hooks,
//!   fsmonitor, signing and credential helpers switched off;
//! - the git directory lives outside the worktree and is only ever mounted
//!   read-only into containers, so its config, hooks and refs stay the
//!   host's;
//! - nested repositories are refused before `git add` can look inside them;
//! - symlinks are never followed: trees are inspected through git objects.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use secrecy::{ExposeSecret, SecretString};
use tokio::process::Command;

const LOCAL_TIMEOUT: Duration = Duration::from_mins(2);
const NETWORK_TIMEOUT: Duration = Duration::from_mins(10);
/// Refuse to walk worktrees bigger than this rather than stall the run.
const MAX_WORKTREE_ENTRIES: usize = 2_000_000;

/// The whole environment of every host git process.
const BASE_ENV: [(&str, &str); 7] = [
    ("PATH", "/usr/bin:/bin"),
    ("HOME", "/nonexistent"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("LC_ALL", "C"),
];

/// Config every host git command runs with, whatever the repo says.
const HARDENING: [&str; 20] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgsign=false",
    "-c",
    "credential.helper=",
    "-c",
    "core.askPass=",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "transfer.fsckObjects=true",
    "-c",
    "submodule.recurse=false",
];

const AUTHOR: [&str; 4] = [
    "-c",
    "user.name=dsf",
    "-c",
    "user.email=dsf@localhost.invalid",
];

/// A full commit id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha(String);

impl Sha {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let hex = text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if hex && (text.len() == 40 || text.len() == 64) {
            Ok(Self(text.to_owned()))
        } else {
            bail!("`{text}` is not a full commit id")
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Something in a worktree or tree that the host refuses to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A `.git` file or directory below the top level: `git add` would read it.
    NestedRepository(String),
    /// A submodule entry in a tree.
    Gitlink(String),
    /// A `.gitmodules` file at the top level.
    Gitmodules,
    /// A symlink whose target is absolute or climbs out of the repository.
    EscapingSymlink { path: String, target: String },
    /// A symlink inside `.factory/`, which could point the gate at
    /// agent-editable code.
    FactorySymlink(String),
    /// A path git itself should never have produced.
    UnsafePath(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedRepository(path) => write!(f, "nested git repository at `{path}`"),
            Self::Gitlink(path) => write!(f, "submodule at `{path}`"),
            Self::Gitmodules => f.write_str("a .gitmodules file"),
            Self::EscapingSymlink { path, target } => {
                write!(
                    f,
                    "symlink `{path}` points outside the repository (`{target}`)"
                )
            }
            Self::FactorySymlink(path) => write!(f, "symlink in .factory/ at `{path}`"),
            Self::UnsafePath(path) => write!(f, "unsafe path `{path}`"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The worktree or tree contains something dsf won't touch. The run
    /// should be blocked; retrying can't help.
    #[error("refused: {}", list(.0))]
    Refused(Vec<Refusal>),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

fn list(refusals: &[Refusal]) -> String {
    refusals
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub type Result<T> = std::result::Result<T, GitError>;

/// The git executable, located once so commands can run with a minimal PATH.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
}

impl Git {
    pub fn locate() -> anyhow::Result<Self> {
        let path = std::env::var_os("PATH").context("PATH is not set")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("git"))
            .find(|candidate| candidate.is_file())
            .map(|program| Self { program })
            .context("git was not found on PATH")
    }

    pub fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.env_clear().envs(BASE_ENV);
        command
    }
}

/// A GitHub token, sent as an HTTP header that git attaches only to
/// requests under one repository's URL. It is passed through `GIT_CONFIG_*`
/// environment variables, so it is never written to disk.
pub struct Auth {
    token: SecretString,
}

impl Auth {
    pub fn new(token: SecretString) -> Self {
        Self { token }
    }

    /// The token and the header carrying it: what the masker must hide.
    pub fn secrets(&self) -> [SecretString; 2] {
        [self.token.clone(), self.header_value()]
    }

    /// The `Authorization` header value, which the masker must also hide.
    pub fn header_value(&self) -> SecretString {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{}", self.token.expose_secret()));
        SecretString::from(format!("basic {basic}"))
    }

    fn env(&self, url: &str) -> [(String, String); 3] {
        [
            ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
            (
                "GIT_CONFIG_KEY_0".to_owned(),
                format!("http.{url}.extraheader"),
            ),
            (
                "GIT_CONFIG_VALUE_0".to_owned(),
                format!("AUTHORIZATION: {}", self.header_value().expose_secret()),
            ),
        ]
    }
}

/// Only plain HTTPS and local remotes. Anything else could invoke a
/// transport helper or ssh.
fn check_remote(url: &str) -> anyhow::Result<()> {
    let ok = url.starts_with("https://") || url.starts_with("file:///") || url.starts_with('/');
    if ok && !url.contains(char::is_whitespace) {
        Ok(())
    } else {
        bail!("refusing remote `{url}`: only https:// and local paths are allowed")
    }
}

/// A run's repository: a git directory and a separate worktree.
#[derive(Debug, Clone)]
pub struct HostGit {
    git: Git,
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl HostGit {
    /// Clones `url` with the git directory at `git_dir`, apart from the
    /// worktree at `work_tree`, and returns the checked-out commit.
    pub async fn clone(
        git: Git,
        url: &str,
        auth: Option<&Auth>,
        git_dir: &Path,
        work_tree: &Path,
    ) -> Result<(Self, Sha)> {
        check_remote(url)?;
        let mut command = git.command();
        command
            .args(HARDENING)
            .args(["clone", "--quiet", "--no-recurse-submodules", "--template="])
            .arg("--separate-git-dir")
            .arg(git_dir)
            .arg("--")
            .arg(url)
            .arg(work_tree);
        if let Some(auth) = auth {
            command.envs(auth.env(url));
        }
        run(command, NETWORK_TIMEOUT, "clone").await?;

        // Clone leaves a `.git` file pointing at the git directory. Replace
        // it with the empty directory that containers mount it over.
        let gitfile = work_tree.join(".git");
        tokio::fs::remove_file(&gitfile)
            .await
            .with_context(|| format!("removing {}", gitfile.display()))?;
        tokio::fs::create_dir(&gitfile)
            .await
            .with_context(|| format!("creating {}", gitfile.display()))?;

        let repo = Self {
            git,
            git_dir: git_dir.to_owned(),
            work_tree: work_tree.to_owned(),
        };
        let head = repo.head().await?;
        Ok((repo, head))
    }

    /// An existing run's repository, for resuming it.
    pub fn open(git: Git, git_dir: &Path, work_tree: &Path) -> Self {
        Self {
            git,
            git_dir: git_dir.to_owned(),
            work_tree: work_tree.to_owned(),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    pub async fn head(&self) -> Result<Sha> {
        let out = self
            .output(&["rev-parse", "--verify", "HEAD^{commit}"], "rev-parse")
            .await?;
        Ok(Sha::parse(&String::from_utf8_lossy(&out))?)
    }

    /// Commits everything in the worktree if anything changed, returning
    /// the new commit. Nested repositories are refused before `git add`
    /// runs, and the resulting tree is checked before it is accepted.
    pub async fn checkpoint(&self, message: &str) -> Result<Option<Sha>> {
        let work_tree = self.work_tree.clone();
        let nested = tokio::task::spawn_blocking(move || nested_repositories(&work_tree))
            .await
            .map_err(|error| anyhow!("worktree scan panicked: {error}"))??;
        if !nested.is_empty() {
            return Err(GitError::Refused(nested));
        }

        self.output(&["add", "--all", "--", "."], "add").await?;
        if self
            .status(&["diff", "--cached", "--quiet", "--no-ext-diff"])
            .await?
            == 0
        {
            return Ok(None);
        }
        let mut command = self.command();
        command.args(AUTHOR).args([
            "commit",
            "--quiet",
            "--no-verify",
            "--no-edit",
            "-m",
            message,
        ]);
        run(command, LOCAL_TIMEOUT, "commit").await?;

        let head = self.head().await?;
        let refusals = self.tree_refusals(&head).await?;
        if refusals.is_empty() {
            Ok(Some(head))
        } else {
            Err(GitError::Refused(refusals))
        }
    }

    /// Paths that differ between two commits, including both sides of a
    /// rename, so moving a protected file away still counts as touching it.
    pub async fn changed_paths(&self, from: &Sha, to: &Sha) -> Result<Vec<String>> {
        let out = self
            .output(
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    from.as_str(),
                    to.as_str(),
                    "--",
                ],
                "diff",
            )
            .await?;
        Ok(split_nul(&out))
    }

    /// The full patch between two commits, as text, for the pre-push secret
    /// scan and the run's artefacts.
    pub async fn diff(&self, from: &Sha, to: &Sha) -> Result<String> {
        let out = self
            .output(
                &[
                    "diff",
                    "--text",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    from.as_str(),
                    to.as_str(),
                    "--",
                ],
                "diff",
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub async fn diff_stat(&self, from: &Sha, to: &Sha) -> Result<String> {
        let out = self
            .output(
                &[
                    "diff",
                    "--stat=120",
                    "--no-color",
                    "--no-ext-diff",
                    from.as_str(),
                    to.as_str(),
                    "--",
                ],
                "diff",
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Everything in a commit's tree that dsf refuses: submodules, a
    /// top-level `.gitmodules`, and symlinks that leave the repository.
    /// Symlink targets are read from git objects, never followed.
    pub async fn tree_refusals(&self, sha: &Sha) -> Result<Vec<Refusal>> {
        let mut refusals = Vec::new();
        for entry in self.ls_tree(sha, None).await? {
            if !is_safe_path(&entry.path) {
                refusals.push(Refusal::UnsafePath(entry.path));
                continue;
            }
            if entry.mode == "160000" {
                refusals.push(Refusal::Gitlink(entry.path));
            } else if entry.path.eq_ignore_ascii_case(".gitmodules") {
                refusals.push(Refusal::Gitmodules);
            } else if entry.mode == "120000" {
                let target = String::from_utf8_lossy(&self.blob(&entry.oid).await?).into_owned();
                if symlink_escapes(&entry.path, &target) {
                    refusals.push(Refusal::EscapingSymlink {
                        path: entry.path,
                        target,
                    });
                }
            }
        }
        Ok(refusals)
    }

    /// The repository's instructions for agents as they are in `sha`, in
    /// the order Claude Code reads them: `AGENTS.md`, `CLAUDE.md`,
    /// `.claude/CLAUDE.md`, then `.claude/rules/*.md`. Only regular files
    /// count; a symlink is never followed.
    pub async fn instruction_files(&self, sha: &Sha) -> Result<Vec<(String, Vec<u8>)>> {
        const TOP: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".claude/CLAUDE.md"];
        let mut wanted: Vec<TreeEntry> = self
            .ls_tree(sha, None)
            .await?
            .into_iter()
            .filter(|entry| matches!(entry.mode.as_str(), "100644" | "100755"))
            .filter(|entry| {
                TOP.contains(&entry.path.as_str())
                    || (entry.path.starts_with(".claude/rules/")
                        && Path::new(&entry.path)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("md")))
            })
            .collect();
        let rank = |path: &str| TOP.iter().position(|top| *top == path).unwrap_or(TOP.len());
        wanted.sort_by(|a, b| {
            rank(&a.path)
                .cmp(&rank(&b.path))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut files = Vec::new();
        for entry in wanted {
            let contents = self.blob(&entry.oid).await?;
            files.push((entry.path, contents));
        }
        Ok(files)
    }

    /// Writes `.factory/` as it is in `sha` into `dest`, from git objects.
    /// Refuses symlinks and submodules there: either could make the gate
    /// run something the agent can edit.
    pub async fn snapshot_factory(&self, sha: &Sha, dest: &Path) -> Result<()> {
        let entries = self.ls_tree(sha, Some(".factory")).await?;
        let refusals: Vec<Refusal> = entries
            .iter()
            .filter_map(|entry| match entry.mode.as_str() {
                "120000" => Some(Refusal::FactorySymlink(entry.path.clone())),
                "160000" => Some(Refusal::Gitlink(entry.path.clone())),
                _ if !is_safe_path(&entry.path) => Some(Refusal::UnsafePath(entry.path.clone())),
                _ => None,
            })
            .collect();
        if !refusals.is_empty() {
            return Err(GitError::Refused(refusals));
        }
        for entry in entries {
            let contents = self.blob(&entry.oid).await?;
            write_new_file(dest, &entry.path, &contents, entry.mode == "100755").await?;
        }
        Ok(())
    }

    /// Writes the tree of `sha` into the empty directory `dest`, through a
    /// private index so the run's own index is untouched. Verify runs here,
    /// against exactly what would be published.
    pub async fn export(&self, sha: &Sha, dest: &Path, scratch: &Path) -> Result<()> {
        let index = scratch.join("export.index");
        let mut read = self.command();
        read.env("GIT_INDEX_FILE", &index)
            .args(["read-tree", sha.as_str()]);
        run(read, LOCAL_TIMEOUT, "read-tree").await?;
        let mut prefix = OsString::from("--prefix=");
        prefix.push(dest.as_os_str());
        prefix.push("/");
        let mut checkout = self.command();
        checkout
            .env("GIT_INDEX_FILE", &index)
            .args(["checkout-index", "--all", "--force"])
            .arg(prefix);
        run(checkout, LOCAL_TIMEOUT, "checkout-index").await?;
        tokio::fs::remove_file(&index)
            .await
            .with_context(|| format!("removing {}", index.display()))?;
        Ok(())
    }

    /// Discards everything since `sha`, including untracked and ignored
    /// files, so a resumed stage starts from its checkpoint.
    pub async fn reset_to(&self, sha: &Sha) -> Result<()> {
        self.output(&["reset", "--hard", "--quiet", sha.as_str()], "reset")
            .await?;
        self.output(&["clean", "-ffdx", "--quiet"], "clean").await?;
        Ok(())
    }

    /// Pushes `sha` to `branch` at `url`, replacing whatever was there. Only
    /// `factory/` branches can be pushed.
    pub async fn push(
        &self,
        url: &str,
        sha: &Sha,
        branch: &str,
        auth: Option<&Auth>,
    ) -> Result<()> {
        check_remote(url)?;
        let valid_branch = branch.strip_prefix("factory/").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
        if !valid_branch {
            return Err(anyhow!("refusing to push to `{branch}`: only factory/ branches").into());
        }
        let mut command = self.command();
        command
            .args(["push", "--quiet", "--no-verify", "--"])
            .arg(url)
            .arg(format!("+{sha}:refs/heads/{branch}"));
        if let Some(auth) = auth {
            command.envs(auth.env(url));
        }
        run(command, NETWORK_TIMEOUT, "push").await?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = self.git.command();
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(HARDENING)
            .current_dir(&self.work_tree);
        command
    }

    async fn output(&self, args: &[&str], what: &str) -> Result<Vec<u8>> {
        let mut command = self.command();
        command.args(args);
        Ok(run(command, LOCAL_TIMEOUT, what).await?)
    }

    /// Runs a command whose exit code is the answer, e.g. `diff --quiet`.
    async fn status(&self, args: &[&str]) -> Result<i32> {
        let mut command = self.command();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(LOCAL_TIMEOUT, command.output())
            .await
            .context("git timed out")?
            .context("running git")?;
        match output.status.code() {
            Some(code @ (0 | 1)) => Ok(code),
            _ => Err(anyhow!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into()),
        }
    }

    async fn ls_tree(&self, sha: &Sha, path: Option<&str>) -> Result<Vec<TreeEntry>> {
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", sha.as_str(), "--"];
        args.extend(path);
        let out = self.output(&args, "ls-tree").await?;
        out.split(|b| *b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| TreeEntry::parse(record).map_err(GitError::from))
            .collect()
    }

    async fn blob(&self, oid: &str) -> Result<Vec<u8>> {
        self.output(&["cat-file", "blob", oid], "cat-file").await
    }
}

struct TreeEntry {
    mode: String,
    oid: String,
    path: String,
}

impl TreeEntry {
    /// `<mode> SP <type> SP <oid> TAB <path>`
    fn parse(record: &[u8]) -> anyhow::Result<Self> {
        let text = String::from_utf8_lossy(record);
        let (meta, path) = text.split_once('\t').context("malformed ls-tree output")?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some(_kind), Some(oid)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("malformed ls-tree output");
        };
        Ok(Self {
            mode: mode.to_owned(),
            oid: oid.to_owned(),
            path: path.to_owned(),
        })
    }
}

async fn run(mut command: Command, timeout: Duration, what: &str) -> anyhow::Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| format!("git {what} timed out"))?
        .with_context(|| format!("running git {what}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        bail!(
            "git {what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Repo-relative, with no empty, `.`, `..` or `.git` components.
fn is_safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
        })
}

/// Whether a symlink at `path` with `target` resolves, lexically, outside
/// the repository or into its git directory.
fn symlink_escapes(path: &str, target: &str) -> bool {
    if target.starts_with('/') || target.is_empty() {
        return true;
    }
    let mut depth: Vec<&str> = path.split('/').collect();
    depth.pop(); // the link itself
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if depth.pop().is_none() {
                    return true;
                }
            }
            part if part.eq_ignore_ascii_case(".git") => return true,
            part => depth.push(part),
        }
    }
    false
}

/// `.git` entries anywhere below the top level, found without following
/// symlinks.
fn nested_repositories(work_tree: &Path) -> anyhow::Result<Vec<Refusal>> {
    let mut found = Vec::new();
    let mut pending = vec![(work_tree.to_owned(), String::new())];
    let mut seen = 0usize;
    while let Some((dir, prefix)) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            seen += 1;
            if seen > MAX_WORKTREE_ENTRIES {
                bail!("worktree has more than {MAX_WORKTREE_ENTRIES} entries");
            }
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let is_git = name.eq_ignore_ascii_case(".git");
            if is_git && prefix.is_empty() {
                // The mount point for the run's own git directory.
                continue;
            }
            if is_git {
                found.push(Refusal::NestedRepository(relative));
                continue;
            }
            // symlink_metadata never follows a symlink.
            if entry.path().symlink_metadata()?.is_dir() {
                pending.push((entry.path(), relative));
            }
        }
    }
    found.sort_by_key(ToString::to_string);
    Ok(found)
}

/// Creates `root/relative` and any missing parents inside `root`, which the
/// caller created empty, so no component can be a planted symlink.
async fn write_new_file(
    root: &Path,
    relative: &str,
    contents: &[u8],
    executable: bool,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if executable { 0o755 } else { 0o644 })
        .open(&path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents).await?;
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_git_runs_with_exactly_the_base_environment() {
        let repo = HostGit {
            git: Git::at("/usr/bin/git"),
            git_dir: PathBuf::from("/runs/1/repo.git"),
            work_tree: PathBuf::from("/runs/1/work"),
        };
        let command = repo.command();
        let envs: Vec<(String, String)> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value
                        .map(|v| v.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = BASE_ENV
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let mut envs_sorted = envs.clone();
        envs_sorted.sort();
        let mut expected_sorted = expected;
        expected_sorted.sort();
        assert_eq!(envs_sorted, expected_sorted);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"core.hooksPath=/dev/null".to_owned()));
        assert!(args.contains(&"core.fsmonitor=false".to_owned()));
    }

    #[test]
    fn auth_header_is_scoped_to_the_repository_url() {
        let auth = Auth::new(SecretString::from("ghs_example"));
        let env = auth.env("https://github.com/octocat/hello.git");
        assert_eq!(
            env[1].1,
            "http.https://github.com/octocat/hello.git.extraheader"
        );
        // base64("x-access-token:ghs_example")
        assert_eq!(
            env[2].1,
            "AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46Z2hzX2V4YW1wbGU="
        );
    }

    #[test]
    fn only_https_and_local_remotes_are_allowed() {
        for ok in [
            "https://github.com/o/r.git",
            "file:///tmp/r.git",
            "/srv/r.git",
        ] {
            assert!(check_remote(ok).is_ok(), "{ok}");
        }
        for bad in [
            "ext::sh -c touch% /tmp/pwned",
            "git@github.com:o/r.git",
            "ssh://h/r",
            "http://h/r",
            "-u/x",
            "https://h/r q",
        ] {
            assert!(check_remote(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commit_ids_must_be_full_lowercase_hex() {
        assert!(Sha::parse(&"a".repeat(40)).is_ok());
        assert!(Sha::parse(&"0".repeat(64)).is_ok());
        for bad in [
            "HEAD",
            "abc",
            &"A".repeat(40),
            &"g".repeat(40),
            "--output=/tmp/x",
        ] {
            assert!(Sha::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn symlinks_that_leave_the_repository_escape() {
        assert!(!symlink_escapes("docs/link", "../README.md"));
        assert!(!symlink_escapes("a/b/link", "../../c/d"));
        assert!(!symlink_escapes("link", "sub/./file"));
        assert!(symlink_escapes("link", "/etc/passwd"));
        assert!(symlink_escapes("link", "../outside"));
        assert!(symlink_escapes("a/link", "../../outside"));
        assert!(symlink_escapes("link", ".git/config"));
        assert!(symlink_escapes("a/link", "../.GIT/hooks"));
        assert!(symlink_escapes("link", ""));
    }

    #[test]
    fn paths_with_dangerous_components_are_unsafe() {
        assert!(is_safe_path("src/main.rs"));
        for bad in [
            "",
            "/abs",
            "a/../b",
            "./a",
            "a//b",
            ".git/config",
            "sub/.Git/x",
        ] {
            assert!(!is_safe_path(bad), "{bad}");
        }
    }
}
