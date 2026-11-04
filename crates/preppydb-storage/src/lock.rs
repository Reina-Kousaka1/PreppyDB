//! Single-writer directory lock.
//!
//! Rust's standard library has no portable advisory file lock, so PreppyDB uses a
//! `LOCK` file containing the owning process id plus a start time. A lock is
//! considered stale (and is taken over with a warning) when the recorded process no
//! longer exists. This is documented in `docs/SECURITY.md` as a best-effort guard: it
//! protects against accidental double starts, not against deliberate tampering.

use crate::error::{DbError, Result};
use preppydb_json::Json;
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "LOCK";

/// Held while an [`crate::Engine`] has the directory open.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    pid: u32,
    active: bool,
}

impl FileLock {
    /// Create (or take over a stale) lock file in `dir`.
    pub fn acquire(dir: &Path) -> Result<FileLock> {
        let path = dir.join(LOCK_FILE);
        let pid = std::process::id();
        let now = crate::clock::now_ms();
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if let Ok(json) = preppydb_json::parse(&existing) {
                let other_pid = json.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                // A live owning process (including this very process) means the
                // directory is in use; only a dead pid may be taken over.
                if other_pid != 0 && process_alive(other_pid) {
                    return Err(DbError::Locked(format!(
                        "database directory is already open by process {other_pid} ({path:?})"
                    )));
                }
            }
        }
        let payload = Json::obj()
            .with("pid", Json::uint(pid as u64))
            .with("started_at", Json::int(now))
            .with("version", Json::str(env!("CARGO_PKG_VERSION")))
            .to_string();
        write_private(&path, payload.as_bytes())?;
        Ok(FileLock {
            path,
            pid,
            active: true,
        })
    }

    pub fn release(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(&self.path);
            self.active = false;
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Process id recorded in the lock file.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Write a file that only the current user can read (mode 0600 on Unix).
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
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

fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // Without /proc we cannot tell; assume alive to stay on the safe side.
        let _ = pid;
        true
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
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
        assert!(lock.path().exists());
        lock.release();
        assert!(!lock.path().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn second_acquire_by_live_pid_is_rejected() {
        let dir = tempdir("second");
        // Simulate a live foreign process by writing our own pid but claiming it is
        // a different (existing) process: use pid 1, which always exists on Unix.
        #[cfg(unix)]
        {
            let payload = Json::obj().with("pid", Json::uint(1)).to_string();
            write_private(&dir.join(LOCK_FILE), payload.as_bytes()).unwrap();
            let err = FileLock::acquire(&dir).unwrap_err();
            assert!(matches!(err, DbError::Locked(_)));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let dir = tempdir("stale");
        // PID 0 is never a valid process id, so the lock counts as stale.
        let payload = Json::obj().with("pid", Json::uint(0)).to_string();
        write_private(&dir.join(LOCK_FILE), payload.as_bytes()).unwrap();
        let lock = FileLock::acquire(&dir).unwrap();
        assert!(lock.path().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
