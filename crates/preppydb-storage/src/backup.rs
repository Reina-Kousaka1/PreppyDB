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
//! aside to `<name>.replaced-<timestamp>/previous`.

use crate::clock::{format_rfc3339, now_ms};
use crate::engine::Engine;
use crate::error::{DbError, Result};
use crate::lock::FileLock;
use crate::snapshot::fsync_dir;
use preppydb_crypto::sha256::Sha256;
use preppydb_json::Json;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_BACKUP_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub const ARCHIVE_MAGIC: &[u8; 8] = b"PPDBBAK1";
pub const ARCHIVE_END: &[u8; 8] = b"PPDBEND1";
pub const ARCHIVE_FORMAT_VERSION: u32 = 1;

/// One member of an archive.
#[derive(Clone, Debug)]
pub struct BackupEntry {
    /// Path relative to the database directory (legacy or generation manifest, WAL,
    /// and snapshot members).
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
            len: value
                .get("len")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| DbError::corrupt("backup", "entry without a valid length"))?,
            sha256: value
                .get_str("sha256")
                .ok_or_else(|| DbError::corrupt("backup", "entry without a checksum"))?
                .to_string(),
        })
    }
}

/// Archive paths use `/` on every platform. Validate that grammar before passing
/// one to the host filesystem, including hosts where `\` is a separator.
fn valid_member_path(path: &str) -> bool {
    if crate::manifest::Manifest::is_manifest_name(path) {
        return true;
    }
    if path.contains('\\') || path.contains(':') || path.starts_with('/') {
        return false;
    }
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["wal", name] => crate::wal::parse_segment_name(name).is_some(),
        ["snap", name] => {
            name.starts_with("snap-")
                && name.ends_with(".pdb")
                && name[5..name.len() - 4].chars().all(|c| c.is_ascii_digit())
                && name.len() > 9
        }
        _ => false,
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
    for entry in std::fs::read_dir(root)
        .map_err(|err| DbError::io("listing database root for backup", err))?
    {
        let entry = entry.map_err(|err| DbError::io("reading database root entry", err))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !crate::manifest::Manifest::is_manifest_name(&name) {
            continue;
        }
        let metadata = entry
            .file_type()
            .map_err(|err| DbError::io("inspecting backup manifest", err))?;
        if !metadata.is_file() {
            return Err(DbError::corrupt(
                entry.path().display().to_string(),
                "manifest is not a regular file",
            ));
        }
        members.push(entry.path());
    }
    for dir in ["snap", "wal"] {
        let path = root.join(dir);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|err| DbError::io(format!("inspecting backup {dir} directory"), err))?;
        if !metadata.file_type().is_dir() {
            return Err(DbError::corrupt(
                path.display().to_string(),
                "backup member directory is not a real directory",
            ));
        }
        let entries = std::fs::read_dir(&path)
            .map_err(|err| DbError::io(format!("reading backup {dir} directory"), err))?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| DbError::io("reading backup directory entry", err))?;
            let file_type = entry
                .file_type()
                .map_err(|err| DbError::io("inspecting backup directory entry", err))?;
            let member = entry.path();
            if file_type.is_symlink() {
                return Err(DbError::corrupt(
                    member.display().to_string(),
                    "symbolic links are not valid backup members",
                ));
            }
            if !file_type.is_file() {
                return Err(DbError::corrupt(
                    member.display().to_string(),
                    "backup member is not a regular file",
                ));
            }
            if member.extension().is_some_and(|ext| ext == "tmp") {
                // Interrupted snapshot/manifest publication may leave temporary files;
                // they are intentionally not part of a consistent backup.
                continue;
            }
            let relative = member
                .strip_prefix(root)
                .map_err(|_| DbError::Internal("member outside the data directory".into()))?
                .to_string_lossy()
                .replace('\\', "/");
            if !valid_member_path(&relative) {
                return Err(DbError::corrupt(
                    member.display().to_string(),
                    "unexpected file in database storage directory",
                ));
            }
            files.push(member);
        }
        files.sort();
        members.extend(files);
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
    // The final archive rename must never replace a live manifest, snapshot or WAL
    // member. Check before checkpointing so a rejected destination cannot mutate the
    // source database as a side effect.
    validate_backup_destination(&engine.paths().root, archive)?;
    if checkpoint_first {
        engine.checkpoint()?;
    }

    engine.with_stable_files(|seq| create_backup_locked(engine, archive, checkpoint_first, seq))
}

fn create_backup_locked(
    engine: &Engine,
    archive: &Path,
    checkpoint_first: bool,
    seq: u64,
) -> Result<BackupReport> {
    let root = engine.paths().root.clone();
    validate_backup_destination(&root, archive)?;

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
        .with("last_seq", Json::uint(seq))
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
    let (tmp, file) = create_backup_temp(archive)?;
    let mut temp_guard = BackupTempFile::new(tmp.clone());
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
    // Hashes alone prove that copied members match this archive's header, not
    // that its snapshot, manifest and WAL form a recoverable database. Reopen the
    // staged archive before publication so a successful backup is restorable.
    verify_backup(&tmp)?;
    let (bytes, file_hash) = crate::snapshot::file_sha256(&tmp)?;
    debug_assert!(
        bytes >= total_len,
        "archive must contain at least its member data"
    );
    // Re-resolve the destination after creating the parent and writing the temp file.
    // This catches a parent symlink/reparse-point that changed before publication;
    // descriptor-relative publication would be needed to eliminate the final TOCTOU
    // window on hostile, concurrently modified parent directories.
    validate_backup_destination(&root, archive)?;
    crate::fsutil::replace_file(&tmp, archive)
        .map_err(|err| DbError::io("renaming archive into place", err))?;
    temp_guard.disarm();
    fsync_dir(archive.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(
        || Path::new("."),
    ))
    .map_err(|err| {
        DbError::io(
            "backup archive was published but syncing its parent directory failed; publication durability is unknown",
            std::io::Error::other(err.to_string()),
        )
    })?;

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

fn validate_backup_destination(root: &Path, archive: &Path) -> Result<()> {
    let root = std::fs::canonicalize(root)
        .map_err(|err| DbError::io("resolving database directory for backup", err))?;
    let archive = if archive.is_absolute() {
        archive.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| DbError::io("resolving backup destination", err))?
            .join(archive)
    };
    let name = archive
        .file_name()
        .filter(|name| *name != "." && *name != "..")
        .ok_or_else(|| DbError::validation("backup destination must name a file"))?;
    let parent = archive.parent().unwrap_or_else(|| Path::new("."));
    let resolved_parent = canonicalize_with_missing_suffix(parent)?;
    let resolved_destination = resolved_parent.join(name);
    if resolved_destination.starts_with(&root) {
        return Err(DbError::validation(
            "backup archive must be outside the source database directory",
        ));
    }
    Ok(())
}

