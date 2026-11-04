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

/// Durability policy for commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// `fsync` on every commit: no committed write can be lost, slowest.
    Always,
    /// `fsync` at most every N milliseconds: a crash can lose the last N ms.
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

/// Thousands-separated segment file name, e.g. `wal/000000000042.wal`.
pub fn segment_name(index: u64) -> String {
    format!("{index:012}.wal")
}

pub fn parse_segment_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".wal")?;
    if stem.len() != 12 {
        return None;
    }
    stem.parse::<u64>().ok()
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
        if parse_segment_name(&name).is_some() {
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
}

impl WalWriter {
    /// Open (or create) the segment with `index` and append to it.
    pub fn open(
        dir: &Path,
        index: u64,
        max_segment_bytes: u64,
        sync_mode: SyncMode,
    ) -> Result<WalWriter> {
        std::fs::create_dir_all(dir)
            .map_err(|err| DbError::io(format!("creating {}", dir.display()), err))?;
        let path = dir.join(segment_name(index));
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
        let len = file
            .metadata()
            .map_err(|err| DbError::io("stat WAL segment", err))?
            .len();
        if !existed || len == 0 {
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

    /// Append one record frame, rotating to a new segment when the cap is reached.
    pub fn append(&mut self, record: &Record) -> Result<()> {
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
        if self.current_len > WAL_HEADER_LEN
            && self.current_len + frame_len > self.max_segment_bytes
        {
            self.rotate()?;
        }
        let mut frame = Vec::with_capacity(frame_len as usize);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32c(&payload, 0).to_le_bytes());
        frame.extend_from_slice(&payload);
        self.file
            .write_all(&frame)
            .map_err(|err| DbError::io(format!("appending to {}", self.current.display()), err))?;
        self.current_len += frame_len;
        self.unsynced_frames += 1;
        if self.sync_mode.is_always() {
            self.sync()?;
        }
        Ok(())
    }

    /// `fsync` the current segment.
    pub fn sync(&mut self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|err| DbError::io(format!("syncing {}", self.current.display()), err))?;
        self.unsynced_frames = 0;
        Ok(())
    }

    /// Start a new segment (used by rotation and by checkpoints).
    pub fn rotate(&mut self) -> Result<()> {
        self.sync()?;
        let next = self.current_index + 1;
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

/// Scan one segment file, validating every frame checksum.
pub fn scan_segment(path: &Path) -> Result<SegmentScan> {
    let mut file =
        File::open(path).map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
    let len = file
        .metadata()
        .map_err(|err| DbError::io("stat WAL segment", err))?
        .len();
    let mut data = Vec::with_capacity(len.min(128 * 1024 * 1024) as usize);
    file.read_to_end(&mut data)
        .map_err(|err| DbError::io(format!("reading {}", path.display()), err))?;

    let mut scan = SegmentScan {
        valid_len: WAL_HEADER_LEN,
        ..Default::default()
    };
    if data.len() < WAL_HEADER_LEN as usize {
        scan.corruption = Some(format!(
            "segment is {} bytes, shorter than the {} byte header",
            data.len(),
            WAL_HEADER_LEN
        ));
        scan.valid_len = 0;
        return Ok(scan);
    }
    if &data[..8] != WAL_MAGIC {
        scan.corruption = Some("bad WAL magic".to_string());
        scan.valid_len = 0;
        return Ok(scan);
    }
    let version = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    if version != 1 {
        scan.corruption = Some(format!("unsupported WAL version {version}"));
        scan.valid_len = 0;
        return Ok(scan);
    }

    let mut pos = WAL_HEADER_LEN as usize;
    while pos < data.len() {
        let remaining = data.len() - pos;
        if remaining < 8 {
            scan.torn_tail = Some(format!(
                "incomplete frame header at offset {pos} ({remaining} bytes left)"
            ));
            break;
        }
        let payload_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let expected_crc =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        if payload_len > MAX_FRAME_LEN {
            scan.corruption = Some(format!(
                "implausible frame length {payload_len} at offset {pos}"
            ));
            break;
        }
        let frame_end = pos + 8 + payload_len as usize;
        if frame_end > data.len() {
            scan.torn_tail = Some(format!(
                "frame at offset {pos} truncated ({} of {} payload bytes present)",
                data.len() - pos - 8,
                payload_len
            ));
            break;
        }
        let payload = &data[pos + 8..frame_end];
        let actual_crc = crc32c(payload, 0);
        if actual_crc != expected_crc {
            if frame_end == data.len() {
                // Last frame with a bad checksum: the write was interrupted, but the
                // length field was already flushed. Treat as torn tail.
                scan.torn_tail = Some(format!(
                    "checksum mismatch in final frame at offset {pos} (expected {expected_crc:08x}, found {actual_crc:08x})"
                ));
            } else {
                scan.corruption = Some(format!(
                    "checksum mismatch at offset {pos} (expected {expected_crc:08x}, found {actual_crc:08x})"
                ));
            }
            break;
        }
        match Record::decode_payload(payload) {
            Ok(record) => scan.records.push(record),
            Err(err) => {
                scan.corruption = Some(format!("undecodable frame at offset {pos}: {err}"));
                break;
            }
        }
        pos = frame_end;
        scan.valid_len = frame_end as u64;
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
    fn segment_names_parse() {
        assert_eq!(parse_segment_name("000000000001.wal"), Some(1));
        assert_eq!(parse_segment_name("1.wal"), None);
        assert_eq!(parse_segment_name("garbage.wal"), None);
        assert_eq!(parse_segment_name("000000000002.wal.bak"), None);
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
