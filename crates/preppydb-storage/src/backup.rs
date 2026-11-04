//! Backup and restore with end-to-end integrity verification.
//!
//! Archive layout:
//!
//! ```text
//! magic "PPDBBAK1" | u32 header_len | header JSON | <file bytes...> | "PPDBEND1" | sha256(all preceding bytes)
//! ```
//!
//! The header lists every member with its length and SHA-256. `restore` verifies the
//! whole archive (member checksums + trailing digest) **before** writing anything, and
//! it never deletes an existing database directory: a conflicting target is moved
//! aside to `<name>.replaced-<timestamp>`.

use crate::clock::{format_rfc3339, now_ms};
use crate::engine::Engine;
use crate::error::{DbError, Result};
use crate::snapshot::fsync_dir;
use preppydb_crypto::sha256::{sha256, Sha256};
use preppydb_json::Json;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const ARCHIVE_MAGIC: &[u8; 8] = b"PPDBBAK1";
pub const ARCHIVE_END: &[u8; 8] = b"PPDBEND1";
pub const ARCHIVE_FORMAT_VERSION: u32 = 1;

/// One member of an archive.
#[derive(Clone, Debug)]
pub struct BackupEntry {
    /// Path relative to the database directory (`MANIFEST.json`, `wal/00..1.wal`).
    pub path: String,
    pub len: u64,
    pub sha256: String,
}

impl BackupEntry {
    fn to_json(&self) -> Json {
        Json::obj()
            .with("path", Json::str(self.path.clone()))
            .with("len", Json::uint(self.len))
            .with("sha256", Json::str(self.sha256.clone()))
    }

    fn from_json(value: &Json) -> Result<BackupEntry> {
        Ok(BackupEntry {
            path: value
                .get_str("path")
                .ok_or_else(|| DbError::corrupt("backup", "entry without path"))?
                .to_string(),
            len: value.get("len").and_then(|v| v.as_u64()).unwrap_or(0),
            sha256: value.get_str("sha256").unwrap_or("").to_string(),
        })
    }
}

/// Result of [`create_backup`].
#[derive(Clone, Debug)]
pub struct BackupReport {
    pub archive: String,
    pub entries: usize,
    pub bytes: u64,
    pub created_at: i64,
    pub checkpoint_taken: bool,
    /// SHA-256 over the complete archive file.
    pub archive_sha256: String,
    /// SHA-256 over the archive content (everything except the trailing digest).
    pub content_sha256: String,
}

impl BackupReport {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("archive", Json::str(self.archive.clone()))
            .with("entries", Json::uint(self.entries as u64))
            .with("bytes", Json::uint(self.bytes))
            .with("created_at", Json::int(self.created_at))
            .with(
                "created_at_rfc3339",
                Json::str(format_rfc3339(self.created_at)),
            )
            .with("checkpoint_taken", Json::Bool(self.checkpoint_taken))
            .with("archive_sha256", Json::str(self.archive_sha256.clone()))
            .with("content_sha256", Json::str(self.content_sha256.clone()))
    }
}

/// Result of [`restore_backup`].
#[derive(Clone, Debug)]
pub struct RestoreReport {
    pub archive: String,
    pub target: String,
    pub entries: usize,
    pub bytes: u64,
    pub replaced_path: Option<String>,
    pub archive_sha256: String,
}

impl RestoreReport {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("archive", Json::str(self.archive.clone()))
            .with("target", Json::str(self.target.clone()))
            .with("entries", Json::uint(self.entries as u64))
            .with("bytes", Json::uint(self.bytes))
            .with(
                "replaced_path",
                self.replaced_path
                    .clone()
                    .map(Json::str)
                    .unwrap_or(Json::Null),
            )
            .with("archive_sha256", Json::str(self.archive_sha256.clone()))
    }
}

/// Files that make up a database directory (excluding the lock file and reports).
fn collect_members(root: &Path) -> Result<Vec<PathBuf>> {
    let mut members = Vec::new();
    let manifest = root.join(crate::manifest::MANIFEST_FILE);
    if manifest.exists() {
        members.push(manifest);
    }
    for dir in ["snap", "wal"] {
        let path = root.join(dir);
        if let Ok(entries) = std::fs::read_dir(&path) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| p.is_file())
                .filter(|p| !p.extension().map(|ext| ext == "tmp").unwrap_or(false))
                .collect();
            files.sort();
            members.extend(files);
        }
    }
    Ok(members)
}

