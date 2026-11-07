//! HTTP API integration tests: authentication, authorisation, document CRUD,
//! transactions, limits, pricing and administrative endpoints.
//!
//! Each test starts a real server on an ephemeral loopback port and talks to it over
//! TCP — no mocks, no in-process shortcuts.

use preppydb_client::Client;
use preppydb_json::Json;
use preppydb_pricing::AuditLog;
use preppydb_server::api::ApiState;
use preppydb_server::auth::{AuthStore, Role};
use preppydb_server::config::ServerConfig;
use preppydb_server::http::{HttpLimits, ServerCounters};
use preppydb_storage::engine::{Engine, EngineConfig};
use preppydb_storage::wal::SyncMode;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct TestServer {
    base_url: String,
    admin: String,
    writer: String,
    reader: String,
    dir: PathBuf,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the accept loop so it observes the shutdown flag.
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tempdir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "preppydb-api-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Admin client for schema/DDL steps.
fn admin_client(server: &TestServer) -> Client {
    Client::new(&server.base_url, &server.admin).unwrap()
}

#[cfg(feature = "ruby-integration-tests")]
fn test_ruby_path() -> PathBuf {
    if let Some(path) = std::env::var_os("PREPPYDB_TEST_RUBY") {
        let path = PathBuf::from(path);
        assert!(path.is_absolute(), "PREPPYDB_TEST_RUBY must be absolute");
        return path;
    }

    let executable_names: &[&str] = if cfg!(windows) {
        &["ruby.exe", "ruby"]
    } else {
        &["ruby"]
    };
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .flat_map(|directory| {
            executable_names
                .iter()
                .map(move |name| directory.join(name))
        })
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Ruby executable not found on PATH"))
}

fn start<F>(tag: &str, tweak: F) -> TestServer
where
    F: FnOnce(&mut ServerConfig),
{
    let dir = tempdir(tag);
    let mut config = ServerConfig::new(&dir);
    config.sync_mode = SyncMode::Always;
    config.ttl_sweep_interval_ms = None;
    config.workers = 4;
    tweak(&mut config);
    assert!(config.validate().is_empty(), "{:?}", config.validate());

    let mut auth = AuthStore::open(config.auth_file()).unwrap();
    let (admin, _) = auth.create_token("admin", vec![Role::Admin], None).unwrap();
    let (writer, _) = auth
        .create_token("writer", vec![Role::Write], None)
        .unwrap();
    let (reader, _) = auth.create_token("reader", vec![Role::Read], None).unwrap();
    let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
    let counters = Arc::new(ServerCounters::default());
    let state = Arc::new(ApiState::new(engine, config.clone(), auth, counters));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let limits = HttpLimits {
        max_body_bytes: config.max_body_bytes,
        workers: 4,
        ..Default::default()
    };
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || {
        let _ = preppydb_server::http::serve(
            listener,
            state,
            limits,
            thread_shutdown,
            Arc::new(ServerCounters::default()),
        );
    });
    TestServer {
        base_url: format!("http://{address}"),
        admin,
        writer,
        reader,
        dir,
        shutdown,
        handle: Some(handle),
    }
}

/// Minimal raw HTTP request helper (the client library is exercised separately).
fn raw(
    base_url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String, Json) {
    let address = base_url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let payload = body.unwrap_or("");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    request.push_str(payload);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let parsed = preppydb_json::parse(body).unwrap_or(Json::Null);
    (status, body.to_string(), parsed)
}

