#![cfg(feature = "ruby-integration-tests")]

use preppydb_json::Json;
use preppydb_server::query_plan::execute_plan;
use preppydb_server::query_plan::AccessPath;
use preppydb_server::ruby_planner::plan_with_ruby;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::{Engine, EngineConfig, PutOptions};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
#[test]
fn planner_timeout_and_excess_output_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "preppydb-ruby-fault-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let executable = dir.join("fake-ruby");
    let query = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("users"));
    let metadata = CollectionMeta::new("users", 0);
    for script in [
        "#!/bin/sh\nwhile :; do :; done\n",
        "#!/bin/sh\nwhile :; do printf '0123456789'; done\n",
    ] {
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let start = std::time::Instant::now();
        assert!(
            plan_with_ruby(&executable, &query, &metadata, Duration::from_millis(100)).is_err()
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "planner did not terminate after deadline"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn actual_ruby_plans_are_validated_against_rust_metadata() {
    let mut metadata = CollectionMeta::new("users", 0);
    metadata.indexes.insert(
        "names".into(),
        IndexMeta {
            name: "names".into(),
            field: "name".into(),
            unique: false,
            sparse: true,
            created_at: 0,
        },
    );
    let query = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("users"))
        .with(
            "filter",
            Json::obj()
                .with("field", Json::str("name"))
                .with("op", Json::str("eq"))
                .with("value", Json::str("A")),
        )
        .with("limit", Json::int(10));
    let plan =
        plan_with_ruby(Path::new("ruby"), &query, &metadata, Duration::from_secs(5)).unwrap();
    assert_eq!(plan.access, AccessPath::IndexEquality("names".into()));
    metadata.indexes.clear();
    let plan =
        plan_with_ruby(Path::new("ruby"), &query, &metadata, Duration::from_secs(5)).unwrap();
    assert_eq!(plan.access, AccessPath::FullScan);
    assert!(plan_with_ruby(Path::new("ruby"), &query, &metadata, Duration::ZERO).is_err());
    assert!(plan_with_ruby(
        Path::new("preppydb-nonexistent-ruby-runtime"),
        &query,
        &metadata,
        Duration::from_secs(1)
    )
    .is_err());
    let wrong_collection = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("other"));
    assert!(plan_with_ruby(
        Path::new("ruby"),
        &wrong_collection,
        &metadata,
        Duration::from_secs(5)
    )
    .is_err());
}

#[test]
fn ruby_plans_execute_with_index_recheck_and_detect_metadata_drift() {
    let dir = std::env::temp_dir().join(format!(
        "preppydb-ruby-query-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
    engine
        .create_collection(CollectionMeta::new("users", 0))
        .unwrap();
    engine
        .create_index(
            "users",
            IndexMeta {
                name: "names".into(),
                field: "name".into(),
                unique: false,
                sparse: true,
                created_at: 0,
            },
        )
        .unwrap();
    for (key, value) in [
        ("a", Json::obj().with("name", Json::str("A"))),
        ("b", Json::obj().with("name", Json::str("B"))),
        ("missing", Json::obj()),
    ] {
        engine
            .put("users", key, value, PutOptions::default())
            .unwrap();
    }
    engine
        .put(
            "users",
            "null",
            Json::obj().with("name", Json::Null),
            PutOptions::default(),
        )
        .unwrap();
    let query = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("users"))
        .with(
            "filter",
            Json::obj()
                .with("field", Json::str("name"))
                .with("op", Json::str("eq"))
                .with("value", Json::str("A")),
        )
        .with("limit", Json::int(10));
    let metadata = engine.collection_meta("users").unwrap();
    let indexed =
        plan_with_ruby(Path::new("ruby"), &query, &metadata, Duration::from_secs(5)).unwrap();
    let results = execute_plan(&engine, &indexed, &query).unwrap();
    assert_eq!(
        results
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    let mut scan_meta = metadata.clone();
    scan_meta.indexes.clear();
    let scan = plan_with_ruby(
        Path::new("ruby"),
        &query,
        &scan_meta,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(execute_plan(&engine, &scan, &query).unwrap(), results);
    let null_query = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("users"))
        .with(
            "filter",
            Json::obj()
                .with("field", Json::str("name"))
                .with("op", Json::str("eq"))
                .with("value", Json::Null),
        )
        .with("limit", Json::int(10));
    let null_plan = plan_with_ruby(
        Path::new("ruby"),
        &null_query,
        &metadata,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(null_plan.access, AccessPath::FullScan);
    assert_eq!(
        execute_plan(&engine, &null_plan, &null_query)
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        vec!["null"]
    );
    let changed = Json::obj()
        .with("version", Json::int(1))
        .with("collection", Json::str("users"));
    assert!(execute_plan(&engine, &indexed, &changed).is_err());
    engine.drop_index("users", "names").unwrap();
    assert!(execute_plan(&engine, &indexed, &query).is_err());
    drop(engine);
    std::fs::remove_dir_all(dir).unwrap();
}