/// Create an archive of `engine`'s database directory.
///
/// When `checkpoint_first` is set (recommended) a checkpoint runs first, so the archive
/// contains one snapshot plus at most the tail of the log.
pub fn create_backup(
    engine: &Engine,
    archive: &Path,
    checkpoint_first: bool,
) -> Result<BackupReport> {
    let root = engine.paths().root.clone();
    if checkpoint_first {
        engine.checkpoint()?;
    } else {
        engine.flush()?;
    }

    let members = collect_members(&root)?;
    let mut entries = Vec::with_capacity(members.len());
    let mut total_len: u64 = 0;
    for member in &members {
        let (len, hash) = crate::snapshot::file_sha256(member)?;
        total_len += len;
        entries.push(BackupEntry {
            path: member
                .strip_prefix(&root)
                .map_err(|_| DbError::Internal("member outside the data directory".into()))?
                .to_string_lossy()
                .replace('\\', "/"),
            len,
            sha256: hash,
        });
    }

    let header = Json::obj()
        .with("format_version", Json::uint(ARCHIVE_FORMAT_VERSION as u64))
        .with("engine_version", Json::str(env!("CARGO_PKG_VERSION")))
        .with("created_at", Json::int(now_ms()))
        .with("created_at_rfc3339", Json::str(format_rfc3339(now_ms())))
        .with(
            "source",
            Json::str(engine.paths().root.display().to_string()),
        )
        .with("last_seq", Json::uint(engine.last_seq()))
        .with(
            "entries",
            Json::Array(entries.iter().map(|e| e.to_json()).collect()),
        );
    let header_bytes = header.to_string().into_bytes();

    if let Some(parent) = archive.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|err| DbError::io("creating archive directory", err))?;
        }
    }
    let tmp = archive.with_extension("bak.tmp");
    let file = File::create(&tmp)
        .map_err(|err| DbError::io(format!("creating {}", tmp.display()), err))?;
    let mut writer = BufWriter::new(file);
    let mut digest = Sha256::new();
    let write = |writer: &mut BufWriter<File>, digest: &mut Sha256, bytes: &[u8]| -> Result<()> {
        writer
            .write_all(bytes)
            .map_err(|err| DbError::io("writing backup archive", err))?;
        digest.update(bytes);
        Ok(())
    };

    write(&mut writer, &mut digest, ARCHIVE_MAGIC)?;
    write(
        &mut writer,
        &mut digest,
        &(header_bytes.len() as u32).to_le_bytes(),
    )?;
    write(&mut writer, &mut digest, &header_bytes)?;
    for member in &members {
        let mut input = File::open(member)
            .map_err(|err| DbError::io(format!("opening {}", member.display()), err))?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|err| DbError::io("reading backup member", err))?;
            if read == 0 {
                break;
            }
            write(&mut writer, &mut digest, &buffer[..read])?;
        }
    }
    write(&mut writer, &mut digest, ARCHIVE_END)?;
    let archive_digest = digest.finish();
    writer
        .write_all(&archive_digest)
        .map_err(|err| DbError::io("writing archive digest", err))?;
    writer
        .flush()
        .map_err(|err| DbError::io("flushing archive", err))?;
    let file = writer
        .into_inner()
        .map_err(|err| DbError::io("flushing archive", err.into_error()))?;
    file.sync_all()
        .map_err(|err| DbError::io("syncing archive", err))?;
    drop(file);
    crate::fsutil::replace_file(&tmp, archive)
        .map_err(|err| DbError::io("renaming archive into place", err))?;

    let (bytes, file_hash) = crate::snapshot::file_sha256(archive)?;
    debug_assert!(
        bytes >= total_len,
        "archive must contain at least its member data"
    );
    Ok(BackupReport {
        archive: archive.display().to_string(),
        entries: entries.len(),
        bytes,
        created_at: now_ms(),
        checkpoint_taken: checkpoint_first,
        // SHA-256 of the whole archive file, so `sha256sum` corroborates it.
        archive_sha256: file_hash,
        content_sha256: preppydb_crypto::hex_encode(&archive_digest),
    })
}

