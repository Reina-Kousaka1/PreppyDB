//! Durable manifest describing the current snapshot and log files.
//!
//! Checkpoints publish immutable generation-named manifests. Publishing a new name
//! does not replace an existing file, which avoids platform-specific replace-existing
//! behavior. The legacy `MANIFEST.json` remains readable for compatibility. The
//! authoritative state is the newest published manifest plus its snapshot and WAL.

use crate::error::{DbError, Result};
use crate::snapshot::fsync_dir;
use preppydb_json::Json;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "MANIFEST.json";
const VERSIONED_MANIFEST_PREFIX: &str = "MANIFEST-";
const VERSIONED_MANIFEST_SUFFIX: &str = ".json";
const MAX_MANIFEST_GENERATION: u64 = 999_999_999_999;
pub const MANIFEST_FORMAT_VERSION: u32 = 1;
/// Bound the amount of data read from an untrusted manifest before JSON parsing.
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

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
        let file = value
            .get_str("file")
            .ok_or_else(|| DbError::corrupt("manifest", "snapshot entry without 'file'"))?;
        let seq = value
            .get("seq")
            .and_then(Json::as_u64)
            .ok_or_else(|| DbError::corrupt("manifest", "snapshot entry without valid 'seq'"))?;
        let generation = value
            .get("generation")
            .and_then(Json::as_u64)
            .ok_or_else(|| {
                DbError::corrupt("manifest", "snapshot entry without valid 'generation'")
            })?;
        let sha256 = value
            .get_str("sha256")
            .ok_or_else(|| DbError::corrupt("manifest", "snapshot entry without 'sha256'"))?;
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(DbError::corrupt(
                "manifest",
                "snapshot entry has an invalid SHA-256",
            ));
        }
        let doc_count = value
            .get("doc_count")
            .and_then(Json::as_u64)
            .ok_or_else(|| {
                DbError::corrupt("manifest", "snapshot entry without valid 'doc_count'")
            })?;
        let created_at = value.get_i64("created_at").ok_or_else(|| {
            DbError::corrupt("manifest", "snapshot entry without valid 'created_at'")
        })?;
        let bytes = value
            .get("bytes")
            .and_then(Json::as_u64)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| DbError::corrupt("manifest", "snapshot entry without valid 'bytes'"))?;
        Ok(SnapshotRef {
            file: file.to_string(),
            seq,
            generation,
            sha256: sha256.to_string(),
            doc_count,
            created_at,
            bytes,
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
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| DbError::corrupt("manifest", "invalid manifest format version"))?;
        if format_version != MANIFEST_FORMAT_VERSION {
            return Err(DbError::corrupt(
                "manifest",
                format!(
                    "unsupported manifest format version {format_version} (this build understands {MANIFEST_FORMAT_VERSION})"
                ),
            ));
        }
        let generation = value
            .get("generation")
            .and_then(Json::as_u64)
            .ok_or_else(|| DbError::corrupt("manifest", "invalid or missing generation"))?;
        if generation > MAX_MANIFEST_GENERATION {
            return Err(DbError::corrupt(
                "manifest",
                "generation exceeds the publishable manifest range",
            ));
        }
        let last_seq = value
            .get("last_seq")
            .and_then(Json::as_u64)
            .ok_or_else(|| DbError::corrupt("manifest", "invalid or missing last_seq"))?;
        let snapshot = match value.get("snapshot") {
            None | Some(Json::Null) => None,
            Some(entry) => Some(SnapshotRef::from_json(entry)?),
        };
        if let Some(reference) = &snapshot {
            if generation != reference.generation || reference.seq > last_seq {
                return Err(DbError::corrupt(
                    "manifest",
                    "snapshot generation or sequence is inconsistent with manifest watermarks",
                ));
            }
        } else if generation != 0 || last_seq != 0 {
            return Err(DbError::corrupt(
                "manifest",
                "non-empty manifest watermarks require a snapshot reference",
            ));
        }
        let wal = match value.get("wal") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| DbError::corrupt("manifest", "'wal' must be an array"))?
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        DbError::corrupt("manifest", "'wal' entries must be strings")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        Ok(Manifest {
            format_version,
            generation,
            last_seq,
            snapshot,
            wal,
            created_at: match value.get("created_at") {
                None => 0,
                Some(value) => value
                    .as_i64()
                    .ok_or_else(|| DbError::corrupt("manifest", "invalid 'created_at'"))?,
            },
            updated_at: match value.get("updated_at") {
                None => 0,
                Some(value) => value
                    .as_i64()
                    .ok_or_else(|| DbError::corrupt("manifest", "invalid 'updated_at'"))?,
            },
        })
    }

    pub fn path(dir: &Path) -> PathBuf {
        dir.join(MANIFEST_FILE)
    }

    pub fn generation_path(dir: &Path, generation: u64) -> PathBuf {
        dir.join(format!(
            "{VERSIONED_MANIFEST_PREFIX}{generation:012}{VERSIONED_MANIFEST_SUFFIX}"
        ))
    }

    pub fn generation_from_name(name: &str) -> Option<u64> {
        let digits = name
            .strip_prefix(VERSIONED_MANIFEST_PREFIX)?
            .strip_suffix(VERSIONED_MANIFEST_SUFFIX)?;
        if digits.len() != 12 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    pub fn is_manifest_name(name: &str) -> bool {
        name == MANIFEST_FILE || Self::generation_from_name(name).is_some()
    }

    /// Best-effort removal of superseded generation files after the new generation is
    /// durable and authoritative. The legacy manifest is retained for older readers.
    pub fn remove_superseded_generations(dir: &Path, keep: u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if Self::generation_from_name(&name).is_some_and(|generation| generation != keep) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Load, returning `None` when the file does not exist yet.
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        Self::load_with_selection_hook(dir, |_| {})
    }

    fn load_with_selection_hook<F>(dir: &Path, mut after_select: F) -> Result<Option<Manifest>>
    where
        F: FnMut(&Path),
    {
        // Read-only engines do not take the writer lock. A concurrent checkpoint can
        // therefore retire the generation selected below before we open it. Retry a
        // bounded number of times so that this benign publication race cannot be
        // mistaken for a database with no manifest (or an empty database).
        const MAX_GENERATION_RETRIES: usize = 8;

        for attempt in 0..MAX_GENERATION_RETRIES {
            match Self::load_selected_generation(dir, &mut after_select)? {
                SelectedManifest::Loaded(manifest) => return Ok(manifest),
                SelectedManifest::Retry if attempt + 1 < MAX_GENERATION_RETRIES => continue,
                SelectedManifest::Retry => {
                    return Err(DbError::RecoveryRequired(
                        "manifest generations changed repeatedly while opening; retry the open"
                            .into(),
                    ));
                }
            }
        }
        unreachable!("bounded manifest retry loop always returns")
    }

    fn load_selected_generation<F>(dir: &Path, after_select: &mut F) -> Result<SelectedManifest>
    where
        F: FnMut(&Path),
    {
        let mut newest: Option<(u64, PathBuf)> = None;
        for entry in std::fs::read_dir(dir)
            .map_err(|err| DbError::io(format!("listing {}", dir.display()), err))?
        {
            let entry =
                entry.map_err(|err| DbError::io("reading manifest directory entry", err))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(generation) = Self::generation_from_name(&name) else {
                if name
                    .to_ascii_uppercase()
                    .starts_with(VERSIONED_MANIFEST_PREFIX)
                {
                    return Err(DbError::corrupt(
                        entry.path().display().to_string(),
                        "manifest generation has a noncanonical file name",
                    ));
                }
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|(current, _)| generation > *current)
            {
                newest = Some((generation, entry.path()));
            }
        }
        let (expected_generation, path) = match newest {
            Some((generation, path)) => (Some(generation), path),
            None => (None, Manifest::path(dir)),
        };
        after_select(&path);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "manifest is not a regular file",
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return if expected_generation.is_some() {
                    Ok(SelectedManifest::Retry)
                } else {
                    Ok(SelectedManifest::Loaded(None))
                };
            }
            Err(err) => return Err(DbError::io("inspecting manifest file", err)),
        }
        let text = match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((MAX_MANIFEST_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|err| DbError::io(format!("reading {}", path.display()), err))?;
                if bytes.len() > MAX_MANIFEST_BYTES {
                    return Err(DbError::corrupt(
                        path.display().to_string(),
                        "manifest exceeds the maximum size",
                    ));
                }
                String::from_utf8(bytes).map_err(|err| {
                    DbError::corrupt(
                        path.display().to_string(),
                        format!("manifest is not valid UTF-8: {err}"),
                    )
                })?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return if expected_generation.is_some() {
                    Ok(SelectedManifest::Retry)
                } else {
                    Ok(SelectedManifest::Loaded(None))
                };
            }
            Err(err) => return Err(DbError::io(format!("reading {}", path.display()), err)),
        };
        let value = preppydb_json::parse(&text).map_err(|err| {
            DbError::corrupt(path.display().to_string(), format!("invalid JSON: {err}"))
        })?;
        let manifest = Manifest::from_json(&value)?;
        if expected_generation.is_some_and(|generation| generation != manifest.generation) {
            return Err(DbError::corrupt(
                path.display().to_string(),
                "manifest filename generation does not match its contents",
            ));
        }
        Ok(SelectedManifest::Loaded(Some(manifest)))
    }

    /// Publish an immutable checkpoint manifest at a generation-specific path.
    /// The destination must not exist, so a crash cannot expose a missing or
    /// partially replaced previous manifest.
    pub fn publish_generation(&mut self, dir: &Path) -> Result<()> {
        if self.generation > MAX_MANIFEST_GENERATION {
            return Err(DbError::LimitExceeded {
                limit: "manifest_generation",
                max: MAX_MANIFEST_GENERATION,
                actual: self.generation,
            });
        }
        self.updated_at = crate::clock::now_ms();
        let path = Self::generation_path(dir, self.generation);
        let tmp = dir.join(format!(
            ".{VERSIONED_MANIFEST_PREFIX}{:012}{VERSIONED_MANIFEST_SUFFIX}.tmp",
            self.generation
        ));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(DbError::corrupt(
                    path.display().to_string(),
                    "manifest generation already exists",
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(DbError::io("inspecting manifest destination", err)),
        }
        match std::fs::symlink_metadata(&tmp) {
            Ok(_) => std::fs::remove_file(&tmp)
                .map_err(|err| DbError::io("removing stale temporary manifest", err))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(DbError::io("inspecting temporary manifest", err)),
        }

        use std::io::Write;
        let text = format!("{}\n", preppydb_json::to_string_pretty(&self.to_json()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|err| DbError::io(format!("creating {}", tmp.display()), err))?;
        file.write_all(text.as_bytes())
            .map_err(|err| DbError::io("writing checkpoint manifest", err))?;
        file.sync_all()
            .map_err(|err| DbError::io("syncing checkpoint manifest", err))?;
        drop(file);
        std::fs::rename(&tmp, &path).map_err(|err| {
            DbError::io(
                format!("publishing checkpoint manifest {}", path.display()),
                err,
            )
        })?;
        fsync_dir(dir)
    }

    /// Atomically persist the manifest.
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        self.updated_at = crate::clock::now_ms();
        let path = Manifest::path(dir);
        let tmp = dir.join("MANIFEST.json.tmp");
        match std::fs::symlink_metadata(&tmp) {
            Ok(_) => std::fs::remove_file(&tmp)
                .map_err(|err| DbError::io(format!("removing stale {}", tmp.display()), err))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(DbError::io(format!("inspecting {}", tmp.display()), err)),
        }
        let text = preppydb_json::to_string_pretty(&self.to_json());
        let mut options = std::fs::OpenOptions::new();
        // Exclusive creation prevents a symlink planted after the stale-temp check
        // from redirecting the manifest write outside the database directory.
        options.write(true).create_new(true);
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

enum SelectedManifest {
    Loaded(Option<Manifest>),
    Retry,
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
            sha256: "a".repeat(64),
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

    #[cfg(unix)]
    #[test]
    fn save_does_not_follow_a_symlinked_temporary_manifest() {
        use std::os::unix::fs::symlink;

        let dir = tempdir("symlinked-temp");
        let outside_dir = tempdir("symlink-target");
        let outside = outside_dir.join("sentinel");
        std::fs::write(&outside, b"keep this content").unwrap();
        symlink(&outside, dir.join("MANIFEST.json.tmp")).unwrap();

        let mut manifest = Manifest::default();
        manifest.save(&dir).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"keep this content");
        assert!(Manifest::path(&dir).is_file());
        assert!(!dir.join("MANIFEST.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(outside_dir).unwrap();
    }

    #[test]
    fn generation_publication_preserves_legacy_manifest_and_loads_newest() {
        let dir = tempdir("generation-publication");
        let mut legacy = Manifest {
            generation: 1,
            last_seq: 10,
            snapshot: Some(SnapshotRef {
                file: "snap-000000000001.pdb".into(),
                seq: 10,
                generation: 1,
                sha256: "b".repeat(64),
                doc_count: 0,
                created_at: 1,
                bytes: 1,
            }),
            ..Default::default()
        };
        legacy.save(&dir).unwrap();
        let legacy_bytes = std::fs::read(Manifest::path(&dir)).unwrap();

        let mut next = Manifest {
            generation: 2,
            last_seq: 20,
            snapshot: Some(SnapshotRef {
                file: "snap-000000000002.pdb".into(),
                seq: 20,
                generation: 2,
                sha256: "c".repeat(64),
                doc_count: 0,
                created_at: 2,
                bytes: 1,
            }),
            ..Default::default()
        };
        next.publish_generation(&dir).unwrap();
        assert_eq!(std::fs::read(Manifest::path(&dir)).unwrap(), legacy_bytes);
        assert_eq!(
            Manifest::load(&dir).unwrap().unwrap().last_seq,
            20,
            "generation file must supersede the legacy manifest"
        );
        assert!(Manifest::generation_path(&dir, 2).is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_generation_name_does_not_fall_back_to_older_manifest() {
        let dir = tempdir("malformed-generation-name");
        let mut older = Manifest::default();
        older.save(&dir).unwrap();
        let mut newer = Manifest {
            generation: 2,
            snapshot: Some(SnapshotRef {
                file: "snap-000000000002.pdb".into(),
                seq: 0,
                generation: 2,
                sha256: "a".repeat(64),
                doc_count: 0,
                created_at: 1,
                bytes: 1,
            }),
            ..Default::default()
        };
        newer.publish_generation(&dir).unwrap();
        std::fs::rename(
            Manifest::generation_path(&dir, 2),
            dir.join("MANIFEST-+00000000002.json"),
        )
        .unwrap();

        let error = Manifest::load(&dir).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_retries_when_checkpoint_retires_selected_generation() {
        let dir = tempdir("generation-retired-during-open");
        for generation in [1, 2] {
            let mut manifest = Manifest {
                generation,
                last_seq: generation * 10,
                snapshot: Some(SnapshotRef {
                    file: format!("snap-{:012}.pdb", generation),
                    seq: generation * 10,
                    generation,
                    sha256: "a".repeat(64),
                    doc_count: 0,
                    created_at: 1,
                    bytes: 1,
                }),
                ..Default::default()
            };
            manifest.publish_generation(&dir).unwrap();
        }

        let mut checkpoint_published = false;
        let loaded = Manifest::load_with_selection_hook(&dir, |selected| {
            if !checkpoint_published {
                assert_eq!(selected, Manifest::generation_path(&dir, 2));
                std::fs::remove_file(selected).unwrap();
                let mut next = Manifest {
                    generation: 3,
                    last_seq: 30,
                    snapshot: Some(SnapshotRef {
                        file: "snap-000000000003.pdb".into(),
                        seq: 30,
                        generation: 3,
                        sha256: "b".repeat(64),
                        doc_count: 0,
                        created_at: 2,
                        bytes: 1,
                    }),
                    ..Default::default()
                };
                next.publish_generation(&dir).unwrap();
                checkpoint_published = true;
            }
        })
        .unwrap()
        .unwrap();

        assert!(checkpoint_published);
        assert_eq!(loaded.generation, 3);
        assert_eq!(loaded.last_seq, 30);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_bounds_retries_when_manifest_generations_keep_changing() {
        let dir = tempdir("generation-churn-during-open");
        let mut first = Manifest {
            generation: 1,
            last_seq: 10,
            snapshot: Some(SnapshotRef {
                file: "snap-000000000001.pdb".into(),
                seq: 10,
                generation: 1,
                sha256: "a".repeat(64),
                doc_count: 0,
                created_at: 1,
                bytes: 1,
            }),
            ..Default::default()
        };
        first.publish_generation(&dir).unwrap();

        let mut publications = 0;
        let error = Manifest::load_with_selection_hook(&dir, |selected| {
            let generation =
                Manifest::generation_from_name(selected.file_name().unwrap().to_str().unwrap())
                    .unwrap();
            std::fs::remove_file(selected).unwrap();
            let next_generation = generation + 1;
            let mut next = Manifest {
                generation: next_generation,
                last_seq: next_generation * 10,
                snapshot: Some(SnapshotRef {
                    file: format!("snap-{next_generation:012}.pdb"),
                    seq: next_generation * 10,
                    generation: next_generation,
                    sha256: "b".repeat(64),
                    doc_count: 0,
                    created_at: 2,
                    bytes: 1,
                }),
                ..Default::default()
            };
            next.publish_generation(&dir).unwrap();
            publications += 1;
        })
        .unwrap_err();

        assert_eq!(publications, 8, "manifest retries must have a fixed bound");
        assert_eq!(error.code(), crate::error::codes::RECOVERY_REQUIRED);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_newest_generation_fails_closed_instead_of_falling_back() {
        let dir = tempdir("corrupt-newest-generation");
        let mut old = Manifest {
            generation: 1,
            last_seq: 10,
            ..Default::default()
        };
        old.publish_generation(&dir).unwrap();
        std::fs::write(Manifest::generation_path(&dir, 2), b"partial").unwrap();
        assert!(Manifest::load(&dir).is_err());
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
    fn manifest_format_version_does_not_wrap_on_integer_conversion() {
        for version in [u32::MAX as u64 + 1, u64::MAX] {
            let value = Json::obj().with("format_version", Json::uint(version));
            assert!(Manifest::from_json(&value).is_err(), "accepted {version}");
        }
        let wrapped_supported = Json::obj().with(
            "format_version",
            Json::uint(u32::MAX as u64 + MANIFEST_FORMAT_VERSION as u64 + 1),
        );
        assert!(Manifest::from_json(&wrapped_supported).is_err());
    }

    #[test]
    fn invalid_json_is_corruption() {
        let dir = tempdir("json");
        std::fs::write(dir.join(MANIFEST_FILE), "{not json").unwrap();
        let err = Manifest::load(&dir).unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_or_inconsistent_watermarks_are_corruption() {
        let valid = Manifest::default().to_json();
        let mut missing_generation = valid.clone();
        missing_generation.remove_key("generation");
        assert!(Manifest::from_json(&missing_generation).is_err());

        let mut missing_sequence = valid.clone();
        missing_sequence.remove_key("last_seq");
        assert!(Manifest::from_json(&missing_sequence).is_err());

        let inconsistent_empty = valid
            .clone()
            .with("generation", Json::uint(1))
            .with("last_seq", Json::uint(1));
        assert!(Manifest::from_json(&inconsistent_empty).is_err());

        let inconsistent_snapshot = valid
            .with("generation", Json::uint(2))
            .with("last_seq", Json::uint(4))
            .with(
                "snapshot",
                Json::obj()
                    .with("file", Json::str("snap-000000000001.pdb"))
                    .with("generation", Json::uint(1))
                    .with("seq", Json::uint(5))
                    .with("sha256", Json::str("d".repeat(64)))
                    .with("doc_count", Json::uint(0))
                    .with("created_at", Json::int(1))
                    .with("bytes", Json::uint(100)),
            );
        assert!(Manifest::from_json(&inconsistent_snapshot).is_err());
    }

    #[test]
    fn legacy_manifest_rejects_unpublishable_generation() {
        let manifest = Manifest {
            generation: MAX_MANIFEST_GENERATION + 1,
            snapshot: Some(SnapshotRef {
                file: "snap-1000000000000.pdb".into(),
                seq: 0,
                generation: MAX_MANIFEST_GENERATION + 1,
                sha256: "a".repeat(64),
                doc_count: 0,
                created_at: 0,
                bytes: 1,
            }),
            ..Default::default()
        };
        assert!(Manifest::from_json(&manifest.to_json()).is_err());
    }

    #[test]
    fn manifest_rejects_present_invalid_timestamps() {
        let valid = Manifest::default().to_json();
        for field in ["created_at", "updated_at"] {
            let malformed = valid.clone().with(field, Json::str("bad"));
            assert!(Manifest::from_json(&malformed).is_err(), "accepted {field}");
        }
        let mut legacy = valid;
        legacy.remove_key("created_at");
        legacy.remove_key("updated_at");
        let loaded = Manifest::from_json(&legacy).unwrap();
        assert_eq!(loaded.created_at, 0);
        assert_eq!(loaded.updated_at, 0);
    }

    #[test]
    fn snapshot_reference_requires_checksum_and_length_metadata() {
        let valid = SnapshotRef {
            file: "snap-000000000001.pdb".into(),
            seq: 1,
            generation: 1,
            sha256: "a".repeat(64),
            doc_count: 0,
            created_at: 1,
            bytes: 100,
        }
        .to_json();
        assert!(SnapshotRef::from_json(&valid).is_ok());

        let mut missing_hash = valid.clone();
        missing_hash.remove_key("sha256");
        assert!(SnapshotRef::from_json(&missing_hash).is_err());

        let mut missing_length = valid.clone();
        missing_length.remove_key("bytes");
        assert!(SnapshotRef::from_json(&missing_length).is_err());

        let invalid_hash = valid.clone().with("sha256", Json::str("not-a-checksum"));
        assert!(SnapshotRef::from_json(&invalid_hash).is_err());

        let zero_length = valid.with("bytes", Json::uint(0));
        assert!(SnapshotRef::from_json(&zero_length).is_err());
    }

    #[test]
    fn oversized_manifest_is_rejected_before_unbounded_read() {
        let dir = tempdir("oversized");
        let path = Manifest::path(&dir);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_MANIFEST_BYTES + 1) as u64).unwrap();
        drop(file);
        assert!(Manifest::load(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
