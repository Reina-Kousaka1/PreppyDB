//! Write-ahead log: framed, checksummed, segmented append-only files.
//!
//! Frame layout (little endian):
//!
//! ```text
//! [ payload_len: u32 ][ crc32c(payload): u32 ][ payload: payload_len bytes ]
//! ```
//!
//! A frame is only valid when the CRC matches. On recovery a *torn tail* (partial
//! frame at the end of a segment, which is exactly what a crash mid-write leaves
//! behind) is truncated; anything else is reported as corruption and never silently
//! dropped.

use crate::error::{DbError, Result};
use crate::record::Record;
use preppydb_crypto::crc32c;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Segment file header: magic + format version.
pub const WAL_MAGIC: &[u8; 8] = b"PPDBWAL1";
pub const WAL_HEADER_LEN: u64 = 12;
/// Hard upper bound for a single frame (protects the reader from bogus lengths).
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;
/// Maximum segment size accepted by the collecting compatibility API.
/// Use `scan_segment_visit` or `summarize_segment` for larger segments.
pub const MAX_COLLECTED_SEGMENT_BYTES: u64 = 128 * 1024 * 1024;

/// Durability policy for commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// Call `sync_data` before acknowledging every commit. This does not guarantee
    /// power-loss durability when the OS, filesystem, device, or controller does not
    /// honor flush requests.
    Always,
    /// A background worker attempts `fsync` every N milliseconds; commits may be
    /// acknowledged before that sync and can be lost if the process or OS fails first.
    Interval(u64),
    /// Never call `fsync` explicitly; relies on the OS page cache.
    Never,
}

impl SyncMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncMode::Always => "always",
            SyncMode::Interval(_) => "interval",
            SyncMode::Never => "never",
        }
    }

    pub fn parse(text: &str) -> Result<SyncMode> {
        match text.to_ascii_lowercase().as_str() {
            "always" | "sync" => Ok(SyncMode::Always),
            "never" | "none" | "off" => Ok(SyncMode::Never),
            "interval" => Ok(SyncMode::Interval(200)),
            other => {
                if let Some(rest) = other.strip_prefix("interval:") {
                    let ms: u64 = rest
                        .parse()
                        .map_err(|_| DbError::Config(format!("invalid sync interval '{rest}'")))?;
                    if ms == 0 {
                        return Err(DbError::Config("sync interval must be > 0 ms".into()));
                    }
                    Ok(SyncMode::Interval(ms))
                } else {
                    Err(DbError::Config(format!("unknown sync mode '{other}'")))
                }
            }
        }
    }

    pub fn is_always(&self) -> bool {
        matches!(self, SyncMode::Always)
    }
}

const MAX_SEGMENT_INDEX: u64 = 999_999_999_999;

/// Twelve-digit segment file name, e.g. `wal/000000000042.wal`.
pub fn segment_name(index: u64) -> String {
    format!("{index:012}.wal")
}

pub fn parse_segment_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".wal")?;
    if stem.len() != 12 || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    stem.parse::<u64>()
        .ok()
        .filter(|index| (1..=MAX_SEGMENT_INDEX).contains(index))
}

fn validate_segment_index(index: u64) -> Result<()> {
    if index == 0 {
        return Err(DbError::Config("WAL segment index must be positive".into()));
    }
    if index > MAX_SEGMENT_INDEX {
        return Err(DbError::LimitExceeded {
            limit: "wal_segment_index",
            max: MAX_SEGMENT_INDEX,
            actual: index,
        });
    }
    Ok(())
}

/// List existing WAL segments in append order.
pub fn list_segments(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut segments = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(segments),
        Err(err) => return Err(DbError::io(format!("reading {}", dir.display()), err)),
    };
    for entry in entries {
        let entry = entry.map_err(|err| DbError::io("reading WAL directory entry", err))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.to_ascii_lowercase().ends_with(".wal") {
            if parse_segment_name(&name).is_none() {
                return Err(DbError::corrupt(
                    entry.path().display().to_string(),
                    "WAL segment has a noncanonical file name",
                ));
            }
            let file_type = entry
                .file_type()
                .map_err(|err| DbError::io("inspecting WAL segment", err))?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(DbError::corrupt(
                    entry.path().display().to_string(),
                    "WAL segment is not a regular file",
                ));
            }
            segments.push(entry.path());
        }
    }
    segments.sort();
    Ok(segments)
}

