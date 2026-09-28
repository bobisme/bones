use crate::error::ErrorCode;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// Advisory lock errors for repository and database files.
#[derive(Debug)]
pub enum LockError {
    Timeout { path: PathBuf, waited: Duration },
    IoError(io::Error),
}

impl From<io::Error> for LockError {
    fn from(err: io::Error) -> Self {
        Self::IoError(err)
    }
}

impl LockError {
    /// Machine-readable code associated with this lock error.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Timeout { .. } => ErrorCode::LockContention,
            Self::IoError(_) => ErrorCode::EventFileWriteFailed,
        }
    }

    /// Optional remediation hint for operators and agents.
    #[must_use]
    pub const fn hint(&self) -> Option<&'static str> {
        self.code().hint()
    }
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { path, waited } => {
                write!(
                    f,
                    "{}: lock timed out after {:?} at {}",
                    self.code().code(),
                    waited,
                    path.display()
                )
            }
            Self::IoError(err) => write!(f, "{}: {}", self.code().code(), err),
        }
    }
}

impl std::error::Error for LockError {}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

#[derive(Debug)]
struct FileGuard {
    file: File,
    path: PathBuf,
}

/// First pause between two lock attempts. It doubles up to
/// [`MAX_RETRY_PAUSE`].
const MIN_RETRY_PAUSE: Duration = Duration::from_millis(1);
/// Longest pause between two lock attempts, before jitter.
const MAX_RETRY_PAUSE: Duration = Duration::from_millis(10);

/// The gate file for a gated lock at `path`: `<path>.gate`.
fn gate_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".gate");
    PathBuf::from(name)
}

/// A pause of 75-125% of `base`, so that waiters do not poll in step.
fn jittered(base: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    // Each `RandomState` has fresh random keys, so this is a cheap random
    // number without a dependency.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(base.subsec_nanos());
    let permille = 750 + hasher.finish() % 501;
    base * u32::try_from(permille).unwrap_or(1000) / 1000
}

impl FileGuard {
    fn acquire(path: &Path, timeout: Duration, kind: LockKind) -> Result<Self, LockError> {
        Self::acquire_until(path, Instant::now(), timeout, kind)
    }

    /// Acquire a lock at `path` through its gate file (see [`gate_path`]).
    ///
    /// Plain polling is not fair. A process that releases the lock and at
    /// once takes it again wins nearly every time, and a waiter gets in only
    /// if a poll lands in the short gap between them (cr-1kqdjk saw a 45 s
    /// wait). Here every acquirer first takes the gate, exclusive, then the
    /// lock, and then releases the gate. The gate holder is the only process
    /// that can take a free lock, so it gets the lock at its next poll after
    /// the holder releases it. The previous holder must pass the gate to take
    /// the lock again, so it waits behind the gate holder.
    ///
    /// All waiters poll the gate the same way, with jittered pauses, so no
    /// waiter has a built-in advantage. The gate is only a queue position:
    /// the lock itself still gives the mutual exclusion. A process that
    /// takes the lock without the gate (an older binary) is still excluded,
    /// it only skips the queue.
    ///
    /// The gate is released on every return path, also on a timeout, and
    /// the OS releases both locks if the process dies.
    fn acquire_gated(path: &Path, timeout: Duration, kind: LockKind) -> Result<Self, LockError> {
        let start = Instant::now();
        let gate = Self::acquire_until(&gate_path(path), start, timeout, LockKind::Exclusive)
            .map_err(|err| match err {
                LockError::Timeout { waited, .. } => LockError::Timeout {
                    path: path.to_path_buf(),
                    waited,
                },
                other @ LockError::IoError(_) => other,
            })?;
        let lock = Self::acquire_until(path, start, timeout, kind);
        gate.release();
        lock
    }

    fn acquire_until(
        path: &Path,
        start: Instant,
        timeout: Duration,
        kind: LockKind,
    ) -> Result<Self, LockError> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lock path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        let mut pause = MIN_RETRY_PAUSE;
        loop {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)?;

            let locked = match kind {
                LockKind::Shared => file.try_lock_shared().is_ok(),
                LockKind::Exclusive => file.try_lock_exclusive().is_ok(),
            };

            if locked {
                return Ok(Self {
                    file,
                    path: path.to_path_buf(),
                });
            }

            let waited = start.elapsed();
            if waited >= timeout {
                return Err(LockError::Timeout {
                    path: path.to_path_buf(),
                    waited,
                });
            }

            thread::sleep(jittered(pause).min(timeout.saturating_sub(waited)));
            pause = (pause * 2).min(MAX_RETRY_PAUSE);
        }
    }

    fn release(self) {
        let _ = self.file.unlock();
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// RAII guard for repository-wide exclusive lock used during writes.
#[derive(Debug)]
pub struct ShardLock {
    guard: FileGuard,
}

impl ShardLock {
    /// Acquire an exclusive advisory lock on the lock path.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Timeout`] if the lock cannot be acquired within
    /// the given timeout, or [`LockError::IoError`] on I/O failure.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, LockError> {
        Ok(Self {
            guard: FileGuard::acquire(path, timeout, LockKind::Exclusive)?,
        })
    }

    /// Explicitly release the lock. Release also happens automatically on drop.
    pub fn release(self) {
        self.guard.release();
    }

    /// Return the lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.guard.path()
    }
}