#[test]
fn health_and_version_are_public() {
    let server = start("public", |_| {});
    let client = Client::anonymous(&server.base_url).unwrap();
    let health = client.health().unwrap();
    assert_eq!(health.get_str("status"), Some("ok"));
    let version = client.version().unwrap();
    assert_eq!(version.get_str("engine"), Some("preppydb"));
    let (status, _, body) = raw(&server.base_url, "GET", "/readyz", None, None);
    assert_eq!(status, 200);
    assert_eq!(body.get("ready").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn data_endpoints_require_authentication() {
    let server = start("auth", |_| {});
    let (status, _, body) = raw(&server.base_url, "GET", "/v1/stats", None, None);
    assert_eq!(status, 401);
    assert_eq!(
        body.get_path("error.code").and_then(|v| v.as_str()),
        Some("authentication_required")
    );
    let (status, _, body) = raw(
        &server.base_url,
        "GET",
        "/v1/stats",
        Some("not-a-token"),
        None,
    );
    assert_eq!(status, 401);
    assert_eq!(
        body.get_path("error.code").and_then(|v| v.as_str()),
        Some("invalid_token")
    );
    // A read token may read but not write.
    let reader = Client::new(&server.base_url, &server.reader).unwrap();
    assert!(reader.stats().is_ok());
    let err = reader
        .create_collection("nope", None)
        .expect_err("read tokens must not create collections");
    assert_eq!(err.status(), Some(403));
}

#[test]
fn document_lifecycle_and_compare_and_set() {
    let server = start("crud", |_| {});
    let admin = admin_client(&server);
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    admin.create_collection("people", None).unwrap();
    assert_eq!(
        client
            .put_document(
                "people",
                "molly",
                &Json::obj().with("name", Json::str("Molly"))
            )
            .unwrap(),
        1
    );
    let doc = client.get_document("people", "molly").unwrap();
    assert_eq!(doc.get("value").unwrap().get_str("name"), Some("Molly"));
    assert_eq!(doc.get("version").unwrap().as_u64(), Some(1));

    // Compare-and-set with a stale version fails with 409.
    let err = client
        .put_document_checked("people", "molly", &Json::int(2), Some(7))
        .unwrap_err();
    assert_eq!(err.status(), Some(409));
    assert_eq!(err.code(), Some("conflict"));
    // Correct version succeeds and bumps the version.
    assert_eq!(
        client
            .put_document_checked("people", "molly", &Json::int(2), Some(1))
            .unwrap(),
        2
    );
    // Scan sees the document.
    let docs = client.scan("people", None, 10).unwrap();
    assert_eq!(docs.len(), 1);
    // Delete then 404.
    client.delete_document("people", "molly").unwrap();
    let err = client.get_document("people", "molly").unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.code(), Some("not_found"));
}

#[test]
fn schema_violations_are_rejected() {
    let server = start("schema", |_| {});
    let admin = admin_client(&server);
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    let schema = preppydb_json::parse(
        r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"},"age":{"type":"integer","minimum":0}},"additionalProperties":false}"#,
    )
    .unwrap();
    admin.create_collection("strict", Some(schema)).unwrap();
    let err = client
        .put_document("strict", "x", &Json::obj().with("age", Json::int(3)))
        .unwrap_err();
    assert_eq!(err.status(), Some(400));
    assert_eq!(err.code(), Some("validation_failed"));
    let err = client
        .put_document(
            "strict",
            "x",
            &Json::obj()
                .with("name", Json::str("ok"))
                .with("bogus", Json::int(1)),
        )
        .unwrap_err();
    assert_eq!(err.code(), Some("validation_failed"));
    client
        .put_document("strict", "x", &Json::obj().with("name", Json::str("ok")))
        .unwrap();
}

#[test]
fn batch_is_atomic_over_http() {
    let server = start("batch", |_| {});
    let admin = admin_client(&server);
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    admin.create_collection("c", None).unwrap();
    let failing = preppydb_json::parse(
        r#"[
            {"op":"put","key":"a","value":1},
            {"op":"put","key":"b","value":2,"expected_version":9}
        ]"#,
    )
    .unwrap();
    let error = client.batch("c", failing).unwrap_err();
    assert_eq!(error.status(), Some(409));
    assert!(
        client.scan("c", None, 10).unwrap().is_empty(),
        "no partial writes"
    );

    let good = preppydb_json::parse(
        r#"[{"op":"put","key":"a","value":1},{"op":"put","key":"b","value":2}]"#,
    )
    .unwrap();
    let outcome = client.batch("c", good).unwrap();
    assert_eq!(outcome.get("operations").unwrap().as_u64(), Some(2));
    assert_eq!(client.scan("c", None, 10).unwrap().len(), 2);
}