/// Append-only WAL writer with automatic segment rotation.
pub struct WalWriter {
    dir: PathBuf,
    file: File,
    current: PathBuf,
    current_index: u64,
    current_len: u64,
    max_segment_bytes: u64,
    sync_mode: SyncMode,
    /// Total frames appended since the last sync (diagnostics only).
    unsynced_frames: u64,
    #[cfg(test)]
    fail_next_append: Option<AppendFault>,
    #[cfg(test)]
    fail_next_sync: bool,
    #[cfg(test)]
    successful_syncs: u64,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum AppendFault {
    BeforeWrite,
    PartialWrite,
    BeforeSync,
}

impl WalWriter {
    /// Open (or create) the segment with `index` and append to it.
    pub fn open(
        dir: &Path,
        index: u64,
        max_segment_bytes: u64,
        sync_mode: SyncMode,
    ) -> Result<WalWriter> {
        validate_segment_index(index)?;
        Self::open_with_directory_sync(
            dir,
            index,
            max_segment_bytes,
            sync_mode,
            crate::snapshot::fsync_dir,
            crate::fsutil::move_path,
        )
    }

    fn open_with_directory_sync<F>(
        dir: &Path,
        index: u64,
        max_segment_bytes: u64,
        sync_mode: SyncMode,
        sync_directory: F,
        publish_segment: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<WalWriter>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        std::fs::create_dir_all(dir)
            .map_err(|err| DbError::io(format!("creating {}", dir.display()), err))?;
        let path = dir.join(segment_name(index));
        let existed = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
            Ok(_) => {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "WAL segment is not a regular file",
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(DbError::io("inspecting WAL segment", err)),
        };
        let mut file = if existed {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|err| DbError::io(format!("opening {}", path.display()), err))?
        } else {
            let mut staged = path.clone();
            let mut staged_name = path
                .file_name()
                .ok_or_else(|| DbError::validation("WAL segment path has no file name"))?
                .to_os_string();
            staged_name.push(format!(
                ".tmp-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            staged.set_file_name(staged_name);
            let _staged_guard = StagedWalSegment(staged.clone());
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&staged)
                .map_err(|err| DbError::io("creating staged WAL segment", err))?;
            file.write_all(WAL_MAGIC)
                .map_err(|err| DbError::io("writing staged WAL header", err))?;
            file.write_all(&1u32.to_le_bytes())
                .map_err(|err| DbError::io("writing staged WAL header", err))?;
            file.sync_all()
                .map_err(|err| DbError::io("syncing staged WAL header", err))?;
            drop(file);
            publish_segment(&staged, &path)
                .map_err(|err| DbError::io("publishing WAL segment", err))?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|err| DbError::io(format!("opening {}", path.display()), err))?
        };
        let len = file
            .metadata()
            .map_err(|err| DbError::io("stat WAL segment", err))?
            .len();
        if len == 0 {
            // Fresh segment: write the header.
            file.seek(SeekFrom::Start(0))
                .map_err(|err| DbError::io("seeking WAL segment", err))?;
            file.write_all(WAL_MAGIC)
                .map_err(|err| DbError::io("writing WAL header", err))?;
            file.write_all(&1u32.to_le_bytes())
                .map_err(|err| DbError::io("writing WAL header", err))?;
            file.sync_all()
                .map_err(|err| DbError::io("syncing WAL header", err))?;
        }
        // Even an existing segment can be a remnant of a previous creation whose
        // directory sync failed. Its presence after that failure does not prove the
        // filename durable. Recheck the barrier before publishing any writer.
        sync_directory(dir)?;
        let current_len = file
            .metadata()
            .map_err(|err| DbError::io("stat WAL segment", err))?
            .len();
        if current_len < WAL_HEADER_LEN {
            return Err(DbError::corrupt(
                path.display().to_string(),
                "WAL segment is shorter than its header",
            ));
        }
        file.seek(SeekFrom::End(0))
            .map_err(|err| DbError::io("seeking WAL segment", err))?;
        Ok(WalWriter {
            dir: dir.to_path_buf(),
            file,
            current: path,
            current_index: index,
            current_len,
            max_segment_bytes,
            sync_mode,
            unsynced_frames: 0,
            #[cfg(test)]
            fail_next_append: None,
            #[cfg(test)]
            fail_next_sync: false,
            #[cfg(test)]
            successful_syncs: 0,
        })
    }

