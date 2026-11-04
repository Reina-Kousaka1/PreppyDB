//! End-to-end tests for the storage engine: persistence, recovery, transactions,
//! backups and maintenance. Run with `cargo test -p preppydb-storage`.

use preppydb_json::Json;
use preppydb_storage::backup;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::{Engine, EngineConfig, PutOptions, RepairPolicy, ScanOptions};
use preppydb_storage::error::DbError;
use preppydb_storage::record::Op;
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
    std::fs::remove_file(&archive).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
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
