//! Durable manifest describing the current snapshot and log files.
//!
//! The manifest is written atomically (`MANIFEST.json.tmp` + `fsync` + `rename` +
//! `fsync(dir)`) and is treated as *advisory* by recovery: the authoritative source of
//! truth is the snapshot plus every WAL segment still present on disk, filtered by the
//! snapshot's sequence number. That way a crash between "snapshot written" and
//! "manifest updated" can never lose committed data.

use crate::error::{DbError, Result};
use crate::snapshot::fsync_dir;
use preppydb_json::Json;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "MANIFEST.json";
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Reference to the snapshot file the manifest points at.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRef {
    pub file: String,
    pub seq: u64,
    pub generation: u64,
    pub sha256: String,
    pub doc_count: u64,
    pub created_at: i64,
    pub bytes: u64,
}

impl SnapshotRef {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("file", Json::str(self.file.clone()))
            .with("seq", Json::uint(self.seq))
            .with("generation", Json::uint(self.generation))
            .with("sha256", Json::str(self.sha256.clone()))
            .with("doc_count", Json::uint(self.doc_count))
            .with("created_at", Json::int(self.created_at))
            .with("bytes", Json::uint(self.bytes))
    }

    pub fn from_json(value: &Json) -> Result<SnapshotRef> {
        Ok(SnapshotRef {
            file: value
                .get_str("file")
                .ok_or_else(|| DbError::corrupt("manifest", "snapshot entry without 'file'"))?
                .to_string(),
            seq: value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            generation: value
                .get("generation")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            sha256: value.get_str("sha256").unwrap_or("").to_string(),
            doc_count: value.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0),
            created_at: value.get_i64("created_at").unwrap_or(0),
            bytes: value.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        })
    }
}

/// The manifest content.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub format_version: u32,
    pub generation: u64,
    pub last_seq: u64,
    pub snapshot: Option<SnapshotRef>,
    /// WAL segments that are part of the log after the snapshot (informational).
    pub wal: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            generation: 0,
            last_seq: 0,
            snapshot: None,
            wal: Vec::new(),
            created_at: crate::clock::now_ms(),
            updated_at: crate::clock::now_ms(),
        }
    }
}

impl Manifest {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("format_version", Json::uint(self.format_version as u64))
            .with("generation", Json::uint(self.generation))
            .with("last_seq", Json::uint(self.last_seq))
            .with(
                "snapshot",
                self.snapshot
                    .as_ref()
                    .map(|s| s.to_json())
                    .unwrap_or(Json::Null),
            )
            .with(
                "wal",
                Json::Array(
                    self.wal
                        .iter()
                        .map(|name| Json::str(name.clone()))
                        .collect(),
                ),
            )
            .with("created_at", Json::int(self.created_at))
            .with("updated_at", Json::int(self.updated_at))
            .with("engine_version", Json::str(env!("CARGO_PKG_VERSION")))
    }

    pub fn from_json(value: &Json) -> Result<Manifest> {
        let format_version = value
            .get("format_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if format_version != MANIFEST_FORMAT_VERSION {
            return Err(DbError::corrupt(
                "manifest",
                format!(
                    "unsupported manifest format version {format_version} (this build understands {MANIFEST_FORMAT_VERSION})"
                ),
            ));
        }
        let snapshot = match value.get("snapshot") {
            None | Some(Json::Null) => None,
            Some(entry) => Some(SnapshotRef::from_json(entry)?),
        };
        let wal = value
            .get("wal")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Manifest {
            format_version,
            generation: value
                .get("generation")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            last_seq: value.get("last_seq").and_then(|v| v.as_u64()).unwrap_or(0),
            snapshot,
            wal,
            created_at: value.get_i64("created_at").unwrap_or(0),
            updated_at: value.get_i64("updated_at").unwrap_or(0),
        })
    }

    pub fn path(dir: &Path) -> PathBuf {
        dir.join(MANIFEST_FILE)
    }

    /// Load, returning `None` when the file does not exist yet.
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = Manifest::path(dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(DbError::io(format!("reading {}", path.display()), err)),
        };
        let value = preppydb_json::parse(&text).map_err(|err| {
            DbError::corrupt(path.display().to_string(), format!("invalid JSON: {err}"))
        })?;
        Ok(Some(Manifest::from_json(&value)?))
    }

    /// Atomically persist the manifest.
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        self.updated_at = crate::clock::now_ms();
        let path = Manifest::path(dir);
        let tmp = dir.join("MANIFEST.json.tmp");
        let text = preppydb_json::to_string_pretty(&self.to_json());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options
            .open(&tmp)
            .map_err(|err| DbError::io(format!("creating {}", tmp.display()), err))?;
        file.write_all(text.as_bytes())
            .map_err(|err| DbError::io("writing manifest", err))?;
        file.write_all(b"\n")
            .map_err(|err| DbError::io("writing manifest", err))?;
        file.sync_all()
            .map_err(|err| DbError::io("syncing manifest", err))?;
        drop(file);
        crate::fsutil::replace_file(&tmp, &path)
            .map_err(|err| DbError::io("renaming manifest into place", err))?;
        fsync_dir(dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "preppydb-manifest-{tag}-{}-{}",
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
    fn save_and_load_roundtrip() {
        let dir = tempdir("roundtrip");
        let mut manifest = Manifest {
            last_seq: 99,
            generation: 3,
            ..Default::default()
        };
        manifest.snapshot = Some(SnapshotRef {
            file: "snap-000000000003.pdb".into(),
            seq: 99,
            generation: 3,
            sha256: "abc".into(),
            doc_count: 4,
            created_at: 1,
            bytes: 100,
        });
        manifest.wal.push("000000000004.wal".into());
        manifest.save(&dir).unwrap();
        let loaded = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(loaded.last_seq, 99);
        assert_eq!(loaded.snapshot.unwrap().file, "snap-000000000003.pdb");
        assert_eq!(loaded.wal, vec!["000000000004.wal".to_string()]);
        // No temporary file is left behind.
        assert!(!dir.join("MANIFEST.json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_manifest_is_none() {
        let dir = tempdir("missing");
        assert!(Manifest::load(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_format_is_rejected() {
        let dir = tempdir("format");
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"format_version": 99, "generation": 0, "last_seq": 0}"#,
        )
        .unwrap();
        assert!(Manifest::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_json_is_corruption() {
        let dir = tempdir("json");
        std::fs::write(dir.join(MANIFEST_FILE), "{not json").unwrap();
        let err = Manifest::load(&dir).unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