    /// Open an existing WAL segment without write access or initialization.
    ///
    /// Read-only database handles still keep the writer slot for shared engine
    /// structure, but must never create a segment or write its header as a side effect.
    pub fn open_read_only(
        dir: &Path,
        index: u64,
        max_segment_bytes: u64,
        sync_mode: SyncMode,
    ) -> Result<WalWriter> {
        validate_segment_index(index)?;
        let path = dir.join(segment_name(index));
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|err| DbError::io(format!("inspecting {}", path.display()), err))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(DbError::corrupt(
                path.display().to_string(),
                "WAL segment is not a regular file",
            ));
        }
        let mut file = File::open(&path)
            .map_err(|err| DbError::io(format!("opening {} read-only", path.display()), err))?;
        let current_len = file
            .metadata()
            .map_err(|err| DbError::io("stat WAL segment", err))?
            .len();
        if current_len < WAL_HEADER_LEN {
            return Err(DbError::corrupt(
                path.display().to_string(),
                "WAL segment is shorter than its header",
            ));
        }
        file.seek(SeekFrom::End(0))
            .map_err(|err| DbError::io("seeking WAL segment", err))?;
        Ok(WalWriter {
            dir: dir.to_path_buf(),
            file,
            current: path,
            current_index: index,
            current_len,
            max_segment_bytes,
            sync_mode,
            unsynced_frames: 0,
            #[cfg(test)]
            fail_next_append: None,
            #[cfg(test)]
            fail_next_sync: false,
            #[cfg(test)]
            successful_syncs: 0,
        })
    }

    pub fn current_index(&self) -> u64 {
        self.current_index
    }

    pub fn current_path(&self) -> &Path {
        &self.current
    }

    pub fn current_len(&self) -> u64 {
        self.current_len
    }

    pub fn sync_mode(&self) -> SyncMode {
        self.sync_mode
    }

    pub(crate) fn has_unsynced_frames(&self) -> bool {
        self.unsynced_frames != 0
    }

    #[cfg(test)]
    pub(crate) fn successful_syncs(&self) -> u64 {
        self.successful_syncs
    }

    /// Append one record frame, rotating to a new segment when the cap is reached.
    pub fn append(&mut self, record: &Record) -> Result<()> {
        #[cfg(test)]
        let fault = self.fail_next_append.take();
        #[cfg(test)]
        if matches!(fault, Some(AppendFault::BeforeWrite)) {
            return Err(DbError::io(
                "injected WAL append failure",
                std::io::Error::other("injected"),
            ));
        }
        let payload = record.encode_payload();
        if payload.len() as u64 > MAX_FRAME_LEN as u64 {
            return Err(DbError::LimitExceeded {
                limit: "wal_frame_bytes",
                max: MAX_FRAME_LEN as u64,
                actual: payload.len() as u64,
            });
        }
        // Rotate *before* writing so a single record is never split across segments.
        let frame_len = payload.len() as u64 + 8;
        let projected_len =
            self.current_len
                .checked_add(frame_len)
                .ok_or(DbError::LimitExceeded {
                    limit: "wal_segment_bytes",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
        if self.current_len > WAL_HEADER_LEN && projected_len > self.max_segment_bytes {
            self.rotate()?;
        }
        let write_len = self
            .current_len
            .checked_add(frame_len)
            .ok_or(DbError::LimitExceeded {
                limit: "wal_segment_bytes",
                max: u64::MAX,
                actual: u64::MAX,
            })?;
        let next_unsynced_frames =
            self.unsynced_frames
                .checked_add(1)
                .ok_or(DbError::LimitExceeded {
                    limit: "wal_unsynced_frames",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
        let mut frame = Vec::with_capacity(frame_len as usize);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32c(&payload, 0).to_le_bytes());
        frame.extend_from_slice(&payload);
        #[cfg(test)]
        if matches!(fault, Some(AppendFault::PartialWrite)) {
            self.file
                .write_all(&frame[..frame.len() / 2])
                .map_err(|err| DbError::io("injected partial WAL write", err))?;
            return Err(DbError::io(
                "injected partial WAL write",
                std::io::Error::other("injected"),
            ));
        }
        self.file
            .write_all(&frame)
            .map_err(|err| DbError::io(format!("appending to {}", self.current.display()), err))?;
        self.current_len = write_len;
        self.unsynced_frames = next_unsynced_frames;
        #[cfg(test)]
        if matches!(fault, Some(AppendFault::BeforeSync)) {
            return Err(DbError::io(
                "injected sync failure",
                std::io::Error::other("injected"),
            ));
        }
        if self.sync_mode.is_always() {
            self.sync()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_append_failure(&mut self) {
        self.fail_next_append = Some(AppendFault::BeforeWrite);
    }

    #[cfg(test)]
    pub(crate) fn inject_fault(&mut self, fault: AppendFault) {
        self.fail_next_append = Some(fault);
    }

    #[cfg(test)]
    pub(crate) fn inject_sync_failure(&mut self) {
        self.fail_next_sync = true;
    }

    /// `fsync` the current segment.
    pub fn sync(&mut self) -> Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(DbError::io(
                "injected WAL sync failure",
                std::io::Error::other("injected"),
            ));
        }
        self.file
            .sync_data()
            .map_err(|err| DbError::io(format!("syncing {}", self.current.display()), err))?;
        self.unsynced_frames = 0;
        #[cfg(test)]
        {
            self.successful_syncs = self.successful_syncs.saturating_add(1);
        }
        Ok(())
    }

    /// Start a new segment (used by rotation and by checkpoints).
    pub fn rotate(&mut self) -> Result<()> {
        self.sync()?;
        let next = self
            .current_index
            .checked_add(1)
            .ok_or(DbError::LimitExceeded {
                limit: "wal_segment_index",
                max: u64::MAX,
                actual: u64::MAX,
            })?;
        *self = WalWriter::open(&self.dir, next, self.max_segment_bytes, self.sync_mode)?;
        Ok(())
    }

    /// Truncate the current segment to `len` bytes (recovery of a torn tail).
    pub fn truncate_current(&mut self, len: u64) -> Result<()> {
        self.file
            .set_len(len)
            .map_err(|err| DbError::io("truncating WAL segment", err))?;
        self.file
            .seek(SeekFrom::Start(len))
            .map_err(|err| DbError::io("seeking WAL segment", err))?;
        self.current_len = len;
        self.sync()
    }

    /// Stop appending to the current segment and delete it if it only contains the
    /// header (used after a checkpoint has captured all its records).
    pub fn delete_current_if_empty(&mut self) -> Result<bool> {
        if self.current_len <= WAL_HEADER_LEN {
            let path = self.current.clone();
            std::fs::remove_file(&path)
                .map_err(|err| DbError::io(format!("removing {}", path.display()), err))?;
            return Ok(true);
        }
        Ok(false)
    }
}

struct StagedWalSegment(PathBuf);

impl Drop for StagedWalSegment {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Result of scanning one WAL segment.
#[derive(Debug, Default)]
pub struct SegmentScan {
    pub records: Vec<Record>,
    /// Bytes of the segment that were successfully parsed (including the header).
    pub valid_len: u64,
    /// Description of a truncated torn tail, if one was found.
    pub torn_tail: Option<String>,
    /// Corruption that is *not* explainable as a torn tail.
    pub corruption: Option<String>,
}

/// Bounded-memory summary for inspection tools that do not need the records themselves.
#[derive(Debug, Default)]
pub struct SegmentSummary {
    pub record_count: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub valid_len: u64,
    pub torn_tail: Option<String>,
    pub corruption: Option<String>,
}

/// Scan a segment without retaining its records, collecting only summary metadata.
pub fn summarize_segment(path: &Path) -> Result<SegmentSummary> {
    let mut summary = SegmentSummary::default();
    let scan = scan_segment_visit(path, |record| {
        summary.record_count = summary
            .record_count
            .checked_add(1)
            .ok_or_else(|| DbError::corrupt(path.display().to_string(), "record count overflow"))?;
        summary.first_seq.get_or_insert(record.seq);
        summary.last_seq = Some(record.seq);
        Ok(())
    })?;
    summary.valid_len = scan.valid_len;
    summary.torn_tail = scan.torn_tail;
    summary.corruption = scan.corruption;
    Ok(summary)
}

/// Scan one reasonably sized segment file, validating every frame checksum and
/// retaining the records. Large or untrusted segments should use
/// `scan_segment_visit` to avoid collecting every record in memory.
pub fn scan_segment(path: &Path) -> Result<SegmentScan> {
    let len = std::fs::metadata(path)
        .map_err(|err| DbError::io(format!("stat {}", path.display()), err))?
        .len();
    if len > MAX_COLLECTED_SEGMENT_BYTES {
        return Err(DbError::LimitExceeded {
            limit: "collected_wal_segment_bytes",
            max: MAX_COLLECTED_SEGMENT_BYTES,
            actual: len,
        });
    }
    let mut records = Vec::new();
    let mut scan = scan_segment_visit(path, |record| {
        records.push(record);
        Ok(())
    })?;
    scan.records = records;
    Ok(scan)
}

/// Scan and consume records one at a time without retaining the whole segment.
pub fn scan_segment_visit<F>(path: &Path, mut visit: F) -> Result<SegmentScan>
where
    F: FnMut(Record) -> Result<()>,
{
    scan_segment_visit_with_offset(path, |record, _frame_offset| visit(record))
}

/// Scan a segment and pass each record's starting byte offset to the visitor.
///
/// Recovery uses the offset to truncate a semantically invalid suffix at the first
/// record that cannot be safely replayed, without retaining the segment in memory.
pub fn scan_segment_visit_with_offset<F>(path: &Path, mut visit: F) -> Result<SegmentScan>
where
    F: FnMut(Record, u64) -> Result<()>,
{
    let mut file =
        File::open(path).map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
    let len = file
        .metadata()
        .map_err(|err| DbError::io("stat WAL segment", err))?
        .len();
    let mut scan = SegmentScan {
        valid_len: WAL_HEADER_LEN,
        ..Default::default()
    };
    if len < WAL_HEADER_LEN {
        scan.valid_len = 0;
        scan.torn_tail = Some(format!(
            "segment header is incomplete: {len} of {WAL_HEADER_LEN} bytes"
        ));
        return Ok(scan);
    }
    let mut header = [0u8; WAL_HEADER_LEN as usize];
    file.read_exact(&mut header)
        .map_err(|err| DbError::io("reading WAL header", err))?;
    if &header[..8] != WAL_MAGIC {
        scan.corruption = Some("bad WAL magic".to_string());
        scan.valid_len = 0;
        return Ok(scan);
    }
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != 1 {
        scan.corruption = Some(format!("unsupported WAL version {version}"));
        scan.valid_len = 0;
        return Ok(scan);
    }

    let mut pos = WAL_HEADER_LEN;
    while pos < len {
        let remaining = len - pos;
        if remaining < 8 {
            scan.torn_tail = Some(format!(
                "incomplete frame header at offset {pos} ({remaining} bytes left)"
            ));
            break;
        }
        let mut frame = [0u8; 8];
        file.read_exact(&mut frame)
            .map_err(|err| DbError::io("reading WAL frame header", err))?;
        let payload_len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let expected_crc = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        if payload_len > MAX_FRAME_LEN {
            scan.corruption = Some(format!(
                "implausible frame length {payload_len} at offset {pos}"
            ));
            break;
        }
        if u64::from(payload_len) > remaining - 8 {
            scan.torn_tail = Some(format!(
                "frame at offset {pos} truncated ({} of {} payload bytes present)",
                len - pos - 8,
                payload_len
            ));
            break;
        }
        let frame_end = pos + 8 + u64::from(payload_len);
        let mut payload = vec![0u8; payload_len as usize];
        file.read_exact(&mut payload)
            .map_err(|err| DbError::io("reading WAL frame payload", err))?;
        let actual_crc = crc32c(&payload, 0);
        if actual_crc != expected_crc {
            scan.corruption = Some(format!(
                "checksum mismatch at offset {pos} (expected {expected_crc:08x}, found {actual_crc:08x})"
            ));
            break;
        }
        match Record::decode_payload(&payload) {
            Ok(record) => visit(record, pos)?,
            Err(err) => {
                scan.corruption = Some(format!("undecodable frame at offset {pos}: {err}"));
                break;
            }
        }
        pos = frame_end;
        scan.valid_len = frame_end;
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Op;
    use preppydb_json::Json;

    fn tempdir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "preppydb-wal-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn segment_open_syncs_its_directory_before_writer_publication() {
        let dir = tempdir("directory-sync");
        let mut calls = 0;
        let writer = WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Never,
            |synced_dir| {
                calls += 1;
                assert_eq!(synced_dir, dir);
                Ok(())
            },
            |from, to| std::fs::rename(from, to),
        )
        .unwrap();
        assert_eq!(calls, 1);
        drop(writer);

        let writer = WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Never,
            |synced_dir| {
                calls += 1;
                assert_eq!(synced_dir, dir);
                Ok(())
            },
            |from, to| std::fs::rename(from, to),
        )
        .unwrap();
        assert_eq!(calls, 2);
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_new_segment_directory_sync_prevents_writer_publication() {
        let dir = tempdir("directory-sync-failure");
        let result = WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Always,
            |_| {
                Err(DbError::io(
                    "syncing WAL directory",
                    std::io::Error::other("injected"),
                ))
            },
            |from, to| std::fs::rename(from, to),
        );
        assert!(result.is_err());

        // The segment header can exist after the failed barrier, but no WalWriter was
        // published and no record could be appended/acknowledged through this open.
        let path = dir.join(segment_name(1));
        assert!(path.is_file());
        let writer = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always)
            .expect("a later successful open can safely reuse the header");
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_new_segment_publication_cleans_staging_file_and_publishes_no_writer() {
        let dir = tempdir("segment-publication-failure");
        let result = WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Always,
            |_| Ok(()),
            |staged, destination| {
                assert!(staged.is_file());
                assert_eq!(destination, dir.join(segment_name(1)));
                Err(std::io::Error::other(
                    "injected segment publication failure",
                ))
            },
        );

        assert!(result.is_err());
        assert!(!dir.join(segment_name(1)).exists());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reopening_a_segment_rechecks_a_failed_directory_durability_barrier() {
        let dir = tempdir("directory-sync-reopen");
        let fail_sync = |_: &Path| {
            Err(DbError::io(
                "syncing WAL directory",
                std::io::Error::other("injected"),
            ))
        };
        assert!(WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Always,
            fail_sync,
            |from, to| std::fs::rename(from, to),
        )
        .is_err());
        assert!(dir.join(segment_name(1)).is_file());

        // The filename still exists, but the first directory sync failed. Its
        // durability cannot be inferred from the presence of the file.
        assert!(WalWriter::open_with_directory_sync(
            &dir,
            1,
            64 * 1024 * 1024,
            SyncMode::Always,
            fail_sync,
            |from, to| std::fs::rename(from, to),
        )
        .is_err());
        let writer = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn record(seq: u64) -> Record {
        Record::new(
            seq,
            0,
            Op::Put {
                collection: "c".into(),
                key: format!("k{seq}"),
                value: Json::int(seq as i64),
                expires_at: None,
            },
        )
    }

    #[test]
    fn append_and_scan() {
        let dir = tempdir("append");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        for seq in 1..=25 {
            wal.append(&record(seq)).unwrap();
        }
        wal.sync().unwrap();
        let scan = scan_segment(wal.current_path()).unwrap();
        assert!(scan.corruption.is_none());
        assert!(scan.torn_tail.is_none());
        assert_eq!(scan.records.len(), 25);
        assert_eq!(scan.records[24].seq, 25);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wal_length_and_segment_index_overflow_return_limits() {
        let dir = tempdir("overflow");
        let mut wal = WalWriter::open(&dir, 1, u64::MAX, SyncMode::Always).unwrap();
        let file_len = wal.file.metadata().unwrap().len();
        wal.current_len = u64::MAX;
        let error = wal.append(&record(1)).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert_eq!(wal.file.metadata().unwrap().len(), file_len);

        wal.current_len = file_len;
        wal.current_index = u64::MAX;
        let error = wal.rotate().unwrap_err();
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert_eq!(wal.file.metadata().unwrap().len(), file_len);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn noncanonical_wal_names_fail_closed() {
        assert_eq!(parse_segment_name("000000000001.wal"), Some(1));
        for name in [
            "+00000000001.wal",
            "000000000000.wal",
            "1000000000000.wal",
            "000000000001.WAL",
        ] {
            assert_eq!(parse_segment_name(name), None);
            let dir = tempdir("noncanonical-name");
            std::fs::write(dir.join(name), b"unexpected WAL").unwrap();
            assert_eq!(
                list_segments(&dir).unwrap_err().code(),
                crate::error::codes::CORRUPT_DATA
            );
            std::fs::remove_dir_all(dir).unwrap();
        }

        let dir = tempdir("segment-index-limit");
        let error = match WalWriter::open(
            &dir,
            MAX_SEGMENT_INDEX + 1,
            64 * 1024 * 1024,
            SyncMode::Always,
        ) {
            Ok(_) => panic!("out-of-range WAL index must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert!(!dir.join(segment_name(MAX_SEGMENT_INDEX + 1)).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn torn_tail_is_detected_and_truncatable() {
        let dir = tempdir("torn");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        wal.append(&record(1)).unwrap();
        wal.append(&record(2)).unwrap();
        let good_len = wal.current_len();
        let path = wal.current_path().to_path_buf();
        // Simulate a crash in the middle of a third frame.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[42, 0, 0, 0, 9, 9, 9]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let scan = scan_segment(&path).unwrap();
        assert_eq!(scan.records.len(), 2);
        assert!(scan.torn_tail.is_some());
        assert!(scan.corruption.is_none());
        assert_eq!(scan.valid_len, good_len);

        wal.truncate_current(scan.valid_len).unwrap();
        let rescan = scan_segment(&path).unwrap();
        assert!(rescan.torn_tail.is_none());
        assert_eq!(rescan.records.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_length_and_partial_payload_are_distinguished() {
        let dir = tempdir("frame-length");
        let wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        let path = wal.current_path().to_path_buf();
        drop(wal);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&u32::MAX.to_le_bytes()).unwrap();
        file.write_all(&0u32.to_le_bytes()).unwrap();
        drop(file);
        let scan = scan_segment(&path).unwrap();
        assert!(scan.corruption.is_some());
        assert!(scan.torn_tail.is_none());
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(WAL_HEADER_LEN).unwrap();
        drop(file);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&10u32.to_le_bytes()).unwrap();
        file.write_all(&0u32.to_le_bytes()).unwrap();
        file.write_all(&[1, 2]).unwrap();
        drop(file);
        let scan = scan_segment(&path).unwrap();
        assert!(scan.torn_tail.is_some());
        assert!(scan.corruption.is_none());
        assert_eq!(scan.valid_len, WAL_HEADER_LEN);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checksum_corruption_in_middle_is_reported() {
        let dir = tempdir("corrupt");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        for seq in 1..=3 {
            wal.append(&record(seq)).unwrap();
        }
        let path = wal.current_path().to_path_buf();
        drop(wal);
        let mut data = std::fs::read(&path).unwrap();
        // Flip a payload byte of the first frame.
        data[WAL_HEADER_LEN as usize + 8 + 3] ^= 0xff;
        std::fs::write(&path, &data).unwrap();
        let scan = scan_segment(&path).unwrap();
        assert!(scan.corruption.is_some());
        assert!(scan.records.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checksum_corruption_in_complete_final_frame_is_not_a_torn_tail() {
        let dir = tempdir("corrupt-final");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        wal.append(&record(1)).unwrap();
        let path = wal.current_path().to_path_buf();
        drop(wal);
        let mut data = std::fs::read(&path).unwrap();
        data[WAL_HEADER_LEN as usize + 8 + 3] ^= 0xff;
        std::fs::write(&path, &data).unwrap();
        let scan = scan_segment(&path).unwrap();
        assert!(scan.corruption.is_some());
        assert!(scan.torn_tail.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_creates_new_segments() {
        let dir = tempdir("rotate");
        let mut wal = WalWriter::open(&dir, 1, 400, SyncMode::Always).unwrap();
        for seq in 1..=20 {
            wal.append(&record(seq)).unwrap();
        }
        let segments = list_segments(&dir).unwrap();
        assert!(segments.len() > 1, "expected rotation, got {segments:?}");
        let mut total = 0;
        for segment in &segments {
            let scan = scan_segment(segment).unwrap();
            assert!(scan.corruption.is_none());
            total += scan.records.len();
        }
        assert_eq!(total, 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn visitor_processes_records_without_retaining_them() {
        let dir = tempdir("visitor");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        for seq in 1..=1000 {
            wal.append(&record(seq)).unwrap();
        }
        let mut count = 0;
        let scan = scan_segment_visit(wal.current_path(), |record| {
            count += 1;
            assert_eq!(record.seq, count);
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1000);
        assert!(scan.records.is_empty());
        assert!(scan.corruption.is_none());
        drop(wal);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn summary_matches_scan_without_retaining_records() {
        let dir = tempdir("summary");
        let mut wal = WalWriter::open(&dir, 1, 64 * 1024 * 1024, SyncMode::Always).unwrap();
        for seq in 10..=30 {
            wal.append(&record(seq)).unwrap();
        }
        wal.sync().unwrap();
        let expected = scan_segment(wal.current_path()).unwrap();
        let summary = summarize_segment(wal.current_path()).unwrap();
        assert_eq!(summary.record_count, expected.records.len() as u64);
        assert_eq!(summary.first_seq, Some(10));
        assert_eq!(summary.last_seq, Some(30));
        assert_eq!(summary.valid_len, expected.valid_len);
        assert_eq!(summary.torn_tail, None);
        assert_eq!(summary.corruption, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collecting_scan_rejects_oversized_segment_before_reading_records() {
        let dir = tempdir("collect-limit");
        let path = dir.join(segment_name(1));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_COLLECTED_SEGMENT_BYTES + 1).unwrap();
        drop(file);
        assert!(matches!(
            scan_segment(&path),
            Err(DbError::LimitExceeded {
                limit: "collected_wal_segment_bytes",
                max: MAX_COLLECTED_SEGMENT_BYTES,
                actual,
            }) if actual == MAX_COLLECTED_SEGMENT_BYTES + 1
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segment_names_parse() {
        assert_eq!(parse_segment_name("000000000001.wal"), Some(1));
        assert_eq!(parse_segment_name("1.wal"), None);
        assert_eq!(parse_segment_name("garbage.wal"), None);
        assert_eq!(parse_segment_name("000000000002.wal.bak"), None);
    }

    #[cfg(unix)]
    #[test]
    fn wal_writer_rejects_symlinked_segment_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir("symlink-segment");
        let wal_dir = dir.join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, wal_dir.join(segment_name(1))).unwrap();

        let error = match WalWriter::open(&wal_dir, 1, 1024, SyncMode::Always) {
            Ok(_) => panic!("WAL writer accepted a symlinked segment"),
            Err(error) => error,
        };
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn segment_listing_rejects_symlinked_segments() {
        use std::os::unix::fs::symlink;

        let dir = tempdir("list-symlink-segment");
        let target = dir.join("target");
        std::fs::write(&target, b"not a wal").unwrap();
        symlink(&target, dir.join(segment_name(1))).unwrap();

        let error = list_segments(&dir).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_mode_parsing() {
        assert_eq!(SyncMode::parse("always").unwrap(), SyncMode::Always);
        assert_eq!(
            SyncMode::parse("interval:50").unwrap(),
            SyncMode::Interval(50)
        );
        assert!(SyncMode::parse("sometimes").is_err());
    }
}