/// Parsed archive header plus verification state.
struct ArchiveReader {
    file: BufReader<File>,
    entries: Vec<BackupEntry>,
    header: Json,
    data_start: u64,
    expected_digest: [u8; 32],
}

fn open_archive(archive: &Path) -> Result<ArchiveReader> {
    let file = File::open(archive)
        .map_err(|err| DbError::io(format!("opening {}", archive.display()), err))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|err| DbError::io("reading archive magic", err))?;
    if &magic != ARCHIVE_MAGIC {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            "not a PreppyDB backup (bad magic)",
        ));
    }
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|err| DbError::io("reading archive header length", err))?;
    let header_len = u32::from_le_bytes(len_buf);
    if header_len > 16 << 20 {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            format!("implausible header length {header_len}"),
        ));
    }
    let mut header_bytes = vec![0u8; header_len as usize];
    reader
        .read_exact(&mut header_bytes)
        .map_err(|err| DbError::io("reading archive header", err))?;
    let header_text = std::str::from_utf8(&header_bytes)
        .map_err(|err| DbError::corrupt("backup", format!("header is not UTF-8: {err}")))?;
    let header = preppydb_json::parse(header_text)
        .map_err(|err| DbError::corrupt("backup", format!("header is not JSON: {err}")))?;
    let version = header
        .get("format_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if version != ARCHIVE_FORMAT_VERSION as u64 {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            format!("unsupported archive format version {version}"),
        ));
    }
    let entries = header
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| DbError::corrupt("backup", "header without entries"))?
        .iter()
        .map(BackupEntry::from_json)
        .collect::<Result<Vec<_>>>()?;

    let data_start = (8 + 4 + header_bytes.len()) as u64;
    // The trailing 32 bytes hold the digest of everything before it.
    let total = reader
        .get_ref()
        .metadata()
        .map_err(|err| DbError::io("stat archive", err))?
        .len();
    if total < data_start + 8 + 32 {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            "archive is truncated",
        ));
    }
    reader
        .seek(SeekFrom::Start(total - 32))
        .map_err(|err| DbError::io("seeking archive digest", err))?;
    let mut expected_digest = [0u8; 32];
    reader
        .read_exact(&mut expected_digest)
        .map_err(|err| DbError::io("reading archive digest", err))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| DbError::io("rewinding archive", err))?;
    Ok(ArchiveReader {
        file: reader,
        entries,
        header,
        data_start,
        expected_digest,
    })
}

impl ArchiveReader {
    /// Stream the whole archive, verifying member checksums, the end marker and the
    /// trailing digest. Nothing is written to disk here.
    fn verify(&mut self) -> Result<()> {
        let mut digest = Sha256::new();
        let mut prefix = vec![0u8; self.data_start as usize];
        self.file
            .read_exact(&mut prefix)
            .map_err(|err| DbError::io("reading archive prefix", err))?;
        digest.update(&prefix);
        let entries = self.entries.clone();
        for entry in &entries {
            if entry
                .path
                .split('/')
                .any(|part| part == ".." || part.is_empty())
            {
                return Err(DbError::corrupt(
                    "backup",
                    format!("unsafe member path '{}'", entry.path),
                ));
            }
            let mut hasher = Sha256::new();
            let mut remaining = entry.len;
            let mut buffer = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let want = (buffer.len() as u64).min(remaining) as usize;
                self.file
                    .read_exact(&mut buffer[..want])
                    .map_err(|err| DbError::io("reading archive member", err))?;
                digest.update(&buffer[..want]);
                hasher.update(&buffer[..want]);
                remaining -= want as u64;
            }
            let actual = preppydb_crypto::hex_encode(&hasher.finish());
            if actual != entry.sha256 {
                return Err(DbError::corrupt(
                    "backup",
                    format!("member '{}' checksum mismatch", entry.path),
                ));
            }
        }
        let mut end = [0u8; 8];
        self.file
            .read_exact(&mut end)
            .map_err(|err| DbError::io("reading archive end marker", err))?;
        if &end != ARCHIVE_END {
            return Err(DbError::corrupt("backup", "missing archive end marker"));
        }
        digest.update(&end);
        let actual = digest.finish();
        if actual != self.expected_digest {
            return Err(DbError::corrupt(
                "backup",
                "archive SHA-256 does not match the trailing digest",
            ));
        }
        Ok(())
    }

    fn entry_count(&self) -> usize {
        self.entries.len()
    }

    fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.len).sum()
    }
}

