//! The on-disk layout of one run, under `<state_dir>/runs/<id>/`.
//!
//! ```text
//! repo.git/        git directory, mounted read-only into containers
//! work/            the worktree the agent edits
//! factory/         .factory/ snapshot from the base commit
//! verify/<n>/      a clean export of the commit under test, per attempt
//! attempts/<n>/    prompt, stream, stderr and diff for each stage attempt
//! scratch/         host-only temporary files
//! ```
//!
//! A published run's clone and worktree go as soon as it's pushed, since
//! the branch now holds the work. Everything else stays until the run is
//! older than the operator's `run_retention`, for looking into why a run
//! ended as it did.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One dsf process per state directory, so no two ever resume, or clean
/// up after, the same run. The lock goes when the process does, however
/// it ends.
pub struct StateLock {
    _file: File,
}

impl StateLock {
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        match Self::try_acquire(state_dir)? {
            Some(lock) => Ok(lock),
            None => bail!("another dsf is using {}", state_dir.display()),
        }
    }

    /// `None` if another dsf holds the lock.
    pub fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        let path = state_dir.join("dsf.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunDir {
    root: PathBuf,
}

impl RunDir {
    /// Creates a fresh run directory, readable only by the factory user.
    /// Fails if it already exists, so a run never inherits another's files.
    pub fn create(state_dir: &Path, run_id: i64) -> Result<Self> {
        let runs = state_dir.join("runs");
        private_dir(&runs, true)?;
        let root = runs.join(run_id.to_string());
        private_dir(&root, false)?;
        for sub in ["attempts", "verify", "scratch"] {
            private_dir(&root.join(sub), false)?;
        }
        Ok(Self { root })
    }

    /// An existing run's directory, for resuming it.
    pub fn open(state_dir: &Path, run_id: i64) -> Result<Self> {
        let root = state_dir.join("runs").join(run_id.to_string());
        anyhow::ensure!(root.is_dir(), "run directory {} is missing", root.display());
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> PathBuf {
        self.root.join("repo.git")
    }

    pub fn work_tree(&self) -> PathBuf {
        self.root.join("work")
    }

    pub fn factory_snapshot(&self) -> PathBuf {
        self.root.join("factory")
    }

    /// The repository's instructions for agents, as read from the base
    /// commit when the run started.
    pub fn instructions(&self) -> PathBuf {
        self.root.join("instructions.md")
    }

    pub fn scratch(&self) -> PathBuf {
        self.root.join("scratch")
    }

    /// A new, empty directory for exporting the commit an attempt verifies.
    pub fn verify_tree(&self, attempt_id: i64) -> Result<PathBuf> {
        let path = self.root.join("verify").join(attempt_id.to_string());
        private_dir(&path, false)?;
        Ok(path)
    }

    /// A new, empty directory for an attempt's artefacts.
    pub fn attempt(&self, attempt_id: i64) -> Result<PathBuf> {
        let path = self.root.join("attempts").join(attempt_id.to_string());
        private_dir(&path, false)?;
        Ok(path)
    }

    /// Removes the clone, the worktree and any verify trees, keeping the
    /// attempts' logs.
    pub fn remove_checkout(&self) -> Result<()> {
        for sub in ["repo.git", "work", "factory", "verify", "scratch"] {
            remove_tree(&self.root.join(sub))?;
        }
        Ok(())
    }

    /// Removes a run's directory, if it has one.
    pub fn discard(state_dir: &Path, run_id: i64) -> Result<()> {
        remove_tree(&state_dir.join("runs").join(run_id.to_string()))
    }

    /// The ids of the runs that have a directory.
    pub fn list(state_dir: &Path) -> Result<Vec<i64>> {
        let runs = state_dir.join("runs");
        let entries = match std::fs::read_dir(&runs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", runs.display()));
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(id) = entry.file_name().to_str().and_then(|n| n.parse().ok())
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
}

/// Bytes free to an unprivileged user on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Result<u64> {
    let stats = rustix::fs::statvfs(path)
        .with_context(|| format!("checking free space at {}", path.display()))?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// A size as a person reads it: whole gigabytes, or megabytes below one.
pub fn size_text(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        format!("{} GB", bytes / GB)
    } else {
        format!("{} MB", bytes / 1_000_000)
    }
}

/// Removes `path` and everything under it, never following a symlink.
/// When that fails, it tries again: once after making directories
/// writable, since an agent may have left some without write permission,
/// then after short pauses, since Docker Desktop lets go of a container's
/// mount points a moment after the container is removed.
fn remove_tree(path: &Path) -> Result<()> {
    const PAUSES: u32 = 10;
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    let mut failures = 0;
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if failures == 0 => make_writable(path)?,
            Err(_) if failures <= PAUSES => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
        }
        failures += 1;
    }
}