/// Canonicalize an output parent even when some trailing directories do not yet
/// exist. Existing prefixes are resolved through symlinks/reparse points; the
/// missing suffix is then normalized lexically.
fn canonicalize_with_missing_suffix(path: &Path) -> Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    let mut suffix = Vec::new();
    let mut resolved = loop {
        match std::fs::canonicalize(&cursor) {
            Ok(resolved) => break resolved,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(component) = cursor.file_name() else {
                    return Err(DbError::io("resolving backup destination parent", err));
                };
                suffix.push(component.to_os_string());
                let Some(parent) = cursor.parent() else {
                    return Err(DbError::io("resolving backup destination parent", err));
                };
                cursor = parent.to_path_buf();
            }
            Err(err) => return Err(DbError::io("resolving backup destination parent", err)),
        }
    };
    for component in suffix.iter().rev() {
        if component == "." {
            continue;
        }
        if component == ".." {
            resolved.pop();
        } else {
            resolved.push(component);
        }
    }
    Ok(resolved)
}

fn create_backup_temp(archive: &Path) -> Result<(PathBuf, File)> {
    let parent = archive
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for _ in 0..32 {
        let id = next_backup_temp_id()?;
        let path = parent.join(format!(".preppydb-backup-{}-{id}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(DbError::io(
                    format!("creating backup temporary file {}", path.display()),
                    error,
                ));
            }
        }
    }
    Err(DbError::io(
        "creating unique backup temporary file",
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a unique temporary name after 32 attempts",
        ),
    ))
}

fn next_backup_temp_id() -> Result<u64> {
    let mut current = NEXT_BACKUP_TEMP_ID.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).ok_or(DbError::LimitExceeded {
            limit: "backup_temp_id",
            max: u64::MAX,
            actual: u64::MAX,
        })?;
        match NEXT_BACKUP_TEMP_ID.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

struct BackupTempFile {
    path: PathBuf,
    armed: bool,
}

impl BackupTempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BackupTempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Parsed archive header plus verification state.
struct ArchiveReader {
    file: BufReader<File>,
    entries: Vec<BackupEntry>,
    header: Json,
    data_start: u64,
    expected_digest: [u8; 32],
}

fn read_archive_exact(reader: &mut impl Read, buffer: &mut [u8], part: &str) -> Result<()> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            DbError::corrupt("backup", format!("truncated archive {part}"))
        } else {
            DbError::io(format!("reading archive {part}"), error)
        }
    })
}

fn open_archive(archive: &Path) -> Result<ArchiveReader> {
    let file = File::open(archive)
        .map_err(|err| DbError::io(format!("opening {}", archive.display()), err))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 8];
    read_archive_exact(&mut reader, &mut magic, "magic")?;
    if &magic != ARCHIVE_MAGIC {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            "not a PreppyDB backup (bad magic)",
        ));
    }
    let mut len_buf = [0u8; 4];
    read_archive_exact(&mut reader, &mut len_buf, "header length")?;
    let header_len = u32::from_le_bytes(len_buf);
    if header_len > 16 << 20 {
        return Err(DbError::corrupt(
            archive.display().to_string(),
            format!("implausible header length {header_len}"),
        ));
    }
    let mut header_bytes = vec![0u8; header_len as usize];
    read_archive_exact(&mut reader, &mut header_bytes, "header")?;
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
    validate_archive_layout(&entries, data_start, total)?;
    reader
        .seek(SeekFrom::Start(total - 32))
        .map_err(|err| DbError::io("seeking archive digest", err))?;
    let mut expected_digest = [0u8; 32];
    read_archive_exact(&mut reader, &mut expected_digest, "digest")?;
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

fn validate_archive_layout(entries: &[BackupEntry], data_start: u64, total: u64) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    if !entries
        .iter()
        .any(|entry| entry.path.starts_with("wal/") || entry.path.starts_with("snap/"))
    {
        return Err(DbError::corrupt(
            "backup",
            "archive contains no WAL segment or snapshot",
        ));
    }
    if entries.iter().any(|entry| entry.path.starts_with("snap/"))
        && !entries
            .iter()
            .any(|entry| crate::manifest::Manifest::is_manifest_name(&entry.path))
    {
        return Err(DbError::corrupt(
            "backup",
            "snapshot archive is missing its manifest",
        ));
    }
    let mut expected = data_start
        .checked_add(40)
        .ok_or_else(|| DbError::corrupt("backup", "archive size overflow"))?;
    for entry in entries {
        if !valid_member_path(&entry.path) || !names.insert(&entry.path) {
            return Err(DbError::corrupt(
                "backup",
                "invalid or duplicate member path",
            ));
        }
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(DbError::corrupt("backup", "invalid member checksum"));
        }
        expected = expected
            .checked_add(entry.len)
            .ok_or_else(|| DbError::corrupt("backup", "archive member length overflow"))?;
    }
    if expected != total {
        return Err(DbError::corrupt(
            "backup",
            "archive length does not match members",
        ));
    }
    Ok(())
}

