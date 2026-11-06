//! Checkpoint (snapshot) files.
//!
//! A snapshot captures the full logical state as of a log sequence number so that the
//! WAL prefix before that sequence can be discarded.
//!
//! ```text
//! magic "PPDBSNAP1" | u32 header_len | header JSON
//! frames: [u32 len][u32 crc32c(payload)][payload JSON]
//! footer: [u32 0xFFFFFFFF][sha256 of all frame bytes]
//! ```
//!
//! Loading verifies every frame CRC *and* the whole-file SHA-256 before the state is
//! used, so a corrupted checkpoint can never be silently half-applied.

use crate::catalog::CollectionMeta;
use crate::error::{DbError, Result};
use crate::store::Doc;
use preppydb_crypto::crc32c;
use preppydb_crypto::sha256::{sha256, Sha256};
use preppydb_json::Json;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_MAGIC: &[u8; 9] = b"PPDBSNAP1";
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
const FOOTER_MARKER: u32 = 0xFFFF_FFFF;
const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

fn checked_frame_len(payload_len: usize) -> Result<u32> {
    let actual = u64::try_from(payload_len).unwrap_or(u64::MAX);
    if actual > u64::from(MAX_FRAME_LEN) {
        return Err(DbError::LimitExceeded {
            limit: "snapshot_frame_bytes",
            max: u64::from(MAX_FRAME_LEN),
            actual,
        });
    }
    u32::try_from(payload_len).map_err(|_| DbError::LimitExceeded {
        limit: "snapshot_frame_bytes",
        max: u64::from(MAX_FRAME_LEN),
        actual,
    })
}

/// Header of a snapshot file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotHeader {
    pub format_version: u32,
    pub generation: u64,
    pub seq: u64,
    pub created_at: i64,
    pub doc_count: u64,
    pub collections: u64,
}

impl SnapshotHeader {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("format_version", Json::uint(self.format_version as u64))
            .with("generation", Json::uint(self.generation))
            .with("seq", Json::uint(self.seq))
            .with("created_at", Json::int(self.created_at))
            .with("doc_count", Json::uint(self.doc_count))
            .with("collections", Json::uint(self.collections))
    }

    pub fn from_json(value: &Json) -> Result<SnapshotHeader> {
        let format_version = value
            .get("format_version")
            .and_then(|value| value.as_u64())
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| DbError::corrupt("snapshot header", "invalid format version"))?;
        Ok(SnapshotHeader {
            format_version,
            generation: value
                .get("generation")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            seq: value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            created_at: value.get_i64("created_at").unwrap_or(0),
            doc_count: value.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0),
            collections: value
                .get("collections")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        })
    }
}

/// Result of writing a snapshot.
#[derive(Clone, Debug)]
pub struct WrittenSnapshot {
    pub path: PathBuf,
    pub header: SnapshotHeader,
    pub body_sha256: String,
    pub bytes: u64,
}

/// The state carried by a snapshot file.
#[derive(Clone, Debug, Default)]
pub struct SnapshotData {
    pub header: SnapshotHeader,
    pub collections: Vec<(CollectionMeta, Vec<(String, Doc)>)>,
    pub body_sha256: String,
}

