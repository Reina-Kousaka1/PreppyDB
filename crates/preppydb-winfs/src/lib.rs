//! Small, isolated Windows namespace operations used when publishing durable files.
//!
//! The storage crate keeps `unsafe` forbidden. This crate confines the Win32 FFI to
//! two path operations and uses `MOVEFILE_WRITE_THROUGH` so the call does not return
//! before Windows reports that the rename has reached disk. This does not claim that
//! every parent or ancestor directory entry is durable across sudden power loss.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

/// Rename a file or directory without replacing an existing destination.
pub fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    move_path_with_flags(from, to, MOVEFILE_WRITE_THROUGH)
}

/// Atomically replace a file and wait for the Windows move operation to finish.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    move_path_with_flags(from, to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
}

fn move_path_with_flags(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    let from = wide_path(from)?;
    let to = wide_path(to)?;
    // SAFETY: both pointers refer to owned, NUL-terminated UTF-16 buffers that live
    // through the call. MoveFileExW does not retain either pointer.
    let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path contains a NUL character",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "preppydb-winfs-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn move_path_does_not_replace_an_existing_destination() {
        let dir = temp_dir("no-replace");
        let source = dir.join("source");
        let destination = dir.join("destination");
        std::fs::write(&source, b"source bytes").unwrap();
        std::fs::write(&destination, b"destination bytes").unwrap();

        assert!(move_path(&source, &destination).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination bytes");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_file_publishes_the_complete_source_and_removes_its_old_name() {
        let dir = temp_dir("replace");
        let source = dir.join("manifest.tmp");
        let destination = dir.join("manifest.json");
        std::fs::write(&source, b"complete new manifest").unwrap();
        std::fs::write(&destination, b"old manifest").unwrap();

        replace_file(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"complete new manifest"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn move_path_publishes_a_directory_tree_as_one_path_operation() {
        let dir = temp_dir("directory");
        let source = dir.join("staged");
        let destination = dir.join("live");
        std::fs::create_dir_all(source.join("wal")).unwrap();
        std::fs::write(source.join("wal").join("000000000001.wal"), b"wal bytes").unwrap();

        move_path(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(destination.join("wal").join("000000000001.wal")).unwrap(),
            b"wal bytes"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn win32_paths_with_nul_are_rejected_before_ffi() {
        let error = wide_path(Path::new("invalid\0path")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
