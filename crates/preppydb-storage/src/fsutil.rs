//! Small filesystem helpers shared by the storage engine, the backup code and the
//! server's credential store.
//!
//! The only non-obvious one is [`replace_file`]: POSIX `rename(2)` atomically replaces an
//! existing destination, while the Win32 rename used by `std::fs::rename` fails when the
//! target exists. Every manifest, snapshot, backup and `auth.json` write goes through
//! this helper so the "write temp, then swap" pattern behaves the same on Linux, macOS
//! and Windows.

use std::path::Path;

/// Atomically move `from` over `to`, replacing an existing file.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        match std::fs::rename(from, to) {
            Ok(()) => Ok(()),
            Err(err) if to.exists() => {
                // Retry once after removing the destination (the caller holds the only
                // writer lock for this directory, and the source is a fully written and
                // flushed temp file).
                std::fs::remove_file(to)?;
                match std::fs::rename(from, to) {
                    Ok(()) => Ok(()),
                    Err(second) => Err(std::io::Error::new(
                        second.kind(),
                        format!(
                            "replacing {} with {}: {second} (first attempt: {err})",
                            to.display(),
                            from.display()
                        ),
                    )),
                }
            }
            Err(err) => Err(err),
        }
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
}