impl ArchiveReader {
    /// Stream the whole archive, verifying member checksums, the end marker and the
    /// trailing digest. Nothing is written to disk here.
    fn verify(&mut self) -> Result<()> {
        let mut digest = Sha256::new();
        let mut prefix = vec![0u8; self.data_start as usize];
        read_archive_exact(&mut self.file, &mut prefix, "prefix")?;
        digest.update(&prefix);
        let entries = self.entries.clone();
        for entry in &entries {
            if !valid_member_path(&entry.path) {
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
                read_archive_exact(&mut self.file, &mut buffer[..want], "member")?;
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
        read_archive_exact(&mut self.file, &mut end, "end marker")?;
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
    let temp = std::env::temp_dir().join(format!(
        "preppydb-verify-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    create_restore_directory(&temp)?;
    let _temp_guard = RestoreTempDirectory(temp.clone());
    extract_archive(&mut reader, &temp)?;
    verify_restored_database(&temp)?;
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

    validate_restore_target(target, force)?;

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
    create_restore_directory(&temp)?;
    let _temp_guard = RestoreTempDirectory(temp.clone());

    extract_archive(&mut reader, &temp)?;
    verify_restored_database(&temp)?;
    sync_restore_tree(&temp)?;

    let aside = parent.join(format!(
        "{}.replaced-{stamp}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "db".into())
    ));
    // Hold the target's writer lock through publication. Engine::open checks the
    // sibling intent both before creating the database directory and again after
    // taking its directory lock, closing the open-vs-restore race for writers.
    validate_restore_target(target, force)?;
    let mut target_lock = match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Some(FileLock::acquire(target)?)
        }
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(DbError::io("checking restore target", err)),
    };
    validate_restore_target(target, force)?;
    let intent = RestoreIntent::create(target, &temp, &aside)?;
    // The durable sibling intent now prevents new Engine::open calls from
    // succeeding (including callers that passed the first check before the
    // intent was created: they check again after acquiring the directory lock).
    // Release the in-directory LOCK handle before renaming the target directory;
    // Windows refuses to rename a directory while a file handle inside it remains
    // open, even when the lock owner is the restoring process itself.
    drop(target_lock.take());
    let replaced_path = complete_restore_publication(
        intent,
        publish_restore(&temp, target, &aside, force),
        target,
    )?;

    Ok(RestoreReport {
        archive: archive.display().to_string(),
        target: target.display().to_string(),
        entries: entry_count,
        bytes: total_bytes,
        replaced_path,
        archive_sha256: preppydb_crypto::hex_encode(&reader.expected_digest),
    })
}

/// Keep the restore intent only when publication may have changed the target.
/// A definite pre-publication failure must not permanently prevent opening the
/// unchanged database; failure to remove the intent itself is recovery-required.
fn complete_restore_publication(
    intent: RestoreIntent,
    publication: Result<Option<String>>,
    target: &Path,
) -> Result<Option<String>> {
    match publication {
        Ok(replaced_path) => {
            intent.complete().map_err(|error| {
                DbError::RestoreOutcomeUnknown(format!(
                    "target '{}' was published but restore intent cleanup failed: {error}",
                    target.display()
                ))
            })?;
            Ok(replaced_path)
        }
        Err(error)
            if error.code() == crate::error::codes::RESTORE_OUTCOME_UNKNOWN
                || error.code() == crate::error::codes::RECOVERY_REQUIRED =>
        {
            Err(error)
        }
        Err(error) => {
            intent.complete().map_err(|cleanup_error| {
                DbError::RecoveryRequired(format!(
                    "restore failed before a known publication completed, and its intent could not be cleared: {cleanup_error}"
                ))
            })?;
            Err(error)
        }
    }
}

fn extract_archive(reader: &mut ArchiveReader, root: &Path) -> Result<()> {
    reader
        .file
        .seek(SeekFrom::Start(reader.data_start))
        .map_err(|err| DbError::io("seeking archive members", err))?;
    for entry in &reader.entries {
        let path = root.join(&entry.path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| DbError::io("creating restore subdirectory", err))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut out = options
            .open(&path)
            .map_err(|err| DbError::io(format!("creating {}", path.display()), err))?;
        copy_verified_member(&mut reader.file, &mut out, entry)?;
        out.sync_all()
            .map_err(|err| DbError::io("syncing restored file", err))?;
    }
    Ok(())
}

/// Persist restored directory entries bottom-up before the staged tree is published.
/// Files are synced as they are extracted; syncing only the root would not make nested
/// `wal/` and `snap/` directory entries durable on filesystems that require directory
/// synchronization for that guarantee.
fn sync_restore_tree(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|err| DbError::io("inspecting restored directory", err))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DbError::corrupt(
                directory.display().to_string(),
                "restored tree contains a non-directory directory entry",
            ));
        }
        directories.push(directory.clone());
        for entry in std::fs::read_dir(&directory)
            .map_err(|err| DbError::io("listing restored directory", err))?
        {
            let entry =
                entry.map_err(|err| DbError::io("reading restored directory entry", err))?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|err| DbError::io("inspecting restored entry", err))?;
            if metadata.file_type().is_symlink() {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "restored tree contains a symbolic link",
                ));
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "restored tree contains an unsupported file type",
                ));
            }
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        fsync_dir(&directory)?;
    }
    Ok(())
}

fn verify_restored_database(path: &Path) -> Result<()> {
    let mut config = crate::engine::EngineConfig::new(path);
    config.read_only = true;
    config.take_lock = false;
    config.repair = crate::engine::RepairPolicy::Strict;
    let engine = Engine::open(config)?;
    let report = engine.verify()?;
    if !report.ok {
        return Err(DbError::corrupt(
            "restored backup",
            report.issues.join("; "),
        ));
    }
    Ok(())
}

struct RestoreTempDirectory(PathBuf);

impl Drop for RestoreTempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn validate_restore_target(target: &Path, force: bool) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(DbError::io("checking restore target", err)),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(DbError::validation(
            "restore target must be a directory, not a symlink or file",
        ));
    }
    let entries =
        std::fs::read_dir(target).map_err(|err| DbError::io("reading restore target", err))?;
    for entry in entries {
        let entry = entry.map_err(|err| DbError::io("reading restore target entry", err))?;
        if entry.file_name() == crate::lock::LOCK_FILE {
            continue;
        }
        if !force {
            return Err(DbError::validation(format!(
                "target '{}' exists and is not empty; pass force=true to move it aside and restore",
                target.display()
            )));
        }
        break;
    }
    Ok(true)
}

