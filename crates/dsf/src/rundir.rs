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
    fn attempt_and_verify_directories_are_fresh_each_time() {
        let state = tempfile::tempdir().unwrap();
        let run = RunDir::create(state.path(), 1).unwrap();
        let attempt = run.attempt(3).unwrap();
        assert!(attempt.ends_with("attempts/3"));
        assert!(run.attempt(3).is_err());
        assert!(run.verify_tree(3).unwrap().ends_with("verify/3"));
    }
}