#[test]
fn request_limits_are_enforced() {
    let server = start("limits", |config| {
        config.max_body_bytes = 2048;
        config.rate_limit_per_second = 2;
        config.rate_limit_burst = 2;
    });
    let admin = admin_client(&server);
    admin.create_collection("c", None).unwrap();
    let huge = "x".repeat(4096);
    let payload = format!(r#"{{"key":"k","value":"{huge}"}}"#);
    let (status, _, body) = raw(
        &server.base_url,
        "POST",
        "/v1/collections/c/documents",
        Some(&server.writer),
        Some(&payload),
    );
    assert_eq!(status, 413);
    assert_eq!(
        body.get_path("error.code").and_then(|v| v.as_str()),
        Some("body_too_large")
    );

    // Broken JSON is a 400, not a panic or a 500.
    let (status, _, body) = raw(
        &server.base_url,
        "POST",
        "/v1/collections/c/documents",
        Some(&server.writer),
        Some("{not json"),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body.get_path("error.code").and_then(|v| v.as_str()),
        Some("invalid_json")
    );

    // The rate limiter kicks in after the burst is exhausted.
    let mut limited = 0;
    for _ in 0..12 {
        let (status, _, _) = raw(
            &server.base_url,
            "GET",
            "/v1/stats",
            Some(&server.reader),
            None,
        );
        if status == 429 {
            limited += 1;
        }
    }
    assert!(
        limited > 0,
        "expected some 429 responses from the rate limiter"
    );
}

#[test]
fn anonymous_rate_limit_is_shared_across_tcp_connections_from_same_ip() {
    let server = start("rate-limit-ip", |config| {
        config.rate_limit_per_second = 1;
        config.rate_limit_burst = 1;
    });

    // `raw` opens a fresh TCP connection each time. The listener records only the
    // peer IP, so a new ephemeral client port cannot allocate a fresh anonymous bucket.
    let (first, _, _) = raw(&server.base_url, "GET", "/v1/stats", None, None);
    let (second, _, body) = raw(&server.base_url, "GET", "/v1/stats", None, None);
    assert_eq!(first, 401);
    assert_eq!(second, 429);
    assert_eq!(
        body.get_path("error.code").and_then(|value| value.as_str()),
        Some("rate_limited")
    );
}

#[test]
fn unknown_endpoints_and_methods_are_structured() {
    let server = start("routing", |_| {});
    let (status, _, body) = raw(
        &server.base_url,
        "GET",
        "/v1/nope",
        Some(&server.admin),
        None,
    );
    assert_eq!(status, 404);
    assert_eq!(
        body.get_path("error.code").and_then(|v| v.as_str()),
        Some("unknown_endpoint")
    );
    let (status, _, _) = raw(&server.base_url, "PUT", "/healthz", None, None);
    assert_eq!(status, 404);
}

#[test]
#[cfg(feature = "ruby-integration-tests")]
fn ruby_query_endpoint_requires_read_permission_and_executes_index_plan() {
    let server = start("ruby-query", |config| {
        config.ruby_planner_path = Some(test_ruby_path());
    });
    let admin = admin_client(&server);
    admin.create_collection("users", None).unwrap();
    admin
        .create_index("users", "by_name", "name", false)
        .unwrap();
    admin
        .put_document("users", "a", &Json::obj().with("name", Json::str("A")))
        .unwrap();
    admin
        .put_document("users", "b", &Json::obj().with("name", Json::str("B")))
        .unwrap();
    let query = r#"{"version":1,"filter":{"field":"name","op":"eq","value":"A"},"limit":10}"#;
    assert_eq!(
        raw(
            &server.base_url,
            "POST",
            "/v1/collections/users/query",
            None,
            Some(query)
        )
        .0,
        401
    );
    let (status, _, body) = raw(
        &server.base_url,
        "POST",
        "/v1/collections/users/query",
        Some(&server.reader),
        Some(query),
    );
    assert_eq!(status, 200);
    assert_eq!(body.get_path("data.count").and_then(Json::as_u64), Some(1));
    let (_, _, mismatch) = raw(
        &server.base_url,
        "POST",
        "/v1/collections/users/query",
        Some(&server.reader),
        Some(r#"{"version":1,"collection":"other"}"#),
    );
    assert_eq!(
        mismatch.get_path("error.code").and_then(Json::as_str),
        Some("invalid_query")
    );
}

#[test]
fn credential_corruption_fails_closed_over_http() {
    let server = start("auth-reload-corrupt", |_| {});
    let path = server.dir.join("auth.json");
    let valid = std::fs::read(&path).unwrap();
    assert_eq!(
        raw(
            &server.base_url,
            "GET",
            "/v1/stats",
            Some(&server.admin),
            None
        )
        .0,
        200
    );
    std::fs::write(&path, b"{invalid").unwrap();
    assert_eq!(
        raw(
            &server.base_url,
            "GET",
            "/v1/stats",
            Some(&server.admin),
            None
        )
        .0,
        503
    );
    std::fs::write(&path, valid).unwrap();
    assert_eq!(
        raw(
            &server.base_url,
            "GET",
            "/v1/stats",
            Some(&server.admin),
            None
        )
        .0,
        200
    );
}

#[test]
fn login_flow_and_session_tokens() {
    let server = start("login", |_| {});
    // Add a user to the running server's credential file; the server reloads it.
    let mut auth = AuthStore::open(server.dir.join("auth.json")).unwrap();
    auth.create_user("admin", "correct horse battery", vec![Role::Admin])
        .unwrap();
    drop(auth);
    // Password verification is PBKDF2 with 120k iterations, and the emulated ARM64 job
    // runs it orders of magnitude slower than native hardware, so the default 10 s client
    // timeout is not enough there.
    let slow = std::time::Duration::from_secs(120);
    let mut client = Client::anonymous(&server.base_url)
        .unwrap()
        .with_timeout(slow);
    // The server notices edits to auth.json when it handles a request; on filesystems
    // with whole-second timestamps (and under emulation) that can take a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let outcome = loop {
        match client.login("admin", "correct horse battery") {
            Ok(outcome) => break outcome,
            Err(err) if std::time::Instant::now() < deadline => {
                assert_eq!(err.status(), Some(401), "unexpected login failure: {err:?}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(err) => {
                let path = server.dir.join("auth.json");
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| format!("<{e}>"));
                let meta = std::fs::metadata(&path)
                    .map(|m| format!("len={} modified={:?}", m.len(), m.modified()))
                    .unwrap_or_else(|e| format!("<{e}>"));
                panic!("login did not succeed within 60s: {err:?}; {meta}; auth.json = {text}");
            }
        }
    };
    assert!(outcome.get_str("token").is_some());
    assert!(
        client.stats().is_ok(),
        "session token must work for API calls"
    );
    let mut wrong = Client::anonymous(&server.base_url)
        .unwrap()
        .with_timeout(slow);
    let err = wrong.login("admin", "wrong password!!").unwrap_err();
    assert_eq!(err.status(), Some(401));
    // No secret ever leaks in the login response.
    let text = outcome.to_string();
    assert!(!text.contains("correct horse battery"));
    assert!(!text.contains("hash"));
}

#[test]
fn pricing_flow_is_version_bound_and_immutable() {
    let server = start("pricing", |config| {
        config.pricing_seed = Some("integration-seed".to_string());
    });
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    let rule = preppydb_json::parse(
        r#"{"item_id":"skirt","min_price_cents":2000,"max_price_cents":4000,"anchor_price_cents":2500,"step_cents":100}"#,
    )
    .unwrap();
    let stored = client.put_price_rule(&rule).unwrap();
    assert_eq!(stored.get("version").unwrap().as_u64(), Some(1));

    let tick = client.tick_price("skirt", 5).unwrap();
    let history = tick.get("changes").unwrap().as_array().unwrap();
    assert_eq!(history.len(), 5);
    for change in history {
        let price = change.get("new_cents").unwrap().as_i64().unwrap();
        assert!(
            (2000..=4000).contains(&price),
            "price {price} outside bounds"
        );
    }

    let offer = client.create_offer("skirt", Some(60_000)).unwrap();
    let offer_id = offer.get_str("offer_id").unwrap().to_string();
    let purchase = client.purchase(&offer_id, 2).unwrap();
    let charged = purchase
        .get_path("purchase.price_cents")
        .unwrap()
        .as_i64()
        .unwrap();
    assert!(charged >= 2000);
    // Redeeming twice fails.
    let err = client.purchase(&offer_id, 1).unwrap_err();
    assert_eq!(err.status(), Some(409));

    // Editing the rule bumps its version and makes older offers stale.
    let mut changed = rule.clone();
    changed.set("max_price_cents", Json::int(5000));
    client.put_price_rule(&changed).unwrap();
    let second = client.create_offer("skirt", None).unwrap();
    let second_id = second.get_str("offer_id").unwrap().to_string();
    let mut third = changed.clone();
    third.set("anchor_price_cents", Json::int(2100));
    client.put_price_rule(&third).unwrap();
    let err = client.purchase(&second_id, 1).unwrap_err();
    assert_eq!(err.code(), Some("offer_stale"));

    // The purchase history is append-only and still shows the original amount.
    let purchase_docs = client.scan("purchases", None, 10).unwrap();
    assert_eq!(purchase_docs.len(), 1);
    assert_eq!(
        purchase_docs[0]
            .get("value")
            .unwrap()
            .get("price_cents")
            .unwrap()
            .as_i64(),
        Some(charged)
    );
    let err = client.delete_document("purchases", "whatever").unwrap_err();
    assert_eq!(err.code(), Some("managed_collection"));
    let overwrite = client.put_document(
        "purchases",
        "manual",
        &Json::obj().with("price_cents", Json::int(1)),
    );
    assert_eq!(overwrite.unwrap_err().code(), Some("managed_collection"));

    let admin = admin_client(&server);
    let verified = admin.audit_verify().unwrap();
    assert_eq!(verified.get("ok").and_then(|v| v.as_bool()), Some(true));
    let history_entries = client.price_history("skirt").unwrap();
    assert_eq!(history_entries.len(), 5);
}

#[test]
fn pricing_audit_failure_is_reported_and_disables_further_writes() {
    let server = start("pricing-audit-failure", |_| {});
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    let rule = preppydb_json::parse(r#"{"item_id":"skirt","min_price_cents":2000,"max_price_cents":4000,"anchor_price_cents":2500,"step_cents":100}"#).unwrap();
    client.put_price_rule(&rule).unwrap();
    let path = server.dir.join("pricing-audit.log");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let err = client.put_price_rule(&rule).unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(err.code(), Some("audit_unavailable"));
    let stored = client.get_document("price_rules", "skirt").unwrap();
    assert_eq!(
        stored.get_path("value.version").and_then(|v| v.as_u64()),
        Some(1)
    );
    let err = client.put_price_rule(&rule).unwrap_err();
    assert_eq!(err.code(), Some("audit_unavailable"));
    let stored = client.get_document("price_rules", "skirt").unwrap();
    assert_eq!(
        stored.get_path("value.version").and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn pricing_collections_reject_generic_mutations() {
    let server = start("pricing-managed-collections", |_| {});
    let admin = admin_client(&server);
    assert_eq!(
        admin
            .create_collection("price_rules", None)
            .unwrap_err()
            .code(),
        Some("managed_collection")
    );
    // Accessing the pricing API provisions its own collections.
    admin.pricing_rules().unwrap();
    assert_eq!(
        admin.drop_collection("price_rules").unwrap_err().code(),
        Some("managed_collection")
    );
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    let error = client
        .put_document(
            "price_rules",
            "hat",
            &Json::obj().with("anchor_price_cents", Json::int(100)),
        )
        .unwrap_err();
    assert_eq!(error.status(), Some(403));
    assert_eq!(error.code(), Some("managed_collection"));
}

#[test]
fn admin_recovery_reports_pricing_audit_startup_failure() {
    let server = start("pricing-audit-open-failure", |config| {
        let blocker = config.data_path.join("audit-parent-file");
        std::fs::write(&blocker, b"not a directory").unwrap();
        config.pricing_audit_path = Some(blocker.join("pricing-audit.log"));
    });

    let recovery = admin_client(&server).recovery().unwrap();
    let audit = recovery.get("pricing_audit").unwrap();
    assert_eq!(audit.get("available").and_then(Json::as_bool), Some(false));
    let diagnostic = audit
        .get_str("startup_error")
        .expect("audit initialization failure must be diagnosable to admins");
    assert!(diagnostic.contains("audit_log_integrity"));

    let rule = preppydb_json::parse(
        r#"{"item_id":"hat","min_price_cents":100,"max_price_cents":200,"anchor_price_cents":150,"step_cents":20}"#,
    )
    .unwrap();
    let error = admin_client(&server).put_price_rule(&rule).unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.code(), Some("audit_unavailable"));
}

#[test]
fn unresolved_pricing_audit_intent_blocks_retries_after_restart() {
    let server = start("pricing-audit-unresolved", |config| {
        let audit_path = config.data_path.join("pricing-audit.log");
        let mut audit = AuditLog::open(&audit_path).unwrap();
        audit
            .append_pricing_transition(
                "request-before-crash:purchase",
                "started",
                100,
                "purchase",
                Json::obj()
                    .with("purchase_id", Json::str("purchase-1"))
                    .with("price_cents", Json::int(250)),
            )
            .unwrap();

        // Model a process that durably changed the database, then terminated before
        // it could append the terminal audit record.
        let engine = Engine::open(EngineConfig::new(&config.data_path)).unwrap();
        engine
            .create_collection(preppydb_storage::catalog::CollectionMeta::new(
                "purchases",
                100,
            ))
            .unwrap();
        engine
            .put(
                "purchases",
                "purchase-1",
                Json::obj().with("price_cents", Json::int(250)),
                Default::default(),
            )
            .unwrap();
        drop(engine);
        config.pricing_audit_path = Some(audit_path);
    });

    let recovery = admin_client(&server).recovery().unwrap();
    let audit = recovery.get("pricing_audit").unwrap();
    let pending = audit
        .get("pending_operations")
        .and_then(Json::as_array)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].get_str("operation"), Some("purchase"));
    assert_eq!(
        admin_client(&server)
            .get_document("purchases", "purchase-1")
            .unwrap()
            .get_path("value.price_cents")
            .and_then(Json::as_i64),
        Some(250)
    );

    let rule = preppydb_json::parse(
        r#"{"item_id":"hat","min_price_cents":100,"max_price_cents":200,"anchor_price_cents":150,"step_cents":20}"#,
    )
    .unwrap();
    let error = admin_client(&server).put_price_rule(&rule).unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.code(), Some("audit_outcome_unknown"));
}