/// Write `collections` to `path` atomically (write to `.tmp`, fsync, rename).
pub fn write_snapshot(
    path: &Path,
    generation: u64,
    seq: u64,
    created_at: i64,
    collections: &[(CollectionMeta, Vec<(String, Doc)>)],
) -> Result<WrittenSnapshot> {
    let tmp_path = path.with_extension("pdb.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| DbError::io(format!("creating {}", parent.display()), err))?;
    }
    let header = SnapshotHeader {
        format_version: SNAPSHOT_FORMAT_VERSION,
        generation,
        seq,
        created_at,
        doc_count: collections.iter().map(|(_, docs)| docs.len() as u64).sum(),
        collections: collections.len() as u64,
    };
    let header_json = header.to_json().to_string();

    let file = File::create(&tmp_path)
        .map_err(|err| DbError::io(format!("creating {}", tmp_path.display()), err))?;
    let mut writer = BufWriter::new(file);
    let mut body_hasher = Sha256::new();
    let mut bytes: u64 = 0;

    writer
        .write_all(SNAPSHOT_MAGIC)
        .map_err(|err| DbError::io("writing snapshot magic", err))?;
    bytes += SNAPSHOT_MAGIC.len() as u64;
    let header_bytes = header_json.as_bytes();
    writer
        .write_all(&(header_bytes.len() as u32).to_le_bytes())
        .map_err(|err| DbError::io("writing snapshot header length", err))?;
    writer
        .write_all(header_bytes)
        .map_err(|err| DbError::io("writing snapshot header", err))?;
    bytes += 4 + header_bytes.len() as u64;

    let write_frame = |writer: &mut BufWriter<File>,
                       hasher: &mut Sha256,
                       payload: &[u8],
                       bytes: &mut u64|
     -> Result<()> {
        let payload_len = checked_frame_len(payload.len())?;
        let mut frame = Vec::with_capacity(payload.len() + 8);
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(&crc32c(payload, 0).to_le_bytes());
        frame.extend_from_slice(payload);
        writer
            .write_all(&frame)
            .map_err(|err| DbError::io("writing snapshot frame", err))?;
        hasher.update(&frame);
        *bytes += frame.len() as u64;
        Ok(())
    };

    for (meta, docs) in collections {
        let payload = Json::obj()
            .with("t", Json::str("coll"))
            .with("meta", meta.to_json())
            .to_string();
        write_frame(
            &mut writer,
            &mut body_hasher,
            payload.as_bytes(),
            &mut bytes,
        )?;
        for (key, doc) in docs {
            let payload = Json::obj()
                .with("t", Json::str("doc"))
                .with("c", Json::str(meta.name.clone()))
                .with("k", Json::str(key.clone()))
                .with("d", doc.to_json())
                .to_string();
            write_frame(
                &mut writer,
                &mut body_hasher,
                payload.as_bytes(),
                &mut bytes,
            )?;
        }
    }

    let body_digest = body_hasher.finish();
    writer
        .write_all(&FOOTER_MARKER.to_le_bytes())
        .map_err(|err| DbError::io("writing snapshot footer", err))?;
    writer
        .write_all(&body_digest)
        .map_err(|err| DbError::io("writing snapshot digest", err))?;
    bytes += 4 + 32;
    writer
        .flush()
        .map_err(|err| DbError::io("flushing snapshot", err))?;
    let file = writer
        .into_inner()
        .map_err(|err| DbError::io("flushing snapshot", err.into_error()))?;
    file.sync_all()
        .map_err(|err| DbError::io("syncing snapshot", err))?;
    drop(file);
    crate::fsutil::replace_file(&tmp_path, path).map_err(|err| {
        DbError::io(
            format!("renaming {} to {}", tmp_path.display(), path.display()),
            err,
        )
    })?;
    fsync_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;

    Ok(WrittenSnapshot {
        path: path.to_path_buf(),
        header,
        body_sha256: preppydb_crypto::hex_encode(&body_digest),
        bytes,
    })
}