fn make_writable(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // symlink_metadata never follows a link, so neither does this.
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn private_dir(path: &Path, allow_existing: bool) -> Result<()> {
    let result = std::fs::DirBuilder::new()
        .recursive(allow_existing)
        .mode(0o700)
        .create(path);
    result.with_context(|| format!("creating {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_process_holds_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let held = StateLock::acquire(dir.path()).unwrap();
        let error = StateLock::acquire(dir.path()).err().unwrap().to_string();
        assert!(error.contains("another dsf"), "{error}");
        assert!(StateLock::try_acquire(dir.path()).unwrap().is_none());
        drop(held);
        StateLock::acquire(dir.path()).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_a_private_layout_and_refuses_to_reuse_it() {
        let state = tempfile::tempdir().unwrap();
        let run = RunDir::create(state.path(), 7).unwrap();
        assert_eq!(run.root(), state.path().join("runs/7"));
        let mode = std::fs::metadata(run.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(run.scratch().is_dir());
        assert!(RunDir::create(state.path(), 7).is_err());
        assert!(RunDir::create(state.path(), 8).is_ok());
        assert_eq!(RunDir::open(state.path(), 7).unwrap().root(), run.root());
        assert!(RunDir::open(state.path(), 9).is_err());
    }

    #[test]
    fn removal_keeps_the_logs_then_takes_the_rest() {
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), "x").unwrap();
        let run = RunDir::create(state.path(), 4).unwrap();
        run.attempt(1).unwrap();
        // What an agent might leave: a read-only directory, and a link out.
        let stuck = run.work_tree().join("vendor/locked");
        std::fs::create_dir_all(&stuck).unwrap();
        std::fs::write(stuck.join("file"), "x").unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::os::unix::fs::symlink(outside.path(), run.work_tree().join("out")).unwrap();

        run.remove_checkout().unwrap();
        assert!(!run.work_tree().exists());
        assert!(run.root().join("attempts/1").is_dir());
        assert_eq!(RunDir::list(state.path()).unwrap(), [4]);

        RunDir::discard(state.path(), 4).unwrap();
        assert!(!run.root().exists());
        assert!(outside.path().join("keep").exists(), "followed a symlink");
        assert_eq!(RunDir::list(state.path()).unwrap(), Vec::<i64>::new());
        RunDir::discard(state.path(), 4).unwrap();
    }

    #[test]
    fn free_space_is_measured_and_shown_simply() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert!(free_space(&dir.path().join("missing")).is_err());
        assert_eq!(size_text(52_400_000_000), "52 GB");
        assert_eq!(size_text(640_000_000), "640 MB");
    }

    #[test]
    fn attempt_and_verify_directories_are_fresh_each_time() {
        let state = tempfile::tempdir().unwrap();
        let run = RunDir::create(state.path(), 1).unwrap();
        let attempt = run.attempt(3).unwrap();
        assert!(attempt.ends_with("attempts/3"));
        assert!(run.attempt(3).is_err());
        assert!(run.verify_tree(3).unwrap().ends_with("verify/3"));
    }
}
