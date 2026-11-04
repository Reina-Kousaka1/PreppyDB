//! HTTP API integration tests: authentication, authorisation, document CRUD,
//! transactions, limits, pricing and administrative endpoints.
//!
//! Each test starts a real server on an ephemeral loopback port and talks to it over
//! TCP — no mocks, no in-process shortcuts.

use preppydb_client::Client;
use preppydb_json::Json;
use preppydb_server::api::ApiState;
use preppydb_server::auth::{AuthStore, Role};
use preppydb_server::config::ServerConfig;
use preppydb_server::http::{HttpLimits, ServerCounters};
use preppydb_storage::engine::{Engine, EngineConfig};
use preppydb_storage::wal::SyncMode;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
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
fn login_flow_and_session_tokens() {
    let server = start("login", |_| {});
    // Add a user to the running server's credential file; the server reloads it.
    let mut auth = AuthStore::open(server.dir.join("auth.json")).unwrap();
    auth.create_user("admin", "correct horse battery", vec![Role::Admin])
        .unwrap();
    drop(auth);
    let mut client = Client::anonymous(&server.base_url).unwrap();
    let outcome = client.login("admin", "correct horse battery").unwrap();
    assert!(outcome.get_str("token").is_some());
    assert!(
        client.stats().is_ok(),
        "session token must work for API calls"
    );
    let mut wrong = Client::anonymous(&server.base_url).unwrap();
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
    assert_eq!(err.code(), Some("immutable_collection"));
    let overwrite = client.put_document(
        "purchases",
        "manual",
        &Json::obj().with("price_cents", Json::int(1)),
    );
    // A brand-new append is rejected only by the schema (missing fields) — never by
    // silently rewriting an existing record.
    assert!(overwrite.is_err());

    let admin = admin_client(&server);
    let verified = admin.audit_verify().unwrap();
    assert_eq!(verified.get("ok").and_then(|v| v.as_bool()), Some(true));
    let history_entries = client.price_history("skirt").unwrap();
    assert_eq!(history_entries.len(), 5);
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

    let archive = server.dir.join("backup.bak");
    let backup = admin.backup(archive.to_str().unwrap()).unwrap();
    assert!(backup.get("entries").unwrap().as_u64().unwrap() >= 2);
    assert!(archive.exists());

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
