//! Single-writer directory lock.
//!
//! Exclusion is provided by an **OS advisory lock** on the `LOCK` file
//! (`File::try_lock`, which is `flock(LOCK_EX|LOCK_NB)` on Unix and `LockFileEx` on
//! Windows). The kernel releases the lock when the owning process exits, so a crashed
//! server never leaves a lock behind and no pid-liveness heuristics are needed — that
//! matters on Windows, where probing a pid is not portable.
//!
//! The file also records the owner's pid, start time and version as plain JSON for
//! humans reading a data directory (`preppydb-server` prints it in its lock error).
//!
//! This is a guard against accidental double starts on one filesystem, not a security
//! boundary: an advisory lock is not enforced against a process that ignores it.
//! The file is retained after unlock; removing it while the database is in use can
//! split contenders across different inodes and defeat mutual exclusion.

use crate::error::{DbError, Result};
use preppydb_json::Json;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "LOCK";

/// Held while an [`crate::Engine`] has the directory open.
#[derive(Debug)]
pub struct FileLock {
    file: Option<File>,
    path: PathBuf,
    pid: u32,
}

impl FileLock {
    /// Take the exclusive lock on `dir`, or fail with [`DbError::Locked`].
    pub fn acquire(dir: &Path) -> Result<FileLock> {
        let path = dir.join(LOCK_FILE);
        let pid = std::process::id();
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let owner = std::fs::read_to_string(&path).unwrap_or_default();
                let detail = owner.trim();
                return Err(DbError::Locked(if detail.is_empty() {
                    format!(
                        "database directory is already open ({} is locked)",
                        path.display()
                    )
                } else {
                    format!("database directory is already open ({detail})")
                }));
            }
            Err(TryLockError::Error(err)) => {
                return Err(DbError::io(format!("locking {}", path.display()), err));
            }
        }

        // We own the directory; refresh the descriptive metadata. A failure here is not
        // fatal — the lock itself is what protects the data.
        let payload = Json::obj()
            .with("pid", Json::uint(pid as u64))
            .with("started_at", Json::int(crate::clock::now_ms()))
            .with("version", Json::str(env!("CARGO_PKG_VERSION")))
            .to_string();
        let _ = file.set_len(0);
        let _ = file.write_all(payload.as_bytes());
        let _ = file.sync_all();

        Ok(FileLock {
            file: Some(file),
            path,
            pid,
        })
    }

    /// Release the lock. Idempotent; also called from `Drop`.
    pub fn release(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
            drop(file);
            // Never unlink the lock file: another process can acquire this inode
            // immediately after unlock, and deleting it would let a third process
            // create and lock a different inode for the same database directory.
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Process id recorded for this holder.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether this handle still owns the lock.
    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    /// The descriptive metadata stored in the lock file.
    ///
    /// Read through the held handle: on Windows the exclusive byte-range lock blocks
    /// readers that open the file separately, so going through our own handle is the
    /// only portable way to look at it while the lock is held.
    pub fn metadata(&mut self) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let file = self.file.as_mut()?;
        if file.seek(SeekFrom::Start(0)).is_err() {
            return None;
        }
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        Some(text)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Write a file that only the current user can read (mode 0600 on Unix).
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| DbError::io(format!("creating {}", path.display()), err))?;
    file.write_all(contents)
        .map_err(|err| DbError::io(format!("writing {}", path.display()), err))?;
    file.sync_all()
        .map_err(|err| DbError::io(format!("syncing {}", path.display()), err))?;
    Ok(())
}

/// Create a directory readable only by the current user (mode 0700 on Unix).
pub fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|err| DbError::io(format!("creating {}", path.display()), err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms).map_err(|err| {
            DbError::io(format!("setting permissions on {}", path.display()), err)
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "preppydb-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn acquire_release_cycle() {
        let dir = tempdir("cycle");
        let mut lock = FileLock::acquire(&dir).unwrap();
        assert!(lock.is_held());
        assert!(lock.path().exists());
        assert_eq!(lock.pid(), std::process::id());
        lock.release();
        assert!(!lock.is_held());
        assert!(
            lock.path().exists(),
            "the stable lock inode must be retained"
        );

        // After release the directory can be locked again.
        let again = FileLock::acquire(&dir).unwrap();
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn second_holder_is_rejected() {
        let dir = tempdir("second");
        let first = FileLock::acquire(&dir).unwrap();
        let err = FileLock::acquire(&dir).unwrap_err();
        assert!(matches!(err, DbError::Locked(_)), "got {err:?}");
        // The error mentions the owning process, which is this one.
        assert!(err.to_string().contains(&std::process::id().to_string()));
        drop(first);
        // Once the first handle is gone the lock is free again.
        let second = FileLock::acquire(&dir).unwrap();
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn handoff_keeps_same_inode_and_excludes_third_holder() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempdir("handoff");
        let mut first = FileLock::acquire(&dir).unwrap();
        let inode = std::fs::metadata(first.path()).unwrap().ino();
        first.release();
        let second = FileLock::acquire(&dir).unwrap();
        assert_eq!(std::fs::metadata(second.path()).unwrap().ino(), inode);
        assert!(matches!(FileLock::acquire(&dir), Err(DbError::Locked(_))));
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_lock_file_does_not_block() {
        let dir = tempdir("stale");
        // A LOCK file left behind by a dead process carries no OS lock, so it must not
        // prevent a new owner from taking over.
        let payload = Json::obj().with("pid", Json::uint(1)).to_string();
        write_private(&dir.join(LOCK_FILE), payload.as_bytes()).unwrap();
        let lock = FileLock::acquire(&dir).unwrap();
        assert!(lock.is_held());
        drop(lock);

        // And the metadata now names us.
        let mut lock = FileLock::acquire(&dir).unwrap();
        let text = lock.metadata().expect("lock metadata is readable");
        let expected = format!("\"pid\":{}", std::process::id());
        assert!(text.contains(&expected), "lock metadata was {text}");
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_is_released_when_the_handle_is_dropped() {
        let dir = tempdir("drop");
        {
            let _lock = FileLock::acquire(&dir).unwrap();
        }
        let lock = FileLock::acquire(&dir).unwrap();
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
