//! Explicit, opt-in Ruby planning bridge. This module does not execute queries.
//!
//! Only the bundled planner source is run; query input is JSON on stdin, never code.
//! An executable path must be configured by the trusted application. This is a
//! process boundary, not an operating-system security sandbox.

use crate::query_plan::{validate_plan, ValidatedPlan};
use preppydb_json::Json;
use preppydb_storage::catalog::CollectionMeta;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 65_536;
const SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../engines/ruby/preppydb_planner.rb"
));

/// Run the bundled planner and validate its output. Call outside storage locks and
/// revalidate against live metadata when executing the returned access path.
pub fn plan_with_ruby(
    ruby: &Path,
    query: &Json,
    metadata: &CollectionMeta,
    timeout: Duration,
) -> Result<ValidatedPlan, String> {
    if query.as_object().is_none() || query.to_string().len() > MAX_BYTES {
        return Err("invalid or oversized query".into());
    }
    let mut input = query.clone();
    let indexes = metadata
        .indexes
        .values()
        .map(|index| {
            Json::obj()
                .with("name", Json::str(index.name.clone()))
                .with("field", Json::str(index.field.clone()))
                .with("sparse", Json::Bool(index.sparse))
        })
        .collect();
    input.set("indexes", Json::Array(indexes));
    let input = input.to_string().into_bytes();
    if input.len() > MAX_BYTES || timeout.is_zero() || timeout > Duration::from_secs(30) {
        return Err("planner input or timeout exceeds limits".into());
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("invalid timeout")?;
    let mut child = Command::new(ruby)
        .arg("--disable-gems")
        .arg("-e")
        .arg(SOURCE)
        .env_remove("RUBYOPT")
        .env_remove("RUBYLIB")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cannot start Ruby planner".to_string())?;
    let mut stdin = child.stdin.take().ok_or("missing planner stdin")?;
    let stdout = child.stdout.take().ok_or("missing planner stdout")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => break Err("Ruby planner timed out"),
            Err(_) => break Err("Ruby planner wait failed"),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let written = writer.join().map_err(|_| "planner input thread failed")?;
    let output = reader
        .join()
        .map_err(|_| "planner output thread failed")?
        .map_err(|_| "planner output read failed")?;
    let status = result?;
    if !status.success() || written.is_err() || output.len() > MAX_BYTES {
        return Err("Ruby planner failed or exceeded output limit".into());
    }
    let output = std::str::from_utf8(&output).map_err(|_| "planner output is not UTF-8")?;
    validate_plan(output, query, metadata).map_err(str::to_string)
}