const RESTORE_INTENT_FORMAT_VERSION: u64 = 1;
const MAX_RESTORE_INTENT_BYTES: usize = 64 * 1024;

struct RestoreIntent {
    path: PathBuf,
    parent: PathBuf,
}

impl RestoreIntent {
    fn create(target: &Path, staged: &Path, aside: &Path) -> Result<Self> {
        let (path, parent) = restore_intent_path(target)?;
        let target_name = target
            .file_name()
            .ok_or_else(|| DbError::validation("restore target must have a file name"))?;
        let staged_name = staged
            .file_name()
            .ok_or_else(|| DbError::validation("restore staging path must have a file name"))?;
        let aside_name = aside
            .file_name()
            .ok_or_else(|| DbError::validation("restore aside path must have a file name"))?;
        let body = Json::obj()
            .with("format_version", Json::uint(RESTORE_INTENT_FORMAT_VERSION))
            .with(
                "target",
                Json::str(target_name.to_string_lossy().to_string()),
            )
            .with(
                "staged",
                Json::str(staged_name.to_string_lossy().to_string()),
            )
            .with("aside", Json::str(aside_name.to_string_lossy().to_string()))
            .with("created_at", Json::int(now_ms()))
            .to_string();
        if body.len() > MAX_RESTORE_INTENT_BYTES {
            return Err(DbError::LimitExceeded {
                limit: "restore_intent_bytes",
                max: MAX_RESTORE_INTENT_BYTES as u64,
                actual: body.len() as u64,
            });
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|err| DbError::io("creating restore intent", err))?;
        use std::io::Write;
        file.write_all(body.as_bytes())
            .map_err(|err| DbError::io("writing restore intent", err))?;
        file.sync_all()
            .map_err(|err| DbError::io("syncing restore intent", err))?;
        fsync_dir(&parent)?;
        Ok(Self { path, parent })
    }

    fn complete(self) -> Result<()> {
        std::fs::remove_file(&self.path)
            .map_err(|err| DbError::io("removing completed restore intent", err))?;
        fsync_dir(&self.parent)
    }
}

fn restore_intent_path(target: &Path) -> Result<(PathBuf, PathBuf)> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let name = target
        .file_name()
        .ok_or_else(|| DbError::validation("restore target must have a file name"))?;
    let path = parent.join(format!(".{}.restore-intent", name.to_string_lossy()));
    Ok((path, parent))
}

/// Refuse to open a path while a previous restore publication still needs inspection.
pub(crate) fn ensure_no_restore_intent(target: &Path) -> Result<()> {
    let (path, _) = restore_intent_path(target)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(DbError::io("inspecting restore intent", err)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "restore intent is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| {
            file.take((MAX_RESTORE_INTENT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|err| DbError::io("reading restore intent", err))?;
    if bytes.len() > MAX_RESTORE_INTENT_BYTES {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "restore intent exceeds the maximum size",
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|err| DbError::corrupt("restore intent", format!("not UTF-8: {err}")))?;
    let value = preppydb_json::parse(text)
        .map_err(|err| DbError::corrupt("restore intent", err.to_string()))?;
    let version = value.get("format_version").and_then(Json::as_u64);
    let target_name = target.file_name().map(|name| name.to_string_lossy());
    if version != Some(RESTORE_INTENT_FORMAT_VERSION)
        || value.get_str("target") != target_name.as_deref()
        || value.get_str("staged").is_none()
        || value.get_str("aside").is_none()
    {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "restore intent fields are invalid or refer to a different target",
        ));
    }
    Err(DbError::RecoveryRequired(format!(
        "unfinished restore intent at {}; inspect the target, staged directory, and retained previous directory before removing the intent",
        path.display()
    )))
}

fn publish_restore(
    temp: &Path,
    target: &Path,
    aside: &Path,
    force: bool,
) -> Result<Option<String>> {
    publish_restore_with(temp, target, aside, force, fsync_dir)
}

