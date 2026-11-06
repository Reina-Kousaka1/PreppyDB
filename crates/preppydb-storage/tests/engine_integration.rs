//! End-to-end tests for the storage engine: persistence, recovery, transactions,
//! backups and maintenance. Run with `cargo test -p preppydb-storage`.

use preppydb_crypto::crc32c;
use preppydb_json::Json;
use preppydb_storage::backup;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::{Engine, EngineConfig, PutOptions, RepairPolicy, ScanOptions};
use preppydb_storage::error::DbError;
use preppydb_storage::manifest::{Manifest, SnapshotRef};
use preppydb_storage::record::{Op, Record};
use preppydb_storage::snapshot;
use preppydb_storage::store::ApplyMode;
use preppydb_storage::wal::{self, SyncMode};
use std::path::{Path, PathBuf};

fn tempdir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "preppydb-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn open(dir: &Path) -> Engine {
    let mut cfg = EngineConfig::new(dir);
    cfg.sync_mode = SyncMode::Always;
    Engine::open(cfg).unwrap()
}

fn save_active_manifest(dir: &Path, manifest: &Manifest) {
    let path = if manifest.generation == 0 {
        Manifest::path(dir)
    } else {
        Manifest::generation_path(dir, manifest.generation)
    };
    std::fs::write(path, format!("{}\n", manifest.to_json())).unwrap();
}