#[test]
fn exact_pricing_intent_is_reconciled_during_server_restart() {
    let server = start("pricing-audit-reconcile", |config| {
        let engine = Engine::open(EngineConfig::new(&config.data_path)).unwrap();
        engine
            .create_collection(preppydb_storage::catalog::CollectionMeta::new(
                "price_rules",
                100,
            ))
            .unwrap();
        let expected = Json::obj()
            .with("item_id", Json::str("hat"))
            .with("version", Json::uint(2))
            .with("current_price_cents", Json::int(175));
        engine
            .put("price_rules", "hat", expected.clone(), Default::default())
            .unwrap();
        drop(engine);

        let audit_path = config.data_path.join("pricing-audit.log");
        let mut audit = AuditLog::open(&audit_path).unwrap();
        audit
            .append_pricing_transition_with_recovery(
                "request-before-crash:rule_updated",
                "started",
                100,
                "rule_updated",
                Json::obj().with("item_id", Json::str("hat")),
                Some(Json::obj().with(
                    "documents",
                    Json::Array(vec![Json::obj()
                        .with("collection", Json::str("price_rules"))
                        .with("key", Json::str("hat"))
                        .with("expected", expected)
                        .with("before_exists", Json::Bool(false))
                        .with("before", Json::Null)]),
                )),
            )
            .unwrap();
        config.pricing_audit_path = Some(audit_path);
    });

    let recovery = admin_client(&server).recovery().unwrap();
    let audit = recovery.get("pricing_audit").unwrap();
    assert!(audit
        .get("pending_operations")
        .and_then(Json::as_array)
        .unwrap()
        .is_empty());
    let reopened = AuditLog::open(server.dir.join("pricing-audit.log")).unwrap();
    assert!(reopened.pending_operations().is_empty());
    assert_eq!(reopened.entry_count(), 2);
}

