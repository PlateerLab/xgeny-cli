use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub trait RunLease {
    fn run_id(&self) -> &str;
}

pub struct LocalRunLease {
    run_id: String,
    path: PathBuf,
    file: File,
}

impl LocalRunLease {
    /// Acquire the canonical advisory lock for one local Run.
    ///
    /// The caller must derive `path` from the same canonical Run directory as its store.
    /// Keeping this value alive holds the lock across event commits and the external call.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::AlreadyHeld`] when another runtime owns the lock, or an I/O error
    /// when the lock file cannot be opened or locked.
    pub fn try_acquire(
        run_id: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Self, LeaseError> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|_| LeaseError::Io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LeaseError::AlreadyHeld);
            }
            Err(TryLockError::Error(_)) => {
                return Err(LeaseError::Io);
            }
        }
        Ok(Self {
            run_id: run_id.into(),
            path,
            file,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for LocalRunLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalRunLease")
            .field("run_id", &self.run_id)
            .field("path", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RunLease for LocalRunLease {
    fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl Drop for LocalRunLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    AlreadyHeld,
    Io,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHeld => formatter.write_str("local Run lease is already held"),
            Self::Io => formatter.write_str("local Run lease is unavailable"),
        }
    }
}

impl std::error::Error for LeaseError {}