/// Read and fully verify a snapshot file.
pub fn read_snapshot(path: &Path) -> Result<SnapshotData> {
    let file =
        File::open(path).map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 9];
    read_snapshot_exact(&mut reader, &mut magic, path, "magic")?;
    if &magic != SNAPSHOT_MAGIC {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "not a PreppyDB snapshot (bad magic)",
        ));
    }
    let mut len_buf = [0u8; 4];
    read_snapshot_exact(&mut reader, &mut len_buf, path, "header length")?;
    let header_len = u32::from_le_bytes(len_buf);
    if header_len > 1 << 20 {
        return Err(DbError::corrupt(
            path.display().to_string(),
            format!("implausible header length {header_len}"),
        ));
    }
    let mut header_bytes = vec![0u8; header_len as usize];
    read_snapshot_exact(&mut reader, &mut header_bytes, path, "header")?;
    let header_text = std::str::from_utf8(&header_bytes)
        .map_err(|err| DbError::corrupt("snapshot header", format!("not UTF-8: {err}")))?;
    let header_json = preppydb_json::parse(header_text)
        .map_err(|err| DbError::corrupt("snapshot header", err.to_string()))?;
    let header = SnapshotHeader::from_json(&header_json)?;
    if header.format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(DbError::corrupt(
            path.display().to_string(),
            format!(
                "unsupported snapshot format version {} (this build understands {})",
                header.format_version, SNAPSHOT_FORMAT_VERSION
            ),
        ));
    }

    let mut hasher = Sha256::new();
    let mut collections: Vec<(CollectionMeta, Vec<(String, Doc)>)> = Vec::new();
    let mut collection_names = HashSet::new();
    let mut document_keys: Vec<HashSet<String>> = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        read_snapshot_exact(&mut reader, &mut len_buf, path, "frame length or footer")?;
        let len = u32::from_le_bytes(len_buf);
        if len == FOOTER_MARKER {
            let mut digest = [0u8; 32];
            read_snapshot_exact(&mut reader, &mut digest, path, "footer digest")?;
            let actual = hasher.finish();
            if actual != digest {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "snapshot body SHA-256 does not match the footer",
                ));
            }
            let mut trailing = [0u8; 1];
            if reader
                .read(&mut trailing)
                .map_err(|err| DbError::io("checking snapshot end of file", err))?
                != 0
            {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "unexpected bytes after snapshot footer",
                ));
            }
            if collections.iter().map(|(_, d)| d.len() as u64).sum::<u64>() != header.doc_count {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    format!(
                        "header announces {} documents but the body contains {}",
                        header.doc_count,
                        collections.iter().map(|(_, d)| d.len()).sum::<usize>()
                    ),
                ));
            }
            return Ok(SnapshotData {
                header,
                collections,
                body_sha256: preppydb_crypto::hex_encode(&digest),
            });
        }
        if len > MAX_FRAME_LEN {
            return Err(DbError::corrupt(
                path.display().to_string(),
                format!("implausible frame length {len}"),
            ));
        }
        hasher.update(&len_buf);
        let mut crc_buf = [0u8; 4];
        read_snapshot_exact(&mut reader, &mut crc_buf, path, "frame checksum")?;
        let crc = u32::from_le_bytes(crc_buf);
        hasher.update(&crc_buf);
        let mut payload = vec![0u8; len as usize];
        read_snapshot_exact(&mut reader, &mut payload, path, "frame payload")?;
        hasher.update(&payload);
        let actual_crc = crc32c(&payload, 0);
        if actual_crc != crc {
            return Err(DbError::corrupt(
                path.display().to_string(),
                format!("frame checksum mismatch (expected {crc:08x}, found {actual_crc:08x})"),
            ));
        }
        let value = preppydb_json::parse(
            std::str::from_utf8(&payload)
                .map_err(|err| DbError::corrupt("snapshot frame", format!("not UTF-8: {err}")))?,
        )
        .map_err(|err| DbError::corrupt("snapshot frame", err.to_string()))?;
        match value.get_str("t") {
            Some("coll") => {
                let meta = CollectionMeta::from_json(value.get("meta").ok_or_else(|| {
                    DbError::corrupt("snapshot", "collection frame without meta")
                })?)?;
                if !collection_names.insert(meta.name.clone()) {
                    return Err(DbError::corrupt(
                        path.display().to_string(),
                        format!("duplicate collection '{}' in snapshot", meta.name),
                    ));
                }
                collections.push((meta, Vec::new()));
                document_keys.push(HashSet::new());
            }
            Some("doc") => {
                let collection = value
                    .get_str("c")
                    .ok_or_else(|| DbError::corrupt("snapshot", "doc frame without collection"))?;
                let key = value
                    .get_str("k")
                    .ok_or_else(|| DbError::corrupt("snapshot", "doc frame without key"))?;
                let doc_json = value
                    .get("d")
                    .ok_or_else(|| DbError::corrupt("snapshot", "doc frame without document"))?;
                let doc = Doc::from_json(doc_json)?;
                let collection_index = collections
                    .iter()
                    .position(|(meta, _)| meta.name == collection)
                    .ok_or_else(|| {
                        DbError::corrupt(
                            path.display().to_string(),
                            format!("document for unknown collection '{collection}'"),
                        )
                    })?;
                if !document_keys[collection_index].insert(key.to_string()) {
                    return Err(DbError::corrupt(
                        path.display().to_string(),
                        format!("duplicate document key in collection '{collection}'"),
                    ));
                }
                collections[collection_index].1.push((key.to_string(), doc));
            }
            other => {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    format!("unknown frame type {other:?}"),
                ))
            }
        }
    }
}

