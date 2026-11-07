//! Local smoke benchmark for ordinary single-document commit cost.
//!
//! Run with `cargo run --release -p preppydb-storage --example commit_scaling`.
//! Results are diagnostic only and are not portable performance claims.

use preppydb_json::Json;
use preppydb_storage::catalog::CollectionMeta;
use preppydb_storage::engine::{Engine, EngineConfig, PutOptions};
use preppydb_storage::wal::SyncMode;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const STORE_SIZES: &[usize] = &[1_000, 10_000, 50_000];
const SAMPLES: usize = 101;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for &document_count in STORE_SIZES {
        let path = temporary_path(document_count);
        let mut config = EngineConfig::new(&path);
        config.sync_mode = SyncMode::Never;
        config.checkpoint_every_mutations = None;
        let engine = Engine::open(config)?;
        engine.create_collection(CollectionMeta::new("items", 0))?;

        for index in 0..document_count {
            engine.put(
                "items",
                &format!("item-{index:06}"),
                Json::obj().with("value", Json::int(index as i64)),
                PutOptions::default(),
            )?;
        }
        engine.put(
            "items",
            "measured",
            Json::obj().with("value", Json::int(0)),
            PutOptions::default(),
        )?;

        let mut samples = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let start = Instant::now();
            engine.put(
                "items",
                "measured",
                Json::obj().with("value", Json::int(sample as i64 + 1)),
                PutOptions::default(),
            )?;
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        println!(
            "documents={document_count} measured_puts={SAMPLES} median_ns={} p95_ns={}",
            samples[SAMPLES / 2],
            samples[SAMPLES * 95 / 100]
        );

        drop(engine);
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn temporary_path(document_count: usize) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "preppydb-commit-scaling-{}-{document_count}-{nonce}",
        std::process::id()
    ))
}