fn publish_restore_with<F>(
    temp: &Path,
    target: &Path,
    aside: &Path,
    force: bool,
    mut sync_parent: F,
) -> Result<Option<String>>
where
    F: FnMut(&Path) -> Result<()>,
{
    let previous = if validate_restore_target(target, force)? {
        // Exclusive reservation avoids replacing a prior restore's retained data.
        create_restore_directory(aside)?;
        let previous = aside.join("previous");
        std::fs::rename(target, &previous)
            .map_err(|err| DbError::io("moving existing database aside", err))?;
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        if let Err(sync_error) = sync_parent(parent) {
            if let Err(rollback_error) = std::fs::rename(&previous, target)
                .and_then(|()| sync_parent(parent).map_err(std::io::Error::other))
            {
                return Err(DbError::RecoveryRequired(format!(
                    "syncing old restore target failed and rollback did not complete; prior database remains at {} (sync: {}; rollback: {})",
                    previous.display(), sync_error, rollback_error
                )));
            }
            return Err(DbError::Io {
                context: "syncing old restore target before publication".into(),
                detail: sync_error.to_string(),
            });
        }
        Some(previous)
    } else {
        None
    };
    if let Err(err) = std::fs::rename(temp, target) {
        if let Some(path) = &previous {
            // Roll back only if no other actor has installed a target meanwhile.
            match std::fs::symlink_metadata(target) {
                Err(ref target_error) if target_error.kind() == std::io::ErrorKind::NotFound => {
                    if let Err(rollback) = std::fs::rename(path, target) {
                        return Err(DbError::RecoveryRequired(format!(
                            "restore publication and rollback failed; old database retained at {} (publish: {err}; rollback: {rollback})",
                            path.display()
                        )));
                    }
                    let parent = target.parent().unwrap_or_else(|| Path::new("."));
                    if let Err(sync_error) = sync_parent(parent) {
                        return Err(DbError::RecoveryRequired(format!(
                            "restore publication failed and the old database was restored, but syncing its parent failed: {sync_error} (publish: {err})"
                        )));
                    }
                }
                _ => {
                    return Err(DbError::RecoveryRequired(format!(
                        "restore publication failed and a target appeared before rollback; old database retained at {} (publish: {err})",
                        path.display()
                    )));
                }
            }
        }
        return Err(DbError::io("moving restored database into place", err));
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = sync_parent(parent) {
        let previous_description = previous
            .as_ref()
            .map(|path| format!(" prior database retained at {}", path.display()))
            .unwrap_or_default();
        return Err(DbError::RestoreOutcomeUnknown(format!(
            "new target '{}' is visible, but syncing its parent failed: {error}.{previous_description}",
            target.display()
        )));
    }
    Ok(previous.map(|path| path.display().to_string()))
}

// The archive can change after the initial verification. Verify the exact bytes
// copied before publishing the restored directory.
fn copy_verified_member(
    input: &mut impl Read,
    output: &mut impl Write,
    entry: &BackupEntry,
) -> Result<()> {
    let mut remaining = entry.len;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut digest = Sha256::new();
    while remaining > 0 {
        let want = (buffer.len() as u64).min(remaining) as usize;
        read_archive_exact(input, &mut buffer[..want], "member")?;
        digest.update(&buffer[..want]);
        output
            .write_all(&buffer[..want])
            .map_err(|err| DbError::io("writing restored file", err))?;
        remaining -= want as u64;
    }
    if preppydb_crypto::hex_encode(&digest.finish()) != entry.sha256 {
        return Err(DbError::corrupt(
            "backup",
            format!("member '{}' changed during restore", entry.path),
        ));
    }
    Ok(())
}

// Never reuse or remove an existing path: it could belong to another restore or
// be a symlink planted by a local user. Restrict access where the OS supports it.
fn create_restore_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();

    builder
        .create(path)
        .map_err(|err| DbError::io("creating exclusive restore temp dir", err))
}