fn read_snapshot_exact(
    reader: &mut impl Read,
    buffer: &mut [u8],
    path: &Path,
    part: &str,
) -> Result<()> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            DbError::corrupt(
                path.display().to_string(),
                format!("truncated snapshot {part}"),
            )
        } else {
            DbError::io(format!("reading snapshot {part}"), error)
        }
    })
}

/// `fsync` a directory so that a rename is durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let file = File::open(dir)
            .map_err(|err| DbError::io(format!("opening directory {}", dir.display()), err))?;
        file.sync_all()
            .map_err(|err| DbError::io(format!("syncing directory {}", dir.display()), err))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Compute the SHA-256 of a file (used by backups and `fsck`).
pub fn file_sha256(path: &Path) -> Result<(u64, String)> {
    let mut file =
        File::open(path).map_err(|err| DbError::io(format!("opening {}", path.display()), err))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| DbError::io("reading file", err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, preppydb_crypto::hex_encode(&hasher.finish())))
}

/// Convenience: hash of a byte slice, hex encoded.
pub fn bytes_sha256(bytes: &[u8]) -> String {
    preppydb_crypto::hex_encode(&sha256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use preppydb_json::Json;

    fn tempdir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "preppydb-snap-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn sample() -> Vec<(CollectionMeta, Vec<(String, Doc)>)> {
        let mut meta = CollectionMeta::new("profiles", 10);
        meta.max_docs = Some(10);
        let doc = Doc {
            value: Json::obj().with("name", Json::str("Molly")),
            version: 3,
            revision: 9,
            created_at: 10,
            updated_at: 20,
            expires_at: None,
        };
        vec![(meta, vec![("molly".to_string(), doc)])]
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempdir("roundtrip");
        let path = dir.join("snap-1.pdb");
        let written = write_snapshot(&path, 1, 42, 1234, &sample()).unwrap();
        assert_eq!(written.header.doc_count, 1);
        assert_eq!(written.header.seq, 42);
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.header, written.header);
        assert_eq!(loaded.body_sha256, written.body_sha256);
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].0.name, "profiles");
        assert_eq!(loaded.collections[0].1[0].0, "molly");
        assert_eq!(loaded.collections[0].1[0].1.version, 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicate_document_keys_in_snapshot_are_rejected() {
        let dir = tempdir("duplicate-document-key");
        let path = dir.join("snap-1.pdb");
        let mut collections = sample();
        let duplicate = collections[0].1[0].clone();
        collections[0].1.push(duplicate);
        write_snapshot(&path, 1, 1, 0, &collections).unwrap();

        let error = read_snapshot(&path).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        assert!(error.to_string().contains("duplicate document key"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicate_collection_names_in_snapshot_are_rejected() {
        let dir = tempdir("duplicate-collection-name");
        let path = dir.join("snap-1.pdb");
        let mut collections = sample();
        collections.push(collections[0].clone());
        write_snapshot(&path, 1, 1, 0, &collections).unwrap();

        let error = read_snapshot(&path).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        assert!(error.to_string().contains("duplicate collection"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_frame_writer_enforces_reader_limit_without_narrowing() {
        assert_eq!(
            checked_frame_len(MAX_FRAME_LEN as usize).unwrap(),
            MAX_FRAME_LEN
        );
        let error = checked_frame_len(MAX_FRAME_LEN as usize + 1).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert!(error.to_string().contains("snapshot_frame_bytes"));
    }

    #[test]
    fn corrupted_body_is_detected() {
        let dir = tempdir("corrupt");
        let path = dir.join("snap-1.pdb");
        write_snapshot(&path, 1, 1, 0, &sample()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let idx = bytes.len() - 40; // somewhere inside the last frame payload
        bytes[idx] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let err = read_snapshot(&path).unwrap_err();
        assert!(
            err.to_string().contains("checksum") || err.to_string().contains("SHA-256"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_snapshot_components_are_reported_as_corruption() {
        let dir = tempdir("truncated");
        let path = dir.join("snap-1.pdb");
        write_snapshot(&path, 1, 1, 0, &sample()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
        let first_frame = 13 + header_len;
        let payload_start = first_frame + 8;
        let payload_len =
            u32::from_le_bytes(bytes[first_frame..first_frame + 4].try_into().unwrap()) as usize;
        let truncated = [
            &bytes[..11],
            &bytes[..13 + header_len - 1],
            &bytes[..first_frame + 2],
            &bytes[..first_frame + 6],
            &bytes[..payload_start + payload_len - 1],
            &bytes[..bytes.len() - 1],
        ];

        for (index, contents) in truncated.iter().enumerate() {
            std::fs::write(&path, contents).unwrap();
            let error = read_snapshot(&path).unwrap_err();
            assert_eq!(
                error.code(),
                crate::error::codes::CORRUPT_DATA,
                "truncation case {index} was misclassified: {error}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trailing_bytes_after_snapshot_footer_are_rejected() {
        let dir = tempdir("trailing-data");
        let path = dir.join("snap-1.pdb");
        write_snapshot(&path, 1, 1, 0, &sample()).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"unexpected")
            .unwrap();
        let error = read_snapshot(&path).unwrap_err();
        assert!(error.to_string().contains("after snapshot footer"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_magic_detected() {
        let dir = tempdir("magic");
        let path = dir.join("snap-1.pdb");
        std::fs::write(&path, b"NOTASNAP!!!!!!").unwrap();
        assert!(read_snapshot(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_format_version_does_not_wrap_on_integer_conversion() {
        let dir = tempdir("version-overflow");
        let path = dir.join("snap-1.pdb");
        write_snapshot(&path, 1, 1, 0, &[]).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
        let header_end = 13 + header_len;
        let mut header =
            preppydb_json::parse(std::str::from_utf8(&bytes[13..header_end]).unwrap()).unwrap();
        header.set("format_version", Json::uint(u32::MAX as u64 + 2));
        let header_bytes = header.to_string().into_bytes();

        // The body hash covers frames, not the header. Rebuild the framing so the
        // snapshot is otherwise internally valid and isolates the version conversion.
        let mut forged = Vec::with_capacity(bytes.len() + header_bytes.len() - header_len);
        forged.extend_from_slice(SNAPSHOT_MAGIC);
        forged.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        forged.extend_from_slice(&header_bytes);
        forged.extend_from_slice(&bytes[header_end..]);
        std::fs::write(&path, forged).unwrap();

        let error = read_snapshot(&path).unwrap_err();
        assert!(
            error.to_string().contains("version") || error.to_string().contains("format"),
            "unexpected error: {error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn doc_count_mismatch_detected() {
        let dir = tempdir("count");
        let path = dir.join("snap-1.pdb");
        write_snapshot(&path, 1, 1, 0, &sample()).unwrap();
        // Rewrite the header's doc_count (header sits right after magic+len) on the raw
        // bytes: the file is not valid UTF-8 as a whole because of the binary framing.
        let mut bytes = std::fs::read(&path).unwrap();
        let needle = b"\"doc_count\":1";
        let position = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("doc_count in header");
        bytes[position + needle.len() - 1] = b'2';
        std::fs::write(&path, &bytes).unwrap();
        let err = read_snapshot(&path).unwrap_err();
        assert!(err.to_string().contains("announces"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_hash_helper() {
        let dir = tempdir("hash");
        let path = dir.join("f.bin");
        std::fs::write(&path, b"abc").unwrap();
        let (size, hash) = file_sha256(&path).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