fn assert_pricing_operation_crash_is_reconciled(mode: &str, terminal_kind: &str) {
    let server = start("pricing-operation-process-crash", |config| {
        let output = pricing_crash_child_command()
            .args(["--exact", "pricing_operation_crash_child", "--nocapture"])
            .env("PREPPYDB_TEST_PRICING_CRASH_DIR", &config.data_path)
            .env("PREPPYDB_TEST_PRICING_CRASH_MODE", mode)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "child should terminate at the injected crash point; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("PRICING_TEST_CRASH_POINT={mode}")),
            "child did not reach the intended crash point: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        config.pricing_audit_path = Some(config.data_path.join("pricing-audit.log"));
    });

    let admin = admin_client(&server);
    let recovery = admin.recovery().unwrap();
    let audit_status = recovery.get("pricing_audit").unwrap();
    assert_eq!(
        audit_status.get("available").and_then(Json::as_bool),
        Some(true)
    );
    assert_eq!(
        audit_status
            .get("pending_operations")
            .and_then(Json::as_array)
            .map(Vec::len),
        Some(0)
    );

    let stored = admin.get_document("price_rules", "hat").unwrap();
    let expected_price = if terminal_kind == "pricing_operation_committed" {
        175
    } else {
        150
    };
    assert_eq!(
        stored
            .get_path("value.current_price_cents")
            .and_then(Json::as_i64),
        Some(expected_price)
    );
    let audit = AuditLog::open(server.dir.join("pricing-audit.log")).unwrap();
    assert_eq!(audit.entry_count(), 2);
    assert!(audit.pending_operations().is_empty());
    assert_eq!(audit.entries().back().unwrap().kind, terminal_kind);
}