fn remove_all_manifests(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if Manifest::is_manifest_name(&name) {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
}

fn doc(name: &str) -> Json {
    Json::obj().with("name", Json::str(name))
}

#[test]
fn write_close_reopen_persists() {
    let dir = tempdir("persist");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("people", 0))
            .unwrap();
        engine
            .put("people", "molly", doc("Molly"), PutOptions::default())
            .unwrap();
        engine
            .put("people", "tori", doc("Tori"), PutOptions::default())
            .unwrap();
        // No explicit checkpoint: durability comes from the WAL alone.
    }
    {
        let engine = open(&dir);
        assert_eq!(
            engine.get("people", "molly").unwrap().value.get_str("name"),
            Some("Molly")
        );
        assert_eq!(engine.count("people").unwrap(), 2);
        assert!(engine.recovery_notes().is_empty());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failed_manifest_publication_stops_writes_and_preserves_prior_checkpoint() {
    let dir = tempdir("manifest-publication-failure");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("saved"), PutOptions::default())
        .unwrap();
    engine.checkpoint().unwrap();
    let original_manifest = std::fs::read(Manifest::generation_path(&dir, 1)).unwrap();
    let blocked_temp = dir.join(".MANIFEST-000000000002.json.tmp");
    std::fs::create_dir(&blocked_temp).unwrap();
    assert!(engine.checkpoint().is_err());
    assert_eq!(
        std::fs::read(Manifest::generation_path(&dir, 1)).unwrap(),
        original_manifest
    );
    assert!(engine
        .put("c", "later", doc("no"), PutOptions::default())
        .is_err());
    drop(engine);
    std::fs::remove_dir(blocked_temp).unwrap();
    let reopened = open(&dir);
    assert_eq!(
        reopened.get("c", "key").unwrap().value.get_str("name"),
        Some("saved")
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_published_snapshot_fails_closed_after_checkpoint() {
    let dir = tempdir("missing-published-snapshot");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    let snapshot = engine.checkpoint().unwrap();
    drop(engine);
    std::fs::remove_file(&snapshot.path).unwrap();
    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn manifest_snapshot_path_traversal_is_rejected_before_file_access() {
    let dir = tempdir("manifest-snapshot-path-traversal");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    engine.checkpoint().unwrap();
    drop(engine);

    let mut manifest = Manifest::load(&dir).unwrap().unwrap();
    manifest.snapshot.as_mut().unwrap().file = "../../MANIFEST.json".into();
    save_active_manifest(&dir, &manifest);

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn manifest_snapshot_header_metadata_mismatch_fails_closed() {
    let dir = tempdir("manifest-snapshot-metadata-mismatch");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    engine.checkpoint().unwrap();
    drop(engine);

    let mut manifest = Manifest::load(&dir).unwrap().unwrap();
    manifest.snapshot.as_mut().unwrap().seq += 1;
    manifest.last_seq += 1;
    save_active_manifest(&dir, &manifest);

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recovery_rejects_duplicate_document_keys_in_snapshot() {
    let dir = tempdir("duplicate-snapshot-document-key");
    let snap_dir = dir.join("snap");
    std::fs::create_dir_all(&snap_dir).unwrap();
    let snapshot_path = snap_dir.join("snap-000000000001.pdb");
    let meta = CollectionMeta::new("people", 0);
    let document = preppydb_storage::store::Doc {
        value: doc("same"),
        version: 1,
        revision: 0,
        created_at: 1,
        updated_at: 1,
        expires_at: None,
    };
    let written = snapshot::write_snapshot(
        &snapshot_path,
        1,
        0,
        1,
        &[(
            meta,
            vec![("same".into(), document.clone()), ("same".into(), document)],
        )],
    )
    .unwrap();

    let manifest = Manifest {
        generation: 1,
        snapshot: Some(SnapshotRef {
            file: snapshot_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            seq: 0,
            generation: 1,
            sha256: written.body_sha256,
            doc_count: written.header.doc_count,
            created_at: written.header.created_at,
            bytes: written.bytes,
        }),
        ..Manifest::default()
    };
    save_active_manifest(&dir, &manifest);

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn manifest_snapshot_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempdir("manifest-snapshot-symlink");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    let snapshot = engine.checkpoint().unwrap();
    drop(engine);

    let external_snapshot = dir.join("outside-snapshot.pdb");
    std::fs::copy(&snapshot.path, &external_snapshot).unwrap();
    std::fs::remove_file(&snapshot.path).unwrap();
    symlink("../outside-snapshot.pdb", &snapshot.path).unwrap();

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn incomplete_legacy_manifest_with_snapshot_fails_closed() {
    let dir = tempdir("incomplete-legacy-manifest");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", Json::str("committed"), PutOptions::default())
        .unwrap();
    let checkpoint = engine.checkpoint().unwrap();
    drop(engine);

    assert!(checkpoint.path.is_file());
    remove_all_manifests(&dir);
    std::fs::write(Manifest::path(&dir), r#"{"format_version":1}"#).unwrap();

    let error = Engine::open(EngineConfig::new(&dir)).unwrap_err();
    assert!(matches!(error, DbError::Corrupt { .. }), "{error}");
    assert!(
        checkpoint.path.is_file(),
        "recovery must not delete the snapshot"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_manifest_after_retired_wal_history_fails_closed() {
    let dir = tempdir("missing-manifest-retired-history");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    let snapshot = engine.checkpoint().unwrap();
    drop(engine);

    remove_all_manifests(&dir);
    std::fs::remove_file(snapshot.path).unwrap();

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn manifestless_wal_must_start_at_sequence_one() {
    let dir = tempdir("manifestless-wal-sequence-gap");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "key", doc("value"), PutOptions::default())
        .unwrap();
    drop(engine);

    let segment = wal::list_segments(&dir.join("wal")).unwrap().remove(0);
    let mut bytes = std::fs::read(&segment).unwrap();
    let frame = wal::WAL_HEADER_LEN as usize;
    let payload_len = u32::from_le_bytes(bytes[frame..frame + 4].try_into().unwrap()) as usize;
    let payload_start = frame + 8;
    let mut record =
        Record::decode_payload(&bytes[payload_start..payload_start + payload_len]).unwrap();
    assert_eq!(record.seq, 1);
    record.seq = 2;
    let payload = record.encode_payload();
    assert_eq!(payload.len(), payload_len);
    bytes[payload_start..payload_start + payload_len].copy_from_slice(&payload);
    bytes[frame + 4..frame + 8].copy_from_slice(&crc32c(&payload, 0).to_le_bytes());
    std::fs::write(&segment, bytes).unwrap();
    assert!(Manifest::load(&dir).unwrap().is_none());
    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn manifestless_complete_wal_replays_from_sequence_one() {
    let dir = tempdir("manifestless-complete-wal");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "key", doc("value"), PutOptions::default())
            .unwrap();
    }
    assert!(Manifest::load(&dir).unwrap().is_none());
    let reopened = open(&dir);
    assert_eq!(
        reopened.get("c", "key").unwrap().value.get_str("name"),
        Some("value")
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn backup_remains_verifiable_during_concurrent_writes() {
    let dir = tempdir("backup-concurrent");
    let archive = dir.with_extension("ppdbbak");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            for i in 0..50 {
                engine
                    .put("c", &format!("key{i}"), Json::int(i), PutOptions::default())
                    .unwrap();
            }
        });
        barrier.wait();
        backup::create_backup(&engine, &archive, false).unwrap();
    });
    assert_eq!(
        backup::verify_backup(&archive)
            .unwrap()
            .get("ok")
            .and_then(Json::as_bool),
        Some(true)
    );
    drop(engine);
    std::fs::remove_file(archive).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checkpoint_then_replay_is_consistent() {
    let dir = tempdir("checkpoint");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    for i in 0..50 {
        engine
            .put(
                "c",
                &format!("k{i}"),
                Json::int(i as i64),
                PutOptions::default(),
            )
            .unwrap();
    }
    let written = engine.checkpoint().unwrap();
    assert_eq!(written.header.doc_count, 50);
    let seq_at_checkpoint = written.header.seq;
    // Nothing changed after the checkpoint, so the log should be nearly empty.
    let segments = wal::list_segments(&engine.paths().wal_dir).unwrap();
    assert_eq!(segments.len(), 1, "checkpoint should retire old segments");
    for i in 50..60 {
        engine
            .put(
                "c",
                &format!("k{i}"),
                Json::int(i as i64),
                PutOptions::default(),
            )
            .unwrap();
    }
    let last_seq = engine.last_seq();
    assert!(last_seq > seq_at_checkpoint);
    drop(engine);

    let engine = open(&dir);
    assert_eq!(engine.count("c").unwrap(), 60);
    assert_eq!(
        engine.last_seq(),
        last_seq,
        "sequence numbers must survive a restart"
    );
    // Indexes defined before the checkpoint must be rebuilt from the snapshot.
    let value = engine.get("c", "k59").unwrap();
    assert_eq!(value.value.as_i64(), Some(59));
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn orphan_snapshot_and_partial_manifest_temp_preserve_previous_generation() {
    let dir = tempdir("checkpoint-crash-before-manifest");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "before", Json::int(1), PutOptions::default())
        .unwrap();
    let first = engine.checkpoint().unwrap();
    engine
        .put("c", "after-a", Json::int(2), PutOptions::default())
        .unwrap();
    engine
        .put("c", "after-b", Json::int(3), PutOptions::default())
        .unwrap();

    let current_rows = engine.scan("c", &ScanOptions::default()).unwrap();
    let metadata = snapshot::read_snapshot(&first.path).unwrap().collections[0]
        .0
        .clone();
    let orphan = dir.join("snap").join("snap-000000000002.pdb");
    let orphan_written = snapshot::write_snapshot(
        &orphan,
        2,
        engine.last_seq(),
        2,
        &[(metadata, current_rows)],
    )
    .unwrap();
    assert!(orphan_written.path.exists());
    // A crash during the temporary manifest write leaves the old published file
    // intact and an incomplete temporary beside it.
    std::fs::write(
        dir.join(".MANIFEST-000000000002.json.tmp"),
        b"{\"format_version\":",
    )
    .unwrap();
    drop(engine);

    let reopened = open(&dir);
    assert_eq!(reopened.count("c").unwrap(), 3);
    assert_eq!(
        reopened.get("c", "after-b").unwrap().value.as_i64(),
        Some(3)
    );
    assert_eq!(reopened.last_seq(), 4);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn published_snapshot_with_unretired_wal_reopens_consistently() {
    let dir = tempdir("checkpoint-crash-after-manifest");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "a", Json::int(1), PutOptions::default())
        .unwrap();
    let first = engine.checkpoint().unwrap();
    engine
        .put("c", "b", Json::int(2), PutOptions::default())
        .unwrap();
    engine
        .put("c", "c", Json::int(3), PutOptions::default())
        .unwrap();
    let current_rows = engine.scan("c", &ScanOptions::default()).unwrap();
    let metadata = snapshot::read_snapshot(&first.path).unwrap().collections[0]
        .0
        .clone();
    let new_snapshot_path = dir.join("snap").join("snap-000000000002.pdb");
    let written = snapshot::write_snapshot(
        &new_snapshot_path,
        2,
        engine.last_seq(),
        2,
        &[(metadata, current_rows)],
    )
    .unwrap();
    let snapshot_seq = engine.last_seq();
    let wal_dir = dir.join("wal");
    let next_segment = wal::list_segments(&wal_dir)
        .unwrap()
        .last()
        .and_then(|path| path.file_name())
        .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
        .unwrap()
        + 1;
    drop(engine);

    // Checkpoint rotates before publishing. Simulate a crash after that publication
    // but before deleting the WAL prefix already represented by the snapshot.
    let keep = wal::segment_name(next_segment);
    let empty =
        wal::WalWriter::open(&wal_dir, next_segment, 64 * 1024 * 1024, SyncMode::Always).unwrap();
    drop(empty);
    let mut manifest = Manifest::load(&dir).unwrap().unwrap();
    manifest.generation = 2;
    manifest.last_seq = snapshot_seq;
    manifest.snapshot = Some(SnapshotRef {
        file: written
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        seq: written.header.seq,
        generation: written.header.generation,
        sha256: written.body_sha256,
        doc_count: written.header.doc_count,
        created_at: written.header.created_at,
        bytes: written.bytes,
    });
    manifest.wal = vec![keep];
    save_active_manifest(&dir, &manifest);

    let reopened = open(&dir);
    assert_eq!(reopened.count("c").unwrap(), 3);
    assert_eq!(reopened.last_seq(), snapshot_seq);
    assert_eq!(reopened.get("c", "b").unwrap().value.as_i64(), Some(2));
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn secondary_index_survives_restart() {
    let dir = tempdir("index");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("items", 0))
            .unwrap();
        engine
            .create_index(
                "items",
                IndexMeta {
                    name: "by_price".into(),
                    field: "price".into(),
                    unique: false,
                    sparse: true,
                    created_at: 0,
                },
            )
            .unwrap();
        engine
            .put(
                "items",
                "hat",
                Json::obj().with("price", Json::int(10)),
                PutOptions::default(),
            )
            .unwrap();
        engine
            .put(
                "items",
                "dress",
                Json::obj().with("price", Json::int(50)),
                PutOptions::default(),
            )
            .unwrap();
        engine.checkpoint().unwrap();
    }
    let engine = open(&dir);
    let hits = engine
        .find_by_index("items", "by_price", &Json::int(50), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "dress");
    let range = engine
        .range_by_index(
            "items",
            "by_price",
            Some(&Json::int(10)),
            Some(&Json::int(60)),
            10,
        )
        .unwrap();
    assert_eq!(range.len(), 2);
    assert_eq!(
        range[0].0, "hat",
        "range scans are ordered by indexed value"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn transaction_write_limit_applies_to_deletes_and_mixed_writes() {
    let dir = tempdir("txn-delete-limit");
    let mut cfg = EngineConfig::new(&dir);
    cfg.limits.max_batch_ops = 1;
    let engine = Engine::open(cfg).unwrap();
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "a", Json::int(1), PutOptions::default())
        .unwrap();
    engine
        .put("c", "b", Json::int(2), PutOptions::default())
        .unwrap();
    let mut txn = engine.begin();
    txn.delete("c", "a").unwrap();
    assert!(matches!(
        txn.delete("c", "b"),
        Err(DbError::LimitExceeded {
            limit: "max_txn_writes",
            ..
        })
    ));
    assert!(matches!(
        txn.put("c", "b", Json::int(3), None),
        Err(DbError::LimitExceeded {
            limit: "max_txn_writes",
            ..
        })
    ));
    // Replacing a buffered operation on the same key does not consume another slot.
    txn.put("c", "a", Json::int(4), None).unwrap();
    txn.commit().unwrap();
    assert_eq!(engine.get("c", "a").unwrap().value.as_i64(), Some(4));
    assert_eq!(engine.get("c", "b").unwrap().value.as_i64(), Some(2));
    drop(engine);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transaction_unique_conflict_restores_prior_documents_and_indexes() {
    let dir = tempdir("txn-unique-undo");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .create_index(
            "c",
            IndexMeta {
                name: "unique_name".into(),
                field: "name".into(),
                unique: true,
                sparse: true,
                created_at: 0,
            },
        )
        .unwrap();
    engine
        .put("c", "a", doc("first"), PutOptions::default())
        .unwrap();
    engine
        .put("c", "z", doc("taken"), PutOptions::default())
        .unwrap();
    let mut txn = engine.begin();
    txn.put("c", "a", doc("changed"), None).unwrap();
    txn.put("c", "b", doc("taken"), None).unwrap();
    assert!(txn.commit().is_err());
    assert_eq!(
        engine.get("c", "a").unwrap().value.get_str("name"),
        Some("first")
    );
    assert!(engine.get_opt("c", "b").unwrap().is_none());
    assert!(engine
        .find_by_index("c", "unique_name", &Json::str("changed"), 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        engine
            .find_by_index("c", "unique_name", &Json::str("first"), 10)
            .unwrap()
            .len(),
        1
    );
    drop(engine);
    let reopened = open(&dir);
    assert_eq!(
        reopened.get("c", "a").unwrap().value.get_str("name"),
        Some("first")
    );
    assert!(reopened.get_opt("c", "b").unwrap().is_none());
    assert_eq!(
        reopened
            .find_by_index("c", "unique_name", &Json::str("first"), 10)
            .unwrap()
            .len(),
        1
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transactions_commit_atomically() {
    let dir = tempdir("txn");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("accounts", 0))
        .unwrap();
    engine
        .put("accounts", "a", Json::int(100), PutOptions::default())
        .unwrap();
    engine
        .put("accounts", "b", Json::int(0), PutOptions::default())
        .unwrap();

    let mut txn = engine.begin();
    let a = txn.get("accounts", "a").unwrap().unwrap().as_i64().unwrap();
    let b = txn.get("accounts", "b").unwrap().unwrap().as_i64().unwrap();
    txn.put("accounts", "a", Json::int(a - 30), None).unwrap();
    txn.put("accounts", "b", Json::int(b + 30), None).unwrap();
    let outcome = txn.commit().unwrap();
    assert_eq!(outcome.puts, 2);
    assert_eq!(
        engine.get("accounts", "a").unwrap().value.as_i64(),
        Some(70)
    );
    assert_eq!(
        engine.get("accounts", "b").unwrap().value.as_i64(),
        Some(30)
    );
    drop(engine);

    // The whole transaction must be a single log record.
    let engine = open(&dir);
    assert_eq!(
        engine.get("accounts", "b").unwrap().value.as_i64(),
        Some(30)
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn transaction_conflict_is_detected() {
    let dir = tempdir("conflict");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "k", Json::int(1), PutOptions::default())
        .unwrap();

    let mut first = engine.begin();
    let mut second = engine.begin();
    let _ = first.get("c", "k").unwrap();
    let _ = second.get("c", "k").unwrap();
    first.put("c", "k", Json::int(2), None).unwrap();
    second.put("c", "k", Json::int(3), None).unwrap();
    first.commit().unwrap();
    let err = second.commit().unwrap_err();
    match err {
        DbError::TransactionConflict { details } => {
            assert!(details[0].contains("c/k"), "{details:?}");
        }
        other => panic!("expected a conflict, got {other}"),
    }
    assert_eq!(engine.get("c", "k").unwrap().value.as_i64(), Some(2));
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_transactions_cannot_lose_a_document_update() {
    use std::sync::{Arc, Barrier};

    let dir = tempdir("concurrent-txn-conflict");
    let engine = Arc::new(open(&dir));
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "k", Json::int(1), PutOptions::default())
        .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let workers = [2, 3].map(|value| {
        let engine = Arc::clone(&engine);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut txn = engine.begin();
            assert_eq!(txn.get("c", "k").unwrap().unwrap().as_i64(), Some(1));
            txn.put("c", "k", Json::int(value), None).unwrap();
            barrier.wait();
            txn.commit()
        })
    });
    let outcomes = workers.map(|worker| worker.join().unwrap());
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(DbError::TransactionConflict { .. })))
            .count(),
        1
    );
    assert!(matches!(
        engine.get("c", "k").unwrap().value.as_i64(),
        Some(2 | 3)
    ));

    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn transaction_read_your_writes_and_rollback() {
    let dir = tempdir("txn2");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    let mut txn = engine.begin();
    txn.put("c", "x", Json::str("staged"), None).unwrap();
    assert_eq!(txn.get("c", "x").unwrap().unwrap().as_str(), Some("staged"));
    assert!(
        engine.get_opt("c", "x").unwrap().is_none(),
        "uncommitted writes must be invisible"
    );
    txn.rollback();
    assert!(engine.get_opt("c", "x").unwrap().is_none());
    assert_eq!(engine.count("c").unwrap(), 0);
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn batch_is_atomic_across_restart() {
    let dir = tempdir("batch");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    let ops = vec![
        Op::Put {
            collection: "c".into(),
            key: "a".into(),
            value: Json::int(1),
            expires_at: None,
        },
        Op::Put {
            collection: "c".into(),
            key: "b".into(),
            value: Json::int(2),
            expires_at: None,
        },
        Op::Delete {
            collection: "c".into(),
            key: "does-not-exist".into(),
        },
    ];
    let err = engine.batch(ops).unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }));
    assert_eq!(
        engine.count("c").unwrap(),
        0,
        "failed batch must not leave documents"
    );
    let ok = vec![
        Op::Put {
            collection: "c".into(),
            key: "a".into(),
            value: Json::int(1),
            expires_at: None,
        },
        Op::Put {
            collection: "c".into(),
            key: "b".into(),
            value: Json::int(2),
            expires_at: None,
        },
    ];
    assert_eq!(engine.batch(ok).unwrap(), 2);
    drop(engine);
    let engine = open(&dir);
    assert_eq!(engine.count("c").unwrap(), 2);
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ttl_expires_and_sweep_is_logged() {
    let dir = tempdir("ttl");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("cache", 0))
        .unwrap();
    let now = preppydb_storage::clock::now_ms();
    engine
        .put(
            "cache",
            "short",
            Json::str("v"),
            PutOptions {
                expected_version: None,
                expires_at: Some(now - 1),
            },
        )
        .unwrap();
    assert!(
        engine.get_opt("cache", "short").unwrap().is_none(),
        "expired documents are invisible"
    );
    let removed = engine.sweep_expired().unwrap();
    assert_eq!(removed, 1);
    let stats = engine.stats();
    assert_eq!(stats.total_documents, 0);
    drop(engine);
    // The sweep was logged, so it survives a restart (nothing resurrects).
    let engine = open(&dir);
    assert_eq!(engine.count("cache").unwrap(), 0);
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crash_recovery_keeps_committed_writes() {
    // A child process writes documents and then aborts without any clean shutdown.
    // Everything that was committed with sync=always must be present afterwards.
    //
    // Running the tests under emulation (CI uses qemu-user for aarch64) cannot exec the
    // test binary a second time without binfmt_misc, so the job sets this flag and the
    // test is skipped there instead of failing for the wrong reason.
    if std::env::var_os("PREPPYDB_SKIP_CHILD_CRASH").is_some() {
        eprintln!("skipping crash test: PREPPYDB_SKIP_CHILD_CRASH is set (emulated run)");
        return;
    }
    let dir = tempdir("crash");
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new(&exe)
        .args(["--exact", "crash_child_writer", "--ignored", "--nocapture"])
        .env("PREPPYDB_CRASH_DIR", &dir)
        .status()
        .expect("spawn child test process");
    assert!(
        !status.success(),
        "child was expected to abort, got {status:?}"
    );

    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap_or(());
    let count = engine.count("c").unwrap();
    assert_eq!(count, 100, "all committed documents must survive the crash");
    assert_eq!(
        engine.get("c", "k99").unwrap().value.as_i64(),
        Some(99),
        "the last committed write must be durable"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Helper test executed in a child process by `crash_recovery_keeps_committed_writes`.
#[test]
#[ignore = "spawned explicitly by the crash recovery test"]
fn crash_child_writer() {
    let dir = match std::env::var("PREPPYDB_CRASH_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => return, // running in a normal `cargo test -- --ignored` sweep
    };
    let mut cfg = EngineConfig::new(&dir);
    cfg.sync_mode = SyncMode::Always;
    let engine = Engine::open(cfg).unwrap();
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    for i in 0..100 {
        engine
            .put("c", &format!("k{i}"), Json::int(i), PutOptions::default())
            .unwrap();
    }
    // Simulate a hard crash: no Drop, no flush, no lock release.
    std::process::abort();
}

#[test]
fn torn_tail_is_truncated_and_reported() {
    let dir = tempdir("torn");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "a", Json::int(1), PutOptions::default())
            .unwrap();
        engine.checkpoint().unwrap();
        engine
            .put("c", "b", Json::int(2), PutOptions::default())
            .unwrap();
    }
    // Append a partial frame to the active segment, as a crash mid-write would.
    let segments = wal::list_segments(&dir.join("wal")).unwrap();
    let last = segments.last().unwrap();
    let mut bytes = std::fs::read(last).unwrap();
    bytes.extend_from_slice(&[200, 0, 0, 0, 1, 2, 3]);
    std::fs::write(last, &bytes).unwrap();

    let torn_len = std::fs::metadata(last).unwrap().len();
    let mut read_only = EngineConfig::new(&dir);
    read_only.read_only = true;
    read_only.take_lock = false;
    let read_only_engine = Engine::open(read_only).unwrap();
    assert_eq!(std::fs::metadata(last).unwrap().len(), torn_len);
    let verification = read_only_engine.verify().unwrap();
    assert!(!verification.ok);
    assert!(verification
        .issues
        .iter()
        .any(|issue| issue.contains("torn tail")));
    drop(read_only_engine);

    let engine = open(&dir);
    assert_eq!(engine.count("c").unwrap(), 2);
    let notes = engine.recovery_notes();
    assert!(
        notes.iter().any(|note| note.kind == "torn_tail"),
        "expected a torn_tail note, got {notes:?}"
    );
    // The report file is written next to the data.
    let reports: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("recovery-report-")
        })
        .collect();
    assert!(!reports.is_empty(), "recovery must leave a report behind");
    drop(engine);
    // A second open must be clean now.
    let engine = open(&dir);
    assert!(engine.recovery_notes().is_empty());
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mid_log_corruption_is_rejected_by_default() {
    let dir = tempdir("corrupt");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        for i in 0..5 {
            engine
                .put("c", &format!("k{i}"), Json::int(i), PutOptions::default())
                .unwrap();
        }
    }
    let segments = wal::list_segments(&dir.join("wal")).unwrap();
    let segment = segments.last().unwrap();
    let mut bytes = std::fs::read(segment).unwrap();
    // Flip a byte inside the first record payload (after the 12 byte header + 8 byte frame
    // header).
    bytes[24] ^= 0xff;
    std::fs::write(segment, &bytes).unwrap();

    let mut cfg = EngineConfig::new(&dir);
    cfg.repair = RepairPolicy::Strict;
    let err = Engine::open(cfg).unwrap_err();
    assert!(
        matches!(err, DbError::Corrupt { .. }),
        "expected corruption error, got {err}"
    );

    // Explicit repair truncates at the damaged record instead of failing to open.
    let mut cfg = EngineConfig::new(&dir);
    cfg.repair = RepairPolicy::TruncateCorruptTail;
    let engine = Engine::open(cfg).unwrap();
    let notes = engine.recovery_notes();
    assert!(
        notes.iter().any(|note| note.kind == "corruption_truncated"),
        "expected corruption note, got {notes:?}"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn terminal_complete_crc_corruption_requires_explicit_repair() {
    let dir = tempdir("corrupt-terminal-frame");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "kept", Json::int(1), PutOptions::default())
            .unwrap();
        engine
            .put("c", "damaged", Json::int(2), PutOptions::default())
            .unwrap();
    }

    let segment = wal::list_segments(&dir.join("wal")).unwrap().pop().unwrap();
    let mut bytes = std::fs::read(&segment).unwrap();
    *bytes.last_mut().unwrap() ^= 0x80;
    std::fs::write(&segment, bytes).unwrap();

    assert!(matches!(
        Engine::open(EngineConfig::new(&dir)),
        Err(DbError::Corrupt { .. })
    ));
    let mut repair = EngineConfig::new(&dir);
    repair.repair = RepairPolicy::TruncateCorruptTail;
    let engine = Engine::open(repair).unwrap();
    assert_eq!(engine.count("c").unwrap(), 1);
    assert_eq!(engine.get("c", "kept").unwrap().value.as_i64(), Some(1));
    assert!(engine.get("c", "damaged").is_err());
    engine
        .put("c", "after-repair", Json::int(3), PutOptions::default())
        .unwrap();
    drop(engine);

    let reopened = open(&dir);
    assert_eq!(reopened.count("c").unwrap(), 2);
    assert_eq!(
        reopened.get("c", "after-repair").unwrap().value.as_i64(),
        Some(3)
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sequence_gap_after_checkpoint_fails_closed_and_repair_drops_suffix() {
    let dir = tempdir("wal-sequence-gap");
    let mut cfg = EngineConfig::new(&dir);
    cfg.sync_mode = SyncMode::Always;
    cfg.max_segment_bytes = wal::WAL_HEADER_LEN + 1;
    {
        let engine = Engine::open(cfg.clone()).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "baseline", Json::int(0), PutOptions::default())
            .unwrap();
        engine.checkpoint().unwrap();
        for i in 0..4 {
            engine
                .put(
                    "c",
                    &format!("after{i}"),
                    Json::int(i),
                    PutOptions::default(),
                )
                .unwrap();
        }
    }

    let segments = wal::list_segments(&dir.join("wal")).unwrap();
    assert!(
        segments.len() >= 4,
        "expected rotated segments: {segments:?}"
    );
    let inspector = Engine::open(cfg.clone()).unwrap();
    std::fs::remove_file(&segments[1]).unwrap();

    let verification = inspector.verify().unwrap();
    assert!(!verification.ok);
    assert!(verification
        .issues
        .iter()
        .any(|issue| issue.contains("does not continue from")));
    drop(inspector);

    assert!(matches!(
        Engine::open(cfg.clone()),
        Err(DbError::Corrupt { .. })
    ));

    cfg.repair = RepairPolicy::TruncateCorruptTail;
    let engine = Engine::open(cfg.clone()).unwrap();
    assert!(engine
        .recovery_notes()
        .iter()
        .any(|note| note.kind == "corruption_truncated"));
    assert_eq!(engine.count("c").unwrap(), 2);
    assert_eq!(engine.get("c", "after0").unwrap().value.as_i64(), Some(0));
    assert!(engine.get("c", "after1").is_err());
    engine
        .put("c", "after-repair", Json::int(9), PutOptions::default())
        .unwrap();
    drop(engine);

    cfg.repair = RepairPolicy::Strict;
    let reopened = Engine::open(cfg).unwrap();
    assert_eq!(reopened.count("c").unwrap(), 3);
    assert_eq!(
        reopened.get("c", "after-repair").unwrap().value.as_i64(),
        Some(9)
    );
    assert!(reopened.get("c", "after2").is_err());
    assert!(reopened.get("c", "after3").is_err());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn verify_and_recovery_reject_torn_wal_segment_followed_by_later_segment() {
    let dir = tempdir("torn-wal-before-later-segment");
    let mut cfg = EngineConfig::new(&dir);
    cfg.sync_mode = SyncMode::Always;
    cfg.max_segment_bytes = wal::WAL_HEADER_LEN + 1;
    {
        let engine = Engine::open(cfg.clone()).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine.checkpoint().unwrap();
        for index in 0..4 {
            engine
                .put(
                    "c",
                    &format!("key-{index}"),
                    Json::int(index),
                    PutOptions::default(),
                )
                .unwrap();
        }
    }

    let segments = wal::list_segments(&dir.join("wal")).unwrap();
    assert!(
        segments.len() >= 2,
        "expected rotated segments: {segments:?}"
    );
    let inspector = Engine::open(cfg.clone()).unwrap();
    let mut first = std::fs::OpenOptions::new()
        .append(true)
        .open(&segments[0])
        .unwrap();
    use std::io::Write;
    first.write_all(&[1, 2, 3]).unwrap();
    first.sync_all().unwrap();

    let report = inspector.verify().unwrap();
    assert!(!report.ok);
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.contains("torn WAL tail is followed by another segment")));
    drop(inspector);

    assert!(matches!(Engine::open(cfg), Err(DbError::Corrupt { .. })));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_rejects_semantically_unreplayable_but_checksummed_wal_record() {
    let dir = tempdir("verify-semantic-wal-corruption");
    let cfg = EngineConfig::new(&dir);
    let engine = Engine::open(cfg.clone()).unwrap();
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    for key in ["a", "b"] {
        engine
            .put(
                "c",
                key,
                Json::obj().with("value", Json::int(7)),
                PutOptions::default(),
            )
            .unwrap();
    }

    let wal_dir = dir.join("wal");
    let segments = wal::list_segments(&wal_dir).unwrap();
    let current = segments.last().unwrap();
    let index = current
        .file_name()
        .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
        .unwrap();
    let mut writer = wal::WalWriter::open(&wal_dir, index, 1024 * 1024, SyncMode::Always).unwrap();
    writer
        .append(&Record::new(
            engine.last_seq() + 1,
            0,
            Op::CreateIndex {
                collection: "c".into(),
                index: IndexMeta {
                    name: "unique_value".into(),
                    field: "value".into(),
                    unique: true,
                    sparse: false,
                    created_at: 0,
                },
            },
        ))
        .unwrap();
    drop(writer);

    let report = engine.verify().unwrap();
    assert!(
        !report.ok,
        "verify must reject a WAL record that recovery cannot apply"
    );
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.contains("unique index")));
    drop(engine);

    assert!(
        Engine::open(cfg).is_err(),
        "reopen must reject the same semantically invalid WAL suffix"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn interrupted_final_wal_segment_header_preserves_prior_commits() {
    let dir = tempdir("torn-final-wal-header");
    let cfg = EngineConfig::new(&dir);
    let wal_dir = dir.join("wal");
    {
        let engine = Engine::open(cfg.clone()).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put(
                "c",
                "committed",
                Json::str("before-rotation"),
                PutOptions::default(),
            )
            .unwrap();
        let segments = wal::list_segments(&wal_dir).unwrap();
        let current = segments
            .last()
            .and_then(|path| path.file_name())
            .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
            .unwrap();
        let _incomplete_rotation = wal::WalWriter::open(
            &wal_dir,
            current.checked_add(1).unwrap(),
            1024 * 1024,
            SyncMode::Always,
        )
        .unwrap();
    }
    let segments = wal::list_segments(&wal_dir).unwrap();
    let final_segment = segments.last().unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(final_segment)
        .unwrap();
    file.set_len(wal::WAL_HEADER_LEN - 3).unwrap();
    file.sync_all().unwrap();

    let mut read_only_cfg = cfg.clone();
    read_only_cfg.read_only = true;
    assert!(matches!(
        Engine::open(read_only_cfg),
        Err(DbError::Corrupt { .. })
    ));
    assert_eq!(
        std::fs::metadata(final_segment).unwrap().len(),
        wal::WAL_HEADER_LEN - 3
    );

    let recovered = Engine::open(cfg.clone()).unwrap();
    assert_eq!(
        recovered.get("c", "committed").unwrap().value.as_str(),
        Some("before-rotation")
    );
    assert!(recovered
        .recovery_notes()
        .iter()
        .any(|note| note.kind == "torn_segment_header"));
    assert_eq!(
        wal::list_segments(&wal_dir).unwrap().last(),
        segments.get(segments.len() - 2)
    );
    recovered
        .put(
            "c",
            "after-recovery",
            Json::Bool(true),
            PutOptions::default(),
        )
        .unwrap();
    drop(recovered);

    let reopened = Engine::open(cfg).unwrap();
    assert_eq!(
        reopened.get("c", "committed").unwrap().value.as_str(),
        Some("before-rotation")
    );
    assert_eq!(
        reopened.get("c", "after-recovery").unwrap().value.as_bool(),
        Some(true)
    );
    drop(reopened);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn interrupted_initial_wal_header_reinitializes_only_empty_database() {
    let dir = tempdir("torn-initial-wal-header");
    let wal_dir = dir.join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let mut segment = wal::WalWriter::open(&wal_dir, 1, 1024 * 1024, SyncMode::Always).unwrap();
    segment.truncate_current(4).unwrap();
    drop(segment);

    let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
    assert_eq!(engine.stats().total_documents, 0);
    assert!(engine
        .recovery_notes()
        .iter()
        .any(|note| note.kind == "torn_segment_header"));
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "first", Json::uint(1), PutOptions::default())
        .unwrap();
    drop(engine);

    let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
    assert_eq!(reopened.get("c", "first").unwrap().value.as_u64(), Some(1));
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn complete_invalid_final_wal_header_fails_closed() {
    let dir = tempdir("invalid-final-wal-header");
    let cfg = EngineConfig::new(&dir);
    let wal_dir = dir.join("wal");
    {
        let engine = Engine::open(cfg.clone()).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "committed", Json::Bool(true), PutOptions::default())
            .unwrap();
        let segments = wal::list_segments(&wal_dir).unwrap();
        let current = segments
            .last()
            .and_then(|path| path.file_name())
            .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
            .unwrap();
        let _empty_next = wal::WalWriter::open(
            &wal_dir,
            current.checked_add(1).unwrap(),
            1024 * 1024,
            SyncMode::Always,
        )
        .unwrap();
    }
    let segments = wal::list_segments(&wal_dir).unwrap();
    let final_segment = segments.last().unwrap();
    let mut bytes = std::fs::read(final_segment).unwrap();
    assert_eq!(bytes.len(), wal::WAL_HEADER_LEN as usize);
    bytes[0] ^= 1;
    std::fs::write(final_segment, &bytes).unwrap();

    assert!(
        Engine::open(cfg).is_err(),
        "reopen must reject the same semantically invalid WAL suffix"
    );
    assert_eq!(
        std::fs::metadata(final_segment).unwrap().len(),
        wal::WAL_HEADER_LEN
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn incomplete_nonfinal_wal_header_fails_closed() {
    let dir = tempdir("torn-nonfinal-wal-header");
    let cfg = EngineConfig::new(&dir);
    let wal_dir = dir.join("wal");
    {
        let engine = Engine::open(cfg.clone()).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "committed", Json::Bool(true), PutOptions::default())
            .unwrap();
        let segments = wal::list_segments(&wal_dir).unwrap();
        let current = segments
            .last()
            .and_then(|path| path.file_name())
            .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
            .unwrap();
        let _middle = wal::WalWriter::open(
            &wal_dir,
            current.checked_add(1).unwrap(),
            1024 * 1024,
            SyncMode::Always,
        )
        .unwrap();
        let _later = wal::WalWriter::open(
            &wal_dir,
            current.checked_add(2).unwrap(),
            1024 * 1024,
            SyncMode::Always,
        )
        .unwrap();
    }
    let segments = wal::list_segments(&wal_dir).unwrap();
    assert!(segments.len() >= 3);
    let middle = &segments[segments.len() - 2];
    std::fs::OpenOptions::new()
        .write(true)
        .open(middle)
        .unwrap()
        .set_len(wal::WAL_HEADER_LEN - 1)
        .unwrap();

    assert!(matches!(Engine::open(cfg), Err(DbError::Corrupt { .. })));
    assert_eq!(
        std::fs::metadata(middle).unwrap().len(),
        wal::WAL_HEADER_LEN - 1
    );
    assert!(segments.last().unwrap().exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn backup_and_restore_roundtrip() {
    let dir = tempdir("backup-src");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("people", 0))
        .unwrap();
    for i in 0..20 {
        engine
            .put(
                "people",
                &format!("p{i}"),
                doc(&format!("P{i}")),
                PutOptions::default(),
            )
            .unwrap();
    }
    let archive = dir.parent().unwrap().join(format!(
        "{}-backup.bak",
        dir.file_name().unwrap().to_string_lossy()
    ));
    let report = backup::create_backup(&engine, &archive, true).unwrap();
    assert!(
        report.entries >= 2,
        "expected manifest + snapshot, got {report:?}"
    );
    assert!(archive.exists());
    assert_eq!(
        backup::archive_sha256(&archive).unwrap(),
        report.archive_sha256,
        "archive hash must match the reported one"
    );
    let verified = backup::verify_backup(&archive).unwrap();
    assert_eq!(verified.get("ok").unwrap().as_bool(), Some(true));
    drop(engine);

    let target = tempdir("backup-dst").join("restored");
    let restored = backup::restore_backup(&archive, &target, false).unwrap();
    assert_eq!(restored.entries, report.entries);
    let engine = open(&target);
    assert_eq!(engine.count("people").unwrap(), 20);
    assert_eq!(
        engine.get("people", "p7").unwrap().value.get_str("name"),
        Some("P7")
    );
    drop(engine);

    // Restoring again over a non-empty target must refuse without force.
    let err = backup::restore_backup(&archive, &target, false).unwrap_err();
    assert!(err.to_string().contains("not empty"), "{err}");

    let engine = open(&target);
    engine
        .put(
            "people",
            "after-backup",
            doc("Later"),
            PutOptions::default(),
        )
        .unwrap();
    drop(engine);
    let forced = backup::restore_backup(&archive, &target, true).unwrap();
    let previous = std::path::PathBuf::from(forced.replaced_path.unwrap());
    let old_engine = open(&previous);
    assert_eq!(old_engine.count("people").unwrap(), 21);
    drop(old_engine);
    let restored_engine = open(&target);
    assert_eq!(restored_engine.count("people").unwrap(), 20);
    drop(restored_engine);
    std::fs::remove_file(&archive).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
}

#[test]
fn restore_refuses_to_replace_a_database_with_an_active_writer() {
    let root = tempdir("restore-active-writer");
    let target = root.join("database");
    let engine = Engine::open(EngineConfig::new(&target)).unwrap();
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "kept", Json::int(1), PutOptions::default())
        .unwrap();
    let archive = root.join("backup.bak");
    backup::create_backup(&engine, &archive, true).unwrap();

    let error = backup::restore_backup(&archive, &target, true).unwrap_err();
    assert!(
        matches!(error, DbError::Locked(_)),
        "unexpected error: {error}"
    );
    assert_eq!(engine.get("c", "kept").unwrap().value.as_i64(), Some(1));
    assert!(!target.with_file_name(".database.restore-intent").exists());

    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn corrupted_archive_is_rejected_before_writing() {
    let dir = tempdir("badbackup-src");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "k", Json::str("v"), PutOptions::default())
        .unwrap();
    let archive = dir.parent().unwrap().join(format!(
        "{}-bad.bak",
        dir.file_name().unwrap().to_string_lossy()
    ));
    backup::create_backup(&engine, &archive, true).unwrap();
    drop(engine);

    let mut bytes = std::fs::read(&archive).unwrap();
    let index = bytes.len() - 100;
    bytes[index] ^= 0x5a;
    std::fs::write(&archive, &bytes).unwrap();

    let target = tempdir("badbackup-dst").join("restored");
    let err = backup::restore_backup(&archive, &target, false).unwrap_err();
    assert!(
        matches!(err, DbError::Corrupt { .. }),
        "expected corruption, got {err}"
    );
    assert!(
        !target.exists(),
        "nothing may be written when the archive is corrupt"
    );
    std::fs::remove_file(&archive).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
}

#[test]
fn compaction_retires_log_segments() {
    let dir = tempdir("compact");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    for i in 0..100 {
        engine
            .put("c", &format!("k{i}"), Json::int(i), PutOptions::default())
            .unwrap();
    }
    for i in 0..50 {
        engine.delete("c", &format!("k{i}"), None).unwrap();
    }
    let before = engine.stats();
    assert!(before.wal_bytes > 0);
    let report = engine.compact().unwrap();
    assert_eq!(report.snapshot.header.doc_count, 50);
    let after = engine.stats();
    assert!(
        after.wal_bytes < before.wal_bytes,
        "compaction should shrink the log ({} -> {})",
        before.wal_bytes,
        after.wal_bytes
    );
    assert_eq!(engine.count("c").unwrap(), 50);
    assert_eq!(report.segments_removed + 1, before.wal_segments.max(1));
    drop(engine);
    let engine = open(&dir);
    assert_eq!(engine.count("c").unwrap(), 50);
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_only_rejects_writes() {
    let dir = tempdir("readonly");
    {
        let engine = open(&dir);
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "k", Json::str("v"), PutOptions::default())
            .unwrap();
    }
    let mut cfg = EngineConfig::new(&dir);
    cfg.read_only = true;
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(engine.get("c", "k").unwrap().value.as_str(), Some("v"));
    let err = engine
        .put("c", "k", Json::str("w"), PutOptions::default())
        .unwrap_err();
    assert!(matches!(err, DbError::ReadOnly(_)));
    assert!(engine.checkpoint().is_err());
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn second_writer_is_locked_out() {
    let dir = tempdir("lock");
    let engine = open(&dir);
    let err = Engine::open(EngineConfig::new(&dir)).unwrap_err();
    assert!(
        matches!(err, DbError::Locked(_)),
        "expected Locked, got {err}"
    );
    drop(engine);
    // After a clean shutdown the directory can be opened again.
    let engine = open(&dir);
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_reports_healthy_database() {
    let dir = tempdir("verify");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put("c", "k", Json::str("v"), PutOptions::default())
        .unwrap();
    engine.checkpoint().unwrap();
    engine
        .put("c", "k2", Json::str("v2"), PutOptions::default())
        .unwrap();
    let report = engine.verify().unwrap();
    assert!(report.ok, "issues: {:?}", report.issues);
    assert!(!report.segments.is_empty());
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scan_orders_and_paginates() {
    let dir = tempdir("scan");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    for key in ["b", "a", "d", "c"] {
        engine
            .put("c", key, Json::str(key), PutOptions::default())
            .unwrap();
    }
    let all = engine
        .scan(
            "c",
            &ScanOptions {
                limit: 0,
                ..Default::default()
            },
        )
        .unwrap();
    let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["a", "b", "c", "d"]);
    let page = engine
        .scan(
            "c",
            &ScanOptions {
                limit: 2,
                offset: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].0, "b");
    let reversed = engine
        .scan(
            "c",
            &ScanOptions {
                limit: 0,
                reverse: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(reversed[0].0, "d");
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn compare_and_set_and_unique_index() {
    let dir = tempdir("cas");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("people", 0))
        .unwrap();
    engine
        .create_index(
            "people",
            IndexMeta {
                name: "email".into(),
                field: "email".into(),
                unique: true,
                sparse: true,
                created_at: 0,
            },
        )
        .unwrap();
    let version = engine
        .put(
            "people",
            "a",
            Json::obj().with("email", Json::str("a@example.org")),
            PutOptions {
                expected_version: Some(0),
                expires_at: None,
            },
        )
        .unwrap();
    assert_eq!(version, 1);
    // insert-only fails for an existing key
    let err = engine
        .put(
            "people",
            "a",
            Json::obj().with("email", Json::str("a@example.org")),
            PutOptions {
                expected_version: Some(0),
                expires_at: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }));
    // unique index violation
    let err = engine
        .put(
            "people",
            "b",
            Json::obj().with("email", Json::str("a@example.org")),
            PutOptions::default(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("unique"));
    assert!(engine.get_opt("people", "b").unwrap().is_none());
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn snapshot_export_is_self_consistent() {
    let dir = tempdir("export");
    let engine = open(&dir);
    engine
        .create_collection(CollectionMeta::new("c", 0))
        .unwrap();
    engine
        .put(
            "c",
            "k",
            Json::obj().with("v", Json::int(1)),
            PutOptions::default(),
        )
        .unwrap();
    let snapshot = engine.export_snapshot();
    assert_eq!(snapshot.get_i64("revision"), Some(engine.last_seq() as i64));
    let value = snapshot
        .get_path("collections.c.documents.k.value.v")
        .unwrap();
    assert_eq!(value.as_i64(), Some(1));
    drop(engine);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_mode_replay_ignores_missing_collections() {
    // Guards the recovery path against log records for already-dropped collections.
    let mut store = preppydb_storage::store::Store::new(Default::default());
    let op = Op::Put {
        collection: "ghost".into(),
        key: "k".into(),
        value: Json::int(1),
        expires_at: None,
    };
    store.apply(&op, 0, 1, ApplyMode::Replay).unwrap();
    assert_eq!(store.total_docs(), 0);
}
