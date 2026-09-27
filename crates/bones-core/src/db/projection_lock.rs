//! The projection write lock (bn-x6aa).
//!
//! Every path that writes projected rows *and* the projection cursor holds
//! this lock for the whole write:
//!
//! - a full rebuild ([`crate::db::rebuild::rebuild`]): build into the staging
//!   file, then copy into the live `bones.db`;
//! - an incremental apply ([`crate::db::incremental::incremental_apply`]):
//!   validate, replay, then write the cursor and prefix digest;
//! - a single-event projection ([`crate::db::project::Projector::project_event`]):
//!   project one event, then advance the cursor to the end of the log. The
//!   `bn` write commands and the TUI project through this call.
//!
//! Without the lock a rebuild could copy its pages into the live file between
//! a writer's event rows and its cursor write. The cursor then covered an
//! event that the copy had removed, and the event was lost for good. Two
//! concurrent rebuilds also shared one staging file.
//!
//! The lock is an exclusive advisory lock on a separate file,
//! `.bones/projection.lock`, not on `bones.db`. On Windows a `LockFileEx`
//! lock on the database file conflicts with the byte-range locks of `SQLite`.
//! Readers do not take the lock. `SQLite` gives them a consistent snapshot.
//!
//! # Lock order
//!
//! The shard lock (`.bones/lock`) comes first. A holder of the projection
//! lock must never acquire the shard lock. The `bn` write commands append
//! under the shard lock, release it, and only then project. The rebuild and
//! incremental paths do not take the shard lock.
//!
//! # Re-entry
//!
//! An advisory file lock conflicts with a second open of the same file, also
//! in the same process. So a code path must acquire the lock once:
//! `incremental_apply` holds it and calls `rebuild_locked`, which does not
//! acquire it again. A second acquire from the same thread is a bug. It fails
//! at once with an error instead of waiting for the timeout.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::lock::DbWriteLock;

/// File name of the projection write lock, beside `bones.db`.
pub const PROJECTION_LOCK_FILE: &str = "projection.lock";

/// How long a projection writer waits for another one.
///
/// A writer can wait for a full rebuild. The Tier M rebuild target is 8 s,
/// and a larger repository or a slow disk takes longer. 60 s covers several
/// such rebuilds but still fails a writer that waits on a stuck process.
pub const PROJECTION_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

thread_local! {
    /// Lock paths this thread holds, to catch a re-entrant acquire.
    static HELD: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// The projection lock path for the projection database at `db_path`.
#[must_use]
pub fn projection_lock_path(db_path: &Path) -> PathBuf {
    let parent = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    parent.join(PROJECTION_LOCK_FILE)
}

/// RAII guard for the projection write lock. Dropping it releases the lock.
///
/// The guard is not `Send`: the re-entry check is per thread.
pub struct ProjectionLock {
    _lock: DbWriteLock,
    key: PathBuf,
    _not_send: PhantomData<*const ()>,
}

impl std::fmt::Debug for ProjectionLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionLock")
            .field("path", &self.key)
            .finish_non_exhaustive()
    }
}

impl Drop for ProjectionLock {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(pos) = held.iter().position(|p| p == &self.key) {
                held.swap_remove(pos);
            }
        });
    }
}

/// Acquire the projection write lock for the database at `db_path`, and
/// wait up to [`PROJECTION_LOCK_TIMEOUT`].
///
/// # Errors
///
/// Returns an error if another process holds the lock for longer than the
/// timeout, if this thread already holds it, or on I/O failure.
pub fn lock_projection(db_path: &Path) -> Result<ProjectionLock> {
    lock_projection_with_timeout(db_path, PROJECTION_LOCK_TIMEOUT)
}

/// [`lock_projection`] with an explicit timeout.
///
/// # Errors
///
/// See [`lock_projection`].
pub fn lock_projection_with_timeout(db_path: &Path, timeout: Duration) -> Result<ProjectionLock> {
    let path = projection_lock_path(db_path);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create projection lock directory {}", dir.display()))?;
    // One key for every spelling of the same directory.
    let key = dir
        .canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .join(PROJECTION_LOCK_FILE);

    if HELD.with(|held| held.borrow().contains(&key)) {
        anyhow::bail!(
            "projection write lock {} is already held by this thread (lock re-entry bug)",
            path.display()
        );
    }

    let lock = DbWriteLock::acquire(&path, timeout).map_err(|err| {
        anyhow::anyhow!(
            "projection database is busy: another bn process is rebuilding or writing it \
             ({err}); retry when it finishes"
        )
    })?;
    HELD.with(|held| held.borrow_mut().push(key.clone()));
    Ok(ProjectionLock {
        _lock: lock,
        key,
        _not_send: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_beside_the_database() {
        assert_eq!(
            projection_lock_path(Path::new("/repo/.bones/bones.db")),
            PathBuf::from("/repo/.bones/projection.lock")
        );
        assert_eq!(
            projection_lock_path(Path::new("bones.db")),
            PathBuf::from("./projection.lock")
        );
    }

    #[test]
    fn second_acquire_on_one_thread_fails_at_once() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db = dir.path().join("bones.db");
        let _held = lock_projection(&db).expect("first acquire");
        let started = std::time::Instant::now();
        let err = lock_projection(&db).expect_err("re-entry must fail");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("already held"), "{err:#}");
    }

    #[test]
    fn other_thread_waits_and_times_out_with_a_clear_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db = dir.path().join("bones.db");
        let _held = lock_projection(&db).expect("acquire");
        let db2 = db.clone();
        let err = std::thread::spawn(move || {
            lock_projection_with_timeout(&db2, Duration::from_millis(50)).map(|_| ())
        })
        .join()
        .expect("join")
        .expect_err("must time out");
        assert!(format!("{err:#}").contains("busy"), "{err:#}");
    }

    #[test]
    fn release_lets_the_next_holder_in() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db = dir.path().join("bones.db");
        drop(lock_projection(&db).expect("first"));
        let _again = lock_projection(&db).expect("after release");
    }
}
