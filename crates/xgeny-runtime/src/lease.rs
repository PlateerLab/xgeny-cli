use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub trait RunLease {
    fn run_id(&self) -> &str;
}

#[derive(Debug)]
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
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LeaseError::Io {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LeaseError::AlreadyHeld { path });
            }
            Err(TryLockError::Error(source)) => {
                return Err(LeaseError::Io { path, source });
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

#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("local Run lease `{path}` is already held", path = .path.display())]
    AlreadyHeld { path: PathBuf },
    #[error("local Run lease `{path}` failed: {source}", path = .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
