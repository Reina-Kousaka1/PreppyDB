//! Small, isolated Windows namespace operations used when publishing durable files.
//!
//! The storage crate keeps `unsafe` forbidden. This crate confines the Win32 FFI to
//! two path operations and uses `MOVEFILE_WRITE_THROUGH` so the call does not return
//! before Windows reports that the rename has reached disk.

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