fn pricing_crash_child_command() -> Command {
    let executable = std::env::current_exe().unwrap();
    #[cfg(target_arch = "aarch64")]
    if let Ok(runner) = std::env::var("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER") {
        let mut parts = runner.split_whitespace();
        if let Some(program) = parts.next() {
            let mut command = Command::new(program);
            command.args(parts).arg(executable);
            return command;
        }
    }
    Command::new(executable)
}

#[test]
fn pricing_operation_crash_before_mutation_recovers_as_aborted() {
    assert_pricing_operation_crash_is_reconciled("before", "pricing_operation_aborted");
}

#[test]
fn pricing_operation_crash_after_mutation_before_audit_recovers_as_committed() {
    assert_pricing_operation_crash_is_reconciled("after", "pricing_operation_committed");
}

#[test]
fn pricing_operation_crash_child() {
    let Some(dir) = std::env::var_os("PREPPYDB_TEST_PRICING_CRASH_DIR").map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("PREPPYDB_TEST_PRICING_CRASH_MODE").unwrap();
    let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
    engine
        .create_collection(preppydb_storage::catalog::CollectionMeta::new(
            "price_rules",
            10,
        ))
        .unwrap();
    let before = Json::obj()
        .with("item_id", Json::str("hat"))
        .with("version", Json::uint(1))
        .with("current_price_cents", Json::int(150));
    engine
        .put("price_rules", "hat", before.clone(), Default::default())
        .unwrap();
    let expected = Json::obj()
        .with("item_id", Json::str("hat"))
        .with("version", Json::uint(2))
        .with("current_price_cents", Json::int(175));
    let recovery = Json::obj().with(
        "documents",
        Json::Array(vec![Json::obj()
            .with("collection", Json::str("price_rules"))
            .with("key", Json::str("hat"))
            .with("expected", expected.clone())
            .with("before_exists", Json::Bool(true))
            .with("before", before)]),
    );
    let audit_path = dir.join("pricing-audit.log");
    let mut audit = AuditLog::open(audit_path).unwrap();
    audit
        .append_pricing_transition_with_recovery(
            "process-crash:rule_updated",
            "started",
            1,
            "rule_updated",
            Json::obj().with("item_id", Json::str("hat")),
            Some(recovery),
        )
        .unwrap();
    if mode == "after" {
        engine
            .put("price_rules", "hat", expected, Default::default())
            .unwrap();
    } else {
        assert_eq!(mode, "before");
    }
    eprintln!("PRICING_TEST_CRASH_POINT={mode}");
    std::process::abort();
}