/// Verify an archive without restoring it.
pub fn verify_backup(archive: &Path) -> Result<Json> {
    let mut reader = open_archive(archive)?;
    let entries = reader.entries.clone();
    let header = reader.header.clone();
    reader.verify()?;
    Ok(Json::obj()
        .with("ok", Json::Bool(true))
        .with("archive", Json::str(archive.display().to_string()))
        .with(
            "created_at",
            header.get("created_at").cloned().unwrap_or(Json::Null),
        )
        .with("entries", Json::uint(entries.len() as u64))
        .with(
            "files",
            Json::Array(entries.iter().map(|e| e.to_json()).collect()),
        )
        .with(
            "sha256",
            Json::str(preppydb_crypto::hex_encode(&reader.expected_digest)),
        ))
}

/// Restore an archive into `target`.
///
/// Steps: open → verify everything → extract into a sibling temp directory → fsync →
/// move any existing target aside → rename temp into place → fsync parent.
pub fn restore_backup(archive: &Path, target: &Path, force: bool) -> Result<RestoreReport> {
    let mut reader = open_archive(archive)?;
    reader.verify()?;
    let entry_count = reader.entry_count();
    let total_bytes = reader.total_bytes();

    if target.exists() {
        let non_empty = std::fs::read_dir(target)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if non_empty && !force {
            return Err(DbError::validation(format!(
                "target '{}' exists and is not empty; pass force=true to move it aside and restore",
                target.display()
            )));
        }
    }

    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|err| DbError::io("creating restore parent directory", err))?;
    let stamp = now_ms();
    let temp = parent.join(format!(
        ".{}.restore-{stamp}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "db".into())
    ));
    if temp.exists() {
        std::fs::remove_dir_all(&temp)
            .map_err(|err| DbError::io("cleaning previous restore temp dir", err))?;
    }
    std::fs::create_dir_all(&temp).map_err(|err| DbError::io("creating restore temp dir", err))?;

    // Rewind and copy the members out.
    reader
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|err| DbError::io("rewinding archive", err))?;
    let mut skip = vec![0u8; reader.data_start as usize];
    reader
        .file
        .read_exact(&mut skip)
        .map_err(|err| DbError::io("skipping archive prefix", err))?;
    for entry in reader.entries.clone() {
        let path = temp.join(&entry.path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| DbError::io("creating restore subdirectory", err))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut out = options
            .open(&path)
            .map_err(|err| DbError::io(format!("creating {}", path.display()), err))?;
        let mut remaining = entry.len;
        let mut buffer = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = (buffer.len() as u64).min(remaining) as usize;
            reader
                .file
                .read_exact(&mut buffer[..want])
                .map_err(|err| DbError::io("reading archive member", err))?;
            out.write_all(&buffer[..want])
                .map_err(|err| DbError::io("writing restored file", err))?;
            remaining -= want as u64;
        }
        out.sync_all()
            .map_err(|err| DbError::io("syncing restored file", err))?;
    }
    fsync_dir(&temp)?;

    let replaced_path = if target.exists() {
        let aside = parent.join(format!(
            "{}.replaced-{stamp}",
            target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "db".into())
        ));
        crate::fsutil::replace_file(target, &aside)
            .map_err(|err| DbError::io("moving existing database aside", err))?;
        Some(aside.display().to_string())
    } else {
        None
    };
    crate::fsutil::replace_file(&temp, target)
        .map_err(|err| DbError::io("moving restored database into place", err))?;
    fsync_dir(parent)?;

    Ok(RestoreReport {
        archive: archive.display().to_string(),
        target: target.display().to_string(),
        entries: entry_count,
        bytes: total_bytes,
        replaced_path,
        archive_sha256: preppydb_crypto::hex_encode(&reader.expected_digest),
    })
}

/// Hash of an archive (helper for tests/tools).
pub fn archive_sha256(path: &Path) -> Result<String> {
    Ok(preppydb_crypto::hex_encode(&sha256(
        &std::fs::read(path).map_err(|err| DbError::io("reading archive", err))?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_constants_are_stable() {
        assert_eq!(ARCHIVE_MAGIC.len(), 8);
        assert_eq!(ARCHIVE_END.len(), 8);
    }
}