/// Hash of an archive (helper for tests/tools).
pub fn archive_sha256(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| DbError::io("reading archive", err))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(preppydb_crypto::hex_encode(&digest.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use preppydb_crypto::sha256::sha256;

    #[test]
    fn truncated_archive_prefix_is_classified_as_corruption() {
        let root = tempdir("truncated-prefix");
        let archive = root.join("truncated.pdbbak");
        let mut incomplete_header = ARCHIVE_MAGIC.to_vec();
        incomplete_header.extend_from_slice(&5u32.to_le_bytes());
        incomplete_header.extend_from_slice(b"{}");
        for bytes in [
            ARCHIVE_MAGIC[..1].to_vec(),
            [&ARCHIVE_MAGIC[..], &[1u8]].concat(),
            incomplete_header,
        ] {
            std::fs::write(&archive, bytes).unwrap();
            let error = match open_archive(&archive) {
                Ok(_) => panic!("truncated archive was accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_reader_keeps_real_read_errors_distinct_from_truncation() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected read failure"))
            }
        }
        let error = read_archive_exact(&mut FailingReader, &mut [0u8; 1], "member").unwrap_err();
        assert_eq!(error.code(), crate::error::codes::IO_ERROR);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "preppydb-backup-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn backup_rejects_destination_inside_source_before_checkpoint_or_directory_creation() {
        let root = tempdir("destination-inside-source");
        let engine = Engine::open(crate::engine::EngineConfig::new(&root)).unwrap();
        let generation_before = engine.stats().generation;
        let manifest = crate::manifest::Manifest::generation_path(&root, 1);

        let error = create_backup(&engine, &manifest, true).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::VALIDATION_FAILED);
        assert_eq!(engine.stats().generation, generation_before);
        assert!(
            !manifest.exists(),
            "rejected output must not replace or create a manifest"
        );

        let nested_output = root
            .join("not-created")
            .join("subdir")
            .join("backup.pdbbak");
        let error = create_backup(&engine, &nested_output, false).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::VALIDATION_FAILED);
        assert!(!nested_output.parent().unwrap().exists());
        assert!(engine.verify().unwrap().ok);

        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backup_destination_resolves_parent_symlinks_before_checkpoint() {
        use std::os::unix::fs::symlink;

        let root = tempdir("destination-parent-symlink");
        let alias = root.with_extension("alias");
        symlink(&root, &alias).unwrap();
        let engine = Engine::open(crate::engine::EngineConfig::new(&root)).unwrap();
        let generation_before = engine.stats().generation;
        let destination = alias.join("nested").join("backup.pdbbak");

        let error = create_backup(&engine, &destination, true).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::VALIDATION_FAILED);
        assert_eq!(engine.stats().generation, generation_before);
        assert!(!root.join("nested").exists());

        drop(engine);
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn corrupt_snapshot_and_reseal_archive(archive: &Path) {
        let mut bytes = std::fs::read(archive).unwrap();
        assert_eq!(&bytes[..8], ARCHIVE_MAGIC);
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let data_start = 12 + header_len;
        let header_text = std::str::from_utf8(&bytes[12..data_start]).unwrap();
        let mut header = preppydb_json::parse(header_text).unwrap();
        let entries = header
            .get("entries")
            .and_then(Json::as_array)
            .unwrap()
            .to_vec();
        let mut member_offset = data_start;
        let mut changed = false;
        for (index, entry) in entries.iter().enumerate() {
            let len = usize::try_from(entry.get("len").and_then(Json::as_u64).unwrap()).unwrap();
            if entry.get_str("path").unwrap().starts_with("snap/") {
                assert!(len > 0);
                bytes[member_offset] ^= 0xff;
                let checksum = preppydb_crypto::hex_encode(&sha256(
                    &bytes[member_offset..member_offset + len],
                ));
                header
                    .get_mut("entries")
                    .and_then(Json::as_array_mut)
                    .unwrap()[index]
                    .set("sha256", Json::str(checksum));
                changed = true;
                break;
            }
            member_offset += len;
        }
        assert!(changed, "backup fixture should include a snapshot");

        let new_header = header.to_string().into_bytes();
        assert_eq!(
            new_header.len(),
            header_len,
            "checksum replacement is fixed width"
        );
        bytes[12..data_start].copy_from_slice(&new_header);
        let trailer = bytes.len() - 32;
        let archive_digest = sha256(&bytes[..trailer]);
        bytes[trailer..].copy_from_slice(&archive_digest);
        std::fs::write(archive, bytes).unwrap();
    }

    #[test]
    fn restore_publication_preserves_existing_data_on_collision_and_failure() {
        let root = std::env::temp_dir().join(format!(
            "preppydb-publish-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("database");
        let aside = root.join("aside");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("old"), b"old").unwrap();
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("keep"), b"keep").unwrap();
        assert!(publish_restore(&root.join("missing"), &target, &aside, true).is_err());
        assert_eq!(std::fs::read(aside.join("keep")).unwrap(), b"keep");
        assert_eq!(std::fs::read(target.join("old")).unwrap(), b"old");

        let fresh_aside = root.join("fresh-aside");
        assert!(publish_restore(&root.join("missing"), &target, &fresh_aside, true).is_err());
        assert_eq!(std::fs::read(target.join("old")).unwrap(), b"old");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_publication_classifies_post_rename_sync_failure_as_unknown() {
        let root = tempdir("publish-sync-failure");
        let target = root.join("database");
        let aside = root.join("aside");
        let staged = root.join("staged");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("old"), b"old").unwrap();
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("new"), b"new").unwrap();
        let intent = RestoreIntent::create(&target, &staged, &aside).unwrap();

        let mut sync_calls = 0;
        let error = publish_restore_with(&staged, &target, &aside, true, |_| {
            sync_calls += 1;
            if sync_calls == 2 {
                return Err(DbError::io(
                    "injected parent sync failure",
                    std::io::Error::other("injected"),
                ));
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(error.code(), crate::error::codes::RESTORE_OUTCOME_UNKNOWN);
        assert_eq!(std::fs::read(target.join("new")).unwrap(), b"new");
        let previous = aside.join("previous");
        assert_eq!(std::fs::read(previous.join("old")).unwrap(), b"old");
        let open_error = match Engine::open(crate::engine::EngineConfig::new(&target)) {
            Ok(_) => panic!("an unfinished restore intent must block database open"),
            Err(error) => error,
        };
        assert_eq!(open_error.code(), crate::error::codes::RECOVERY_REQUIRED);
        assert!(intent.path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_publication_rolls_back_when_syncing_old_target_fails() {
        let root = tempdir("publish-old-sync-failure");
        let target = root.join("database");
        let aside = root.join("aside");
        let staged = root.join("staged");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("old"), b"old").unwrap();
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("new"), b"new").unwrap();

        let mut sync_calls = 0;
        let error = publish_restore_with(&staged, &target, &aside, true, |_| {
            sync_calls += 1;
            if sync_calls == 1 {
                return Err(DbError::io(
                    "injected parent sync failure",
                    std::io::Error::other("injected"),
                ));
            }
            Ok(())
        })
        .unwrap_err();

        assert_ne!(error.code(), crate::error::codes::RESTORE_OUTCOME_UNKNOWN);
        assert_eq!(std::fs::read(target.join("old")).unwrap(), b"old");
        assert!(!target.join("new").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_publication_syncs_rollback_after_staged_rename_fails() {
        for fail_rollback_sync in [false, true] {
            let root = tempdir("publish-rollback-sync");
            let target = root.join("database");
            let aside = root.join("aside");
            let missing_stage = root.join("missing-stage");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("old"), b"old").unwrap();
            let intent = RestoreIntent::create(&target, &missing_stage, &aside).unwrap();
            let intent_path = intent.path.clone();

            let mut sync_calls = 0;
            let publication = publish_restore_with(&missing_stage, &target, &aside, true, |_| {
                sync_calls += 1;
                if fail_rollback_sync && sync_calls == 2 {
                    return Err(DbError::io(
                        "injected rollback sync failure",
                        std::io::Error::other("injected"),
                    ));
                }
                Ok(())
            });
            assert_eq!(sync_calls, 2, "move and rollback must both be synced");
            assert_eq!(std::fs::read(target.join("old")).unwrap(), b"old");

            let error = complete_restore_publication(intent, publication, &target).unwrap_err();
            if fail_rollback_sync {
                assert_eq!(error.code(), crate::error::codes::RECOVERY_REQUIRED);
                assert!(
                    intent_path.exists(),
                    "uncertain rollback must retain intent"
                );
            } else {
                assert_eq!(error.code(), crate::error::codes::IO_ERROR);
                assert!(!intent_path.exists(), "synced rollback may clear intent");
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn restore_intent_is_bounded_and_blocks_empty_database_initialization() {
        let root = tempdir("restore-intent-blocks-open");
        let target = root.join("database");
        let staged = root.join("staged");
        let aside = root.join("aside");
        let intent = RestoreIntent::create(&target, &staged, &aside).unwrap();

        let error = match Engine::open(crate::engine::EngineConfig::new(&target)) {
            Ok(_) => panic!("an unfinished restore intent must block empty initialization"),
            Err(error) => error,
        };
        assert_eq!(error.code(), crate::error::codes::RECOVERY_REQUIRED);
        assert!(!target.exists());
        assert!(intent.path.exists());

        intent.complete().unwrap();
        let engine = Engine::open(crate::engine::EngineConfig::new(&target)).unwrap();
        assert_eq!(engine.stats().total_documents, 0);
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn definite_restore_failure_clears_intent_but_uncertain_failure_keeps_it() {
        let root = tempdir("restore-intent-failure-classification");
        let target = root.join("database");
        let staged = root.join("staged");
        let aside = root.join("aside");
        let first_intent = RestoreIntent::create(&target, &staged, &aside).unwrap();
        let first_path = first_intent.path.clone();
        let definite = DbError::io(
            "injected pre-publication failure",
            std::io::Error::other("injected"),
        );
        assert!(complete_restore_publication(first_intent, Err(definite), &target).is_err());
        assert!(!first_path.exists());

        let second_intent = RestoreIntent::create(&target, &staged, &aside).unwrap();
        let second_path = second_intent.path.clone();
        let uncertain = DbError::RestoreOutcomeUnknown("injected post-publication failure".into());
        assert!(complete_restore_publication(second_intent, Err(uncertain), &target).is_err());
        assert!(second_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_publication_process_crashes_fail_closed_at_rename_boundaries() {
        // The ARM64 workflow uses qemu-user without binfmt_misc, so it cannot
        // re-execute this test binary as a child. The same crash cases run in the
        // native Linux and Windows jobs; only the emulated job opts out explicitly.
        if std::env::var_os("PREPPYDB_SKIP_CHILD_CRASH").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        for phase in [
            "before_publish",
            "after_old_target_move",
            "after_new_target_move",
        ] {
            let root = tempdir(&format!("restore-process-crash-{phase}"));
            let target = root.join("database");
            let staged = root.join("staged");
            let aside = root.join("aside");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("old"), b"old-state").unwrap();
            std::fs::create_dir(&staged).unwrap();
            std::fs::write(staged.join("new"), b"new-state").unwrap();

            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "backup::tests::restore_publication_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PREPPYDB_RESTORE_CRASH_ROOT", &root)
                .env("PREPPYDB_RESTORE_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "child should abort at {phase}; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let intent_path = restore_intent_path(&target).unwrap().0;
            assert!(
                intent_path.exists(),
                "missing recovery marker at {phase}; child stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let open_error = match Engine::open(crate::engine::EngineConfig::new(&target)) {
                Ok(_) => panic!("open must refuse an interrupted restore at {phase}"),
                Err(error) => error,
            };
            assert_eq!(open_error.code(), crate::error::codes::RECOVERY_REQUIRED);

            match phase {
                "before_publish" => {
                    assert_eq!(std::fs::read(target.join("old")).unwrap(), b"old-state");
                    assert_eq!(std::fs::read(staged.join("new")).unwrap(), b"new-state");
                    assert!(!aside.exists());
                }
                "after_old_target_move" => {
                    assert!(!target.exists());
                    assert_eq!(
                        std::fs::read(aside.join("previous/old")).unwrap(),
                        b"old-state"
                    );
                    assert_eq!(std::fs::read(staged.join("new")).unwrap(), b"new-state");
                }
                "after_new_target_move" => {
                    assert_eq!(std::fs::read(target.join("new")).unwrap(), b"new-state");
                    assert_eq!(
                        std::fs::read(aside.join("previous/old")).unwrap(),
                        b"old-state"
                    );
                    assert!(!staged.exists());
                }
                _ => unreachable!(),
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// Child-process crash helper for `restore_publication_process_crashes_fail_closed_at_rename_boundaries`.
    #[test]
    #[ignore = "spawned by restore_publication_process_crashes_fail_closed_at_rename_boundaries"]
    fn restore_publication_crash_child() {
        let Ok(root) = std::env::var("PREPPYDB_RESTORE_CRASH_ROOT") else {
            return;
        };
        let phase = std::env::var("PREPPYDB_RESTORE_CRASH_PHASE").unwrap();
        let root = PathBuf::from(root);
        let target = root.join("database");
        let staged = root.join("staged");
        let aside = root.join("aside");
        let _intent = RestoreIntent::create(&target, &staged, &aside).unwrap();
        if phase == "before_publish" {
            std::process::abort();
        }
        let mut sync_calls = 0;
        let _ = publish_restore_with(&staged, &target, &aside, true, |parent| {
            sync_calls += 1;
            if (phase == "after_old_target_move" && sync_calls == 1)
                || (phase == "after_new_target_move" && sync_calls == 2)
            {
                std::process::abort();
            }
            fsync_dir(parent)
        });
        panic!("crash phase '{phase}' was not reached");
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_symlink_target() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "preppydb-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        std::fs::create_dir(&original).unwrap();
        let target = root.join("link");
        symlink(&original, &target).unwrap();
        assert!(validate_restore_target(&target, true).is_err());
        assert!(target.symlink_metadata().unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backup_member_collection_rejects_symlinks_and_directory_errors() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "preppydb-backup-members-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let outside = root.join("outside");
        let data = root.join("data");
        std::fs::create_dir_all(outside.join("snap")).unwrap();
        std::fs::create_dir_all(outside.join("wal")).unwrap();
        std::fs::write(outside.join("snap/secret.pdb"), b"sensitive").unwrap();
        std::fs::create_dir(&data).unwrap();
        symlink(outside.join("snap/secret.pdb"), data.join("manifest-link")).unwrap();
        std::fs::create_dir(data.join("snap")).unwrap();
        symlink(outside.join("wal"), data.join("wal")).unwrap();
        assert!(collect_members(&data).is_err());

        std::fs::remove_file(data.join("manifest-link")).unwrap();
        std::fs::remove_file(data.join("wal")).unwrap();
        symlink(
            outside.join("snap/secret.pdb"),
            data.join("snap/linked.pdb"),
        )
        .unwrap();
        std::fs::create_dir(data.join("wal")).unwrap();
        assert!(collect_members(&data).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_member_collection_rejects_storage_directories_that_could_hide_data() {
        let root = tempdir("backup-hidden-storage-entry");
        let data = root.join("data");
        std::fs::create_dir_all(data.join("snap")).unwrap();
        std::fs::create_dir_all(data.join("wal/000000000001.wal")).unwrap();

        let error = collect_members(&data).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_layout_rejects_bad_lengths_and_duplicates() {
        let entry = BackupEntry {
            path: "MANIFEST.json".into(),
            len: 4,
            sha256: "a".repeat(64),
        };
        let wal = BackupEntry {
            path: "wal/000000000001.wal".into(),
            len: 0,
            sha256: "b".repeat(64),
        };
        assert!(validate_archive_layout(&[entry.clone(), wal.clone()], 12, 56).is_ok());
        assert!(validate_archive_layout(&[entry.clone(), wal.clone()], 12, 57).is_err());
        assert!(
            validate_archive_layout(&[entry.clone(), entry.clone(), wal.clone()], 12, 60).is_err()
        );
        // Before its first checkpoint, a valid database may have no manifest, but
        // it still has its initial WAL segment.
        assert!(validate_archive_layout(&[], 12, 52).is_err());
        let mut overflow = entry;
        overflow.len = u64::MAX;
        assert!(validate_archive_layout(&[wal, overflow], 12, u64::MAX).is_err());
    }

    #[test]
    fn archive_entry_requires_length_and_checksum() {
        assert!(
            BackupEntry::from_json(&Json::obj().with("path", Json::str("MANIFEST.json"))).is_err()
        );
    }

    #[test]
    fn restore_rechecks_bytes_copied_after_initial_verification() {
        let entry = BackupEntry {
            path: "MANIFEST.json".into(),
            len: 4,
            sha256: preppydb_crypto::hex_encode(&sha256(b"good")),
        };
        let mut output = Vec::new();
        assert!(copy_verified_member(&mut &b"evil"[..], &mut output, &entry).is_err());
        assert_eq!(output, b"evil");
        let mut output = Vec::new();
        let error = copy_verified_member(&mut &b"go"[..], &mut output, &entry).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        let mut output = Vec::new();
        copy_verified_member(&mut &b"good"[..], &mut output, &entry).unwrap();
        assert_eq!(output, b"good");
    }

    #[test]
    fn backup_ignores_stale_legacy_temporary_file_and_publishes_verified_archive() {
        let root = tempdir("stale-backup-temp");
        let source = root.join("source");
        let engine = Engine::open(crate::engine::EngineConfig::new(&source)).unwrap();
        engine
            .create_collection(crate::catalog::CollectionMeta::new("items", 0))
            .unwrap();
        engine
            .put(
                "items",
                "one",
                Json::str("value"),
                crate::engine::PutOptions::default(),
            )
            .unwrap();

        let archive = root.join("database.bak");
        let stale_temp = archive.with_extension("bak.tmp");
        std::fs::write(&stale_temp, b"interrupted previous backup").unwrap();
        create_backup(&engine, &archive, false).unwrap();

        assert_eq!(
            std::fs::read(&stale_temp).unwrap(),
            b"interrupted previous backup"
        );
        assert_eq!(
            verify_backup(&archive)
                .unwrap()
                .get("ok")
                .and_then(Json::as_bool),
            Some(true)
        );
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".preppydb-backup-")
        }));
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_does_not_publish_a_semantically_corrupt_snapshot() {
        let root = tempdir("corrupt-source-snapshot");
        let source = root.join("source");
        let engine = Engine::open(crate::engine::EngineConfig::new(&source)).unwrap();
        engine
            .create_collection(crate::catalog::CollectionMeta::new("items", 0))
            .unwrap();
        engine
            .put(
                "items",
                "one",
                Json::str("value"),
                crate::engine::PutOptions::default(),
            )
            .unwrap();
        engine.checkpoint().unwrap();

        let manifest = crate::manifest::Manifest::load(&source).unwrap().unwrap();
        let snapshot = source.join("snap").join(manifest.snapshot.unwrap().file);
        let mut bytes = std::fs::read(&snapshot).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&snapshot, bytes).unwrap();

        let archive = root.join("database.pdbbak");
        assert!(create_backup(&engine, &archive, false).is_err());
        assert!(!archive.exists(), "invalid backup must not be published");
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_rejects_semantically_corrupt_database_before_replacing_target() {
        let root = tempdir("semantic-corruption");
        let source = root.join("source");
        let engine = Engine::open(crate::engine::EngineConfig::new(&source)).unwrap();
        engine
            .create_collection(crate::catalog::CollectionMeta::new("items", 0))
            .unwrap();
        engine
            .put(
                "items",
                "one",
                Json::str("value"),
                crate::engine::PutOptions::default(),
            )
            .unwrap();
        let archive = root.join("database.bak");
        create_backup(&engine, &archive, true).unwrap();
        drop(engine);

        corrupt_snapshot_and_reseal_archive(&archive);
        assert!(verify_backup(&archive).is_err());

        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"original database marker").unwrap();
        assert!(restore_backup(&archive, &target, true).is_err());
        assert_eq!(
            std::fs::read(target.join("keep")).unwrap(),
            b"original database marker"
        );
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".target.restore-")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_temp_collision_preserves_existing_directory() {
        let path = std::env::temp_dir().join(format!(
            "preppydb-restore-collision-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir(&path).unwrap();
        let marker = path.join("keep");
        std::fs::write(&marker, b"preserve").unwrap();
        assert!(create_restore_directory(&path).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"preserve");
        std::fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restore_temp_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "preppydb-restore-symlink-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir(&root).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = root.join("restore");
        symlink(&outside, &link).unwrap();
        assert!(create_restore_directory(&link).is_err());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn magic_constants_are_stable() {
        assert_eq!(ARCHIVE_MAGIC.len(), 8);
        assert_eq!(ARCHIVE_END.len(), 8);
    }

    #[test]
    fn backup_members_cannot_escape_restore_root_on_any_platform() {
        for path in [
            "../outside",
            "wal/../outside",
            "wal/..\\outside",
            "..\\outside",
            "C:\\outside",
            "C:/outside",
            "/etc/passwd",
            "wal//file.wal",
            "snap/snap-1.pdb/extra",
            "wal/file.wal",
            "MANIFEST-000000000001.json/extra",
        ] {
            assert!(!valid_member_path(path), "accepted {path}");
        }
        for path in [
            "MANIFEST.json",
            "MANIFEST-000000000001.json",
            "wal/000000000001.wal",
            "snap/snap-1.pdb",
        ] {
            assert!(valid_member_path(path), "rejected {path}");
        }
    }
}