#[test]
fn simulate_is_reproducible_and_writes_nothing() {
    let server = start("simulate", |config| {
        config.pricing_seed = Some("sim-seed".to_string());
    });
    let client = Client::new(&server.base_url, &server.reader).unwrap();
    let rule = preppydb_json::parse(
        r#"{"item_id":"hat","min_price_cents":100,"max_price_cents":200,"anchor_price_cents":150,"step_cents":20}"#,
    )
    .unwrap();
    let prices = |simulation: &Json| -> Vec<i64> {
        simulation
            .get("history")
            .and_then(|v| v.as_array())
            .map(|history| {
                history
                    .iter()
                    .map(|change| {
                        change
                            .get("new_cents")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let first = client.simulate(&rule, 10, "fixed-seed").unwrap();
    let second = client.simulate(&rule, 10, "fixed-seed").unwrap();
    let other = client.simulate(&rule, 10, "different-seed").unwrap();
    assert_eq!(
        prices(&first),
        prices(&second),
        "same seed must give the same prices"
    );
    assert_ne!(prices(&first), prices(&other));
    // Nothing was persisted by a simulation.
    let err = client.scan("price_history", None, 10);
    assert!(err.is_err() || err.unwrap().is_empty());
}

#[test]
fn personality_model_and_admin_endpoints() {
    let server = start("admin", |_| {});
    let admin = Client::new(&server.base_url, &server.admin).unwrap();
    let report = admin.bootstrap_model().unwrap();
    let created = report.get("created").unwrap().as_array().unwrap().len();
    assert!(
        created >= 14,
        "expected the model collections, got {created}"
    );
    // Idempotent second run.
    let again = admin.bootstrap_model().unwrap();
    assert_eq!(again.get("created").unwrap().as_array().unwrap().len(), 0);

    admin
        .put_document(
            "personality_profiles",
            "molly",
            &preppydb_json::parse(r#"{"name":"Molly","version":1,"traits":["curious"]}"#).unwrap(),
        )
        .unwrap();
    let profiles = admin.personality_profiles().unwrap();
    assert_eq!(profiles.len(), 1);
    let overview = admin.personality_overview().unwrap();
    assert_eq!(
        overview
            .get_path("collections.personality_profiles.documents")
            .unwrap()
            .as_u64(),
        Some(1)
    );

    let checkpoint = admin.checkpoint().unwrap();
    assert!(checkpoint.get("seq").unwrap().as_u64().unwrap() > 0);
    let verify = admin.verify().unwrap();
    assert_eq!(verify.get("ok").and_then(|v| v.as_bool()), Some(true));

    let archive = server.dir.with_extension("backup.bak");
    let backup = admin.backup(archive.to_str().unwrap()).unwrap();
    assert!(backup.get("entries").unwrap().as_u64().unwrap() >= 2);
    assert!(archive.exists());
    std::fs::remove_file(&archive).unwrap();

    // Admin-only endpoints reject a write-scoped token.
    let writer = Client::new(&server.base_url, &server.writer).unwrap();
    let err = writer.checkpoint().unwrap_err();
    assert_eq!(err.status(), Some(403));
    // The economy guard is enforced over HTTP as well.
    let err = admin
        .create_collection("economy_balances", None)
        .unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert_eq!(err.code(), Some("forbidden_collection"));
}

#[test]
fn index_queries_over_http() {
    let server = start("indexes", |_| {});
    let admin = admin_client(&server);
    let client = Client::new(&server.base_url, &server.writer).unwrap();
    admin.create_collection("items", None).unwrap();
    admin
        .create_index("items", "by_price", "price", false)
        .unwrap();
    for (key, price) in [("hat", 10), ("dress", 50), ("shoes", 30)] {
        client
            .put_document("items", key, &Json::obj().with("price", Json::int(price)))
            .unwrap();
    }
    let hits = client
        .find_by_index("items", "by_price", &Json::int(50))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get_str("key"), Some("dress"));

    let (status, _, body) = raw(
        &server.base_url,
        "GET",
        "/v1/collections/items/indexes/by_price?from=10&to=30",
        Some(&server.reader),
        None,
    );
    assert_eq!(status, 200);
    let rows = body.get_path("data.rows").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 2);
    drop(client);
}

#[test]
#[cfg(feature = "ruby-integration-tests")]
fn ruby_query_result_can_drive_a_lua_cas_write_through_rust_storage() {
    let server = start("ruby-lua-composition", |config| {
        config.ruby_planner_path = Some(test_ruby_path());
        config.lua_execution_enabled = true;
    });
    let admin = admin_client(&server);
    admin.create_collection("accounts", None).unwrap();
    admin
        .create_index("accounts", "by_owner", "owner", true)
        .unwrap();
    admin
        .put_document(
            "accounts",
            "acct-1",
            &Json::obj()
                .with("owner", Json::str("alice"))
                .with("balance", Json::int(100)),
        )
        .unwrap();

    let (status, _, query_result) = raw(
        &server.base_url,
        "POST",
        "/v1/collections/accounts/query",
        Some(&server.reader),
        Some(r#"{"version":1,"filter":{"field":"owner","op":"eq","value":"alice"},"limit":1}"#),
    );
    assert_eq!(status, 200, "{query_result}");
    let row = query_result
        .get_path("data.documents")
        .and_then(Json::as_array)
        .and_then(|rows| rows.first())
        .expect("Ruby-planned Rust query should return one account");
    let input = Json::obj().with("account", row.clone());
    let script = r#"
        local account = input.account
        return {
            result = { key = account.key, previous = account.value.balance },
            operations = preppydb.array({{
                op = "put", collection = "accounts", key = account.key,
                expected_version = account.version,
                value = { owner = account.value.owner, balance = account.value.balance + 25 }
            }})
        }
    "#;
    let (status, _, mutation) = raw(
        &server.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&server.admin),
        Some(&preppydb_json::to_string(
            &Json::obj()
                .with("script", Json::str(script))
                .with("input", input),
        )),
    );
    assert_eq!(status, 200, "{mutation}");
    assert_eq!(
        mutation
            .get_path("data.applied_operations")
            .and_then(Json::as_u64),
        Some(1)
    );
    let (status, _, stored) = raw(
        &server.base_url,
        "GET",
        "/v1/collections/accounts/documents/acct-1",
        Some(&server.reader),
        None,
    );
    assert_eq!(status, 200, "{stored}");
    assert_eq!(
        stored.get_path("data.value.balance").and_then(Json::as_i64),
        Some(125)
    );
    assert_eq!(
        stored.get_path("data.version").and_then(Json::as_u64),
        Some(2)
    );
}

#[test]
fn lua_execution_is_admin_gated_and_commits_validated_batch_atomically() {
    let disabled = start("lua-disabled", |_| {});
    let disabled_body = Json::obj().with(
        "script",
        Json::str("return {result=true, operations=preppydb.array({{op='put',collection='items',key='a',expected_version=0,value={x=1}}})}"),
    );
    let (status, _, body) = raw(
        &disabled.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&disabled.admin),
        Some(&preppydb_json::to_string(&disabled_body)),
    );
    assert_eq!(status, 503);
    assert_eq!(
        body.get_path("error.code").and_then(Json::as_str),
        Some("lua_execution_unavailable")
    );

    let server = start("lua-enabled", |config| config.lua_execution_enabled = true);
    let (status, _, _) = raw(
        &server.base_url,
        "POST",
        "/v1/collections",
        Some(&server.admin),
        Some(r#"{"name":"items"}"#),
    );
    assert_eq!(status, 200);

    let source = r#"
        local amount = input.amount
        return {
            result = { credited = amount },
            operations = preppydb.array({
                { op = "put", collection = "items", key = "alice", expected_version = 0, value = { balance = amount } },
                { op = "put", collection = "items", key = "bob", expected_version = 0, value = { balance = amount * 2 } }
            })
        }
    "#;
    let execute_body = Json::obj()
        .with("script", Json::str(source))
        .with("input", Json::obj().with("amount", Json::int(125)));
    let (status, _, body) = raw(
        &server.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&server.admin),
        Some(&preppydb_json::to_string(&execute_body)),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body.get_path("data.applied_operations")
            .and_then(Json::as_u64),
        Some(2)
    );
    let (_, _, alice) = raw(
        &server.base_url,
        "GET",
        "/v1/collections/items/documents/alice",
        Some(&server.reader),
        None,
    );
    assert_eq!(
        alice.get_path("data.value.balance").and_then(Json::as_i64),
        Some(125)
    );
    let alice_version = alice
        .get_path("data.version")
        .and_then(Json::as_u64)
        .unwrap();
    let conditional = Json::obj()
        .with(
            "script",
            Json::str("return {result=true, operations=preppydb.array({{op='put',collection='items',key='alice',expected_version=input.version,value={balance=126}}})}"),
        )
        .with("input", Json::obj().with("version", Json::uint(alice_version)));
    for expected_status in [200, 409] {
        let (status, _, _) = raw(
            &server.base_url,
            "POST",
            "/v1/lua/execute",
            Some(&server.admin),
            Some(&preppydb_json::to_string(&conditional)),
        );
        assert_eq!(status, expected_status);
    }

    let admin = admin_client(&server);
    admin
        .create_index("items", "unique_balance", "balance", true)
        .unwrap();
    let conflicting = Json::obj().with(
        "script",
        Json::str(
            "return {result=true, operations=preppydb.array({{op='put',collection='items',key='charlie',expected_version=0,value={balance=500}},{op='put',collection='items',key='dave',expected_version=0,value={balance=500}}})}",
        ),
    );
    let (status, _, _) = raw(
        &server.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&server.admin),
        Some(&preppydb_json::to_string(&conflicting)),
    );
    assert_eq!(status, 400);
    for key in ["charlie", "dave"] {
        let (status, _, _) = raw(
            &server.base_url,
            "GET",
            &format!("/v1/collections/items/documents/{key}"),
            Some(&server.reader),
            None,
        );
        assert_eq!(status, 404);
    }

    let (status, _, _) = raw(
        &server.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&server.writer),
        Some(&preppydb_json::to_string(&execute_body)),
    );
    assert_eq!(status, 403);

    let invalid = Json::obj().with(
        "script",
        Json::str("return {result=true, operations=preppydb.array({{op='unknown',collection='items',key='partial'}})}"),
    );
    let (status, _, _) = raw(
        &server.base_url,
        "POST",
        "/v1/lua/execute",
        Some(&server.admin),
        Some(&preppydb_json::to_string(&invalid)),
    );
    assert_eq!(status, 422);
    let (status, _, _) = raw(
        &server.base_url,
        "GET",
        "/v1/collections/items/documents/partial",
        Some(&server.reader),
        None,
    );
    assert_eq!(status, 404);
}
