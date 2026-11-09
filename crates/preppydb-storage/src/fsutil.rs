//! Small filesystem helpers shared by the storage engine, the backup code and the
//! server's credential store.
//!
//! Windows publication uses a narrow Write-through MoveFileExW wrapper; other platforms
//! use the standard library's rename. Replacement errors propagate without deleting the
//! destination. Durability still depends on file sync and platform/filesystem behavior.

use std::path::Path;

/// Move `from` to a path that must not already exist.
pub fn move_path(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        preppydb_winfs::move_path(from, to)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// Move `from` over `to`, propagating any failure without deleting `to` first.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        preppydb_winfs::replace_file(from, to)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "preppydb-fsutil-{tag}-{}-{}",
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
    fn replaces_an_existing_file() {
        let dir = tempdir("replace");
        let target = dir.join("data.bin");
        std::fs::write(&target, b"old").unwrap();
        if !target.is_file() {
            panic!("target was not created");
        }
        let temp = dir.join("data.bin.tmp");
        let mut file = std::fs::File::create(&temp).unwrap();
        file.write_all(b"new").unwrap();
        file.sync_all().unwrap();
        drop(file);
        replace_file(&temp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!temp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn moves_when_the_target_does_not_exist() {
        let dir = tempdir("move");
        let target = dir.join("fresh.bin");
        let temp = dir.join("fresh.bin.tmp");
        std::fs::write(&temp, b"payload").unwrap();
        replace_file(&temp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_replacement_preserves_existing_target() {
        let dir = tempdir("missing-source");
        let target = dir.join("manifest");
        std::fs::write(&target, b"committed").unwrap();
        assert!(replace_file(&dir.join("missing"), &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"committed");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