/// RAII guard for shared projection read lock.
pub struct DbReadLock {
    guard: FileGuard,
}

impl DbReadLock {
    /// Acquire a shared advisory lock on the projection DB path.
    ///
    /// The acquire queues through the gate file `<path>.gate`, so a waiting
    /// writer is not starved by readers, and waiters take turns.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Timeout`] if the lock cannot be acquired within
    /// the given timeout, or [`LockError::IoError`] on I/O failure.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, LockError> {
        Ok(Self {
            guard: FileGuard::acquire_gated(path, timeout, LockKind::Shared)?,
        })
    }

    /// Explicitly release the lock. Release also happens automatically on drop.
    pub fn release(self) {
        self.guard.release();
    }

    /// Return the lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.guard.path()
    }
}

/// RAII guard for exclusive projection write lock.
pub struct DbWriteLock {
    guard: FileGuard,
}

impl DbWriteLock {
    /// Acquire an exclusive advisory lock on the projection DB path.
    ///
    /// The acquire queues through the gate file `<path>.gate`, so a process
    /// that releases the lock and takes it again at once cannot starve a
    /// waiter.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Timeout`] if the lock cannot be acquired within
    /// the given timeout, or [`LockError::IoError`] on I/O failure.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, LockError> {
        Ok(Self {
            guard: FileGuard::acquire_gated(path, timeout, LockKind::Exclusive)?,
        })
    }

    /// Explicitly release the lock. Release also happens automatically on drop.
    pub fn release(self) {
        self.guard.release();
    }

    /// Return the lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.guard.path()
    }
}

#[cfg(test)]
mod tests {
    use super::{DbReadLock, DbWriteLock, LockError, ShardLock};
    use crate::error::ErrorCode;
    use std::{
        path::PathBuf,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    fn lock_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push("bones_lock_tests");
        path.push(name);
        path
    }

    #[test]
    fn shard_lock_allows_acquire_and_release() -> Result<(), LockError> {
        let path = lock_path("basic.lock");
        let lock = ShardLock::acquire(&path, Duration::from_millis(50))?;
        assert_eq!(lock.path(), path.as_path());
        lock.release();
        Ok(())
    }

    #[test]
    fn shard_lock_times_out_when_held() {
        let path = lock_path("timeout.lock");
        let _guard = ShardLock::acquire(&path, Duration::from_millis(50)).unwrap();
        let err = ShardLock::acquire(&path, Duration::from_millis(20)).unwrap_err();

        assert!(matches!(err, LockError::Timeout { path: p, .. } if p == path));
    }

    #[test]
    fn lock_error_maps_to_machine_code() {
        let timeout = LockError::Timeout {
            path: lock_path("code.lock"),
            waited: Duration::from_millis(10),
        };
        assert_eq!(timeout.code(), ErrorCode::LockContention);
        assert!(timeout.hint().is_some());
    }

    #[test]
    fn sqlite_read_locks_are_compatible() -> Result<(), LockError> {
        let path = lock_path("read-share.lock");
        let first = DbReadLock::acquire(&path, Duration::from_millis(50))?;
        let second = DbReadLock::acquire(&path, Duration::from_millis(50))?;

        first.release();
        second.release();
        Ok(())
    }

    #[test]
    fn sqlite_write_blocks_readers() {
        let path = lock_path("write-blocks-read.lock");
        let _write = DbWriteLock::acquire(&path, Duration::from_millis(50)).unwrap();

        let started = std::time::Instant::now();
        let read = DbReadLock::acquire(&path, Duration::from_millis(20));

        assert!(matches!(read, Err(LockError::Timeout { .. })));
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn lock_release_allows_follow_up_lock() -> Result<(), LockError> {
        let path = lock_path("release-followup.lock");
        {
            let _first = ShardLock::acquire(&path, Duration::from_millis(50))?;
        }

        let _second = ShardLock::acquire(&path, Duration::from_millis(50))?;
        Ok(())
    }

    #[test]
    fn contention_is_resolved_after_writer_releases() -> Result<(), LockError> {
        let path = lock_path("thread.lock");

        let blocker = Arc::new(Barrier::new(2));
        let waiter = Arc::new(Barrier::new(2));

        let blocker_thread = Arc::clone(&blocker);
        let waiter_thread = Arc::clone(&waiter);
        let path_in_thread = path.clone();
        let handle = thread::spawn(move || {
            let _writer = ShardLock::acquire(&path_in_thread, Duration::from_millis(200)).unwrap();
            blocker_thread.wait();
            waiter_thread.wait();
        });

        blocker.wait();
        assert!(matches!(
            DbReadLock::acquire(&path, Duration::from_millis(20)),
            Err(LockError::Timeout { .. })
        ));
        waiter.wait();
        handle.join().unwrap();

        let follow_up = ShardLock::acquire(&path, Duration::from_millis(50))?;
        follow_up.release();
        Ok(())
    }
}
