//! The versioned HTTP API (`/v1`), the web UI routes and all authorisation checks.

use crate::auth::{AuthStore, Principal, Role};
use crate::config::ServerConfig;
use crate::http::{Handler, Request, Response, ServerCounters};
use crate::ratelimit::RateLimiter;
use preppydb_json::Json;
use preppydb_pricing::{AuditLog, PriceRule, PricingEngine};
use preppydb_storage::backup;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::{CheckedOp, Engine, PutOptions, ScanOptions};
use preppydb_storage::error::DbError;
use preppydb_storage::record::Op;
use preppydb_storage::store::Doc;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// API version reported by `/v1/version`.
pub const API_VERSION: &str = "1.0.0";

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLE_CSS: &str = include_str!("../web/style.css");
const APP_JS: &str = include_str!("../web/app.js");

/// Pricing engine plus its audit log.
pub struct PricingState {
    pub engine: PricingEngine,
    pub audit: Option<AuditLog>,
}

/// Everything a request needs.
pub struct ApiState {
    pub engine: Engine,
    pub config: ServerConfig,
    pub auth: RwLock<AuthStore>,
    pub limiter: Mutex<RateLimiter>,
    login_limiter: Mutex<RateLimiter>,
    pub counters: Arc<ServerCounters>,
    pub pricing: Mutex<PricingState>,
    audit_startup_error: Option<String>,
    pricing_operations: Mutex<()>,
    pub started_at: i64,
    requests: AtomicU64,
    pricing_collections_ready: AtomicBool,
}

impl ApiState {
    pub fn new(
        engine: Engine,
        config: ServerConfig,
        auth: AuthStore,
        counters: Arc<ServerCounters>,
    ) -> ApiState {
        let pricing_engine = match &config.pricing_seed {
            Some(seed) => PricingEngine::with_seed(seed.as_bytes()),
            None => PricingEngine::from_entropy()
                .unwrap_or_else(|_| PricingEngine::with_seed(b"fallback")),
        };
        let (mut audit, mut audit_startup_error) = match AuditLog::open(config.audit_file()) {
            Ok(audit) => (Some(audit), None),
            Err(error) => (None, Some(format!("{}: {}", error.code, error.message))),
        };
        if let Some(log) = audit.as_mut() {
            if let Err(error) = ensure_empty_audit_matches_pricing_state(&engine, log)
                .and_then(|()| reconcile_pricing_intents(&engine, log))
            {
                audit = None;
                audit_startup_error = Some(error);
            }
        }
        ApiState {
            engine,
            config: config.clone(),
            auth: RwLock::new(auth),
            limiter: Mutex::new(RateLimiter::new(
                config.rate_limit_per_second,
                config.rate_limit_burst,
            )),
            // Login attempts are limited much more aggressively than API calls.
            login_limiter: Mutex::new(RateLimiter::new(1, 10)),
            counters,
            pricing: Mutex::new(PricingState {
                engine: pricing_engine,
                audit,
            }),
            audit_startup_error,
            pricing_operations: Mutex::new(()),
            started_at: preppydb_storage::clock::now_ms(),
            requests: AtomicU64::new(0),
            pricing_collections_ready: AtomicBool::new(false),
        }
    }

    /// Startup diagnostic for an unavailable pricing audit log. The detailed value is
    /// exposed only through admin diagnostics; pricing mutations remain fail-closed.
    pub fn audit_startup_error(&self) -> Option<&str> {
        self.audit_startup_error.as_deref()
    }

    // ---------------------------------------------------------------- helpers

    fn parse_collection_meta(&self, body: &Json) -> Result<CollectionMeta, Response> {
        let name = body.get_str("name").ok_or_else(|| {
            Response::error(400, "invalid_request", "collection 'name' is required")
        })?;
        if is_managed_pricing_collection(name) {
            return Err(Response::error(
                403,
                "managed_collection",
                format!("'{name}' is managed by the pricing API"),
            ));
        }
        if preppydb_model::is_forbidden_collection(name) {
            return Err(Response::error(
                403,
                "forbidden_collection",
                format!(
                    "'{name}' is not managed by PreppyDB: the database never changes economy balances"
                ),
            ));
        }
        let mut meta = CollectionMeta::new(name, preppydb_storage::clock::now_ms());
        match body.get("schema") {
            None | Some(Json::Null) => {}
            Some(schema) => meta.schema = Some(schema.clone()),
        }
        meta.max_docs = body.get("max_docs").and_then(|v| v.as_u64());
        meta.max_doc_bytes = body.get("max_doc_bytes").and_then(|v| v.as_u64());
        meta.default_ttl_ms = body.get("default_ttl_ms").and_then(|v| v.as_i64());
        if let Some(ttl) = meta.default_ttl_ms {
            if ttl <= 0 {
                return Err(Response::error(
                    400,
                    "invalid_request",
                    "default_ttl_ms must be positive",
                ));
            }
        }
        Ok(meta)
    }

    fn authenticate(&self, request: &Request) -> Result<Principal, Response> {
        let header = match request.header("authorization") {
            Some(value) => value,
            None => return Ok(Principal::anonymous()),
        };
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| {
                Response::error(
                    401,
                    "invalid_authorization",
                    "Authorization header must use the Bearer scheme",
                )
            })?
            .trim();
        if token.is_empty() {
            return Err(Response::error(
                401,
                "missing_credentials",
                "empty bearer token",
            ));
        }
        let mut auth = self.auth.write().unwrap_or_else(|err| err.into_inner());
        auth.reload_if_changed().map_err(|_| {
            eprintln!("credential reload failed");
            Response::error(
                503,
                "authentication_unavailable",
                "authentication temporarily unavailable",
            )
        })?;
        if let Some(principal) = auth.authenticate_token(token) {
            return Ok(principal);
        }
        if let Some(principal) = auth.authenticate_session(token) {
            return Ok(principal);
        }
        Err(Response::error(
            401,
            "invalid_token",
            "the token is unknown or expired",
        ))
    }

    fn require(principal: &Principal, role: Role, what: &str) -> Result<(), Response> {
        if principal.allows(role) {
            return Ok(());
        }
        if principal.kind == "anonymous" {
            Err(Response::error(
                401,
                "authentication_required",
                format!("{what} requires authentication"),
            ))
        } else {
            Err(Response::error(
                403,
                "forbidden",
                format!("{what} requires the '{}' role", role.as_str()),
            ))
        }
    }

    fn ensure_pricing_collections(&self) -> Result<(), DbError> {
        if self.pricing_collections_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        let existing: Vec<String> = self
            .engine
            .try_list_collections()?
            .into_iter()
            .map(|meta| meta.name)
            .collect();
        for name in ["price_rules", "price_offers", "price_history", "purchases"] {
            if existing.iter().any(|existing| existing == name) {
                continue;
            }
            let now = preppydb_storage::clock::now_ms();
            let spec = preppydb_model::spec(name);
            let meta = match &spec {
                Some(spec) => spec.collection_meta(now),
                None => {
                    let mut meta = CollectionMeta::new(name, now);
                    if name == "price_offers" {
                        meta.max_docs = Some(1_000_000);
                    }
                    meta
                }
            };
            match self.engine.create_collection(meta) {
                Ok(()) => {}
                Err(DbError::CollectionExists(_)) => {}
                Err(err) => return Err(err),
            }
            // Indexes are part of the model definition; create them with the collection
            // so that index queries work immediately.
            if let Some(spec) = &spec {
                let existing_meta = self.engine.collection_meta(name)?;
                for index in spec.index_metas(now) {
                    if !existing_meta.indexes.contains_key(&index.name) {
                        match self.engine.create_index(name, index) {
                            Ok(()) => {}
                            Err(DbError::IndexExists { .. }) => {}
                            Err(err) => return Err(err),
                        }
                    }
                }
            }
        }
        self.pricing_collections_ready
            .store(true, Ordering::Relaxed);
        Ok(())
    }

    fn audit_transition(
        &self,
        operation_id: &str,
        state: &str,
        operation: &str,
        detail: Json,
    ) -> Result<(), Response> {
        self.audit_transition_with_recovery(operation_id, state, operation, detail, None)
    }

    fn audit_transition_with_recovery(
        &self,
        operation_id: &str,
        state: &str,
        operation: &str,
        detail: Json,
        recovery: Option<Json>,
    ) -> Result<(), Response> {
        let mut pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
        let audit = match pricing.audit.as_mut() {
            Some(audit) => audit,
            None => {
                return Err(Response::error(
                    503,
                    "audit_unavailable",
                    "pricing audit is unavailable; no mutation was started",
                ))
            }
        };
        if state == "started" && !audit.pending_operations().is_empty() {
            return Err(Response::error(503, "audit_outcome_unknown", "a prior pricing operation has no durable outcome; inspect the admin recovery report before retrying"));
        }
        if audit
            .append_pricing_transition_with_recovery(
                operation_id,
                state,
                preppydb_storage::clock::now_ms(),
                operation,
                detail,
                recovery,
            )
            .is_err()
        {
            pricing.audit = None;
            return Err(Response::error(503, "audit_unavailable", "pricing audit transition could not be persisted; inspect the audit log and database before retrying"));
        }
        Ok(())
    }

    fn audit_known_failure(
        &self,
        operation_id: &str,
        operation: &str,
        detail: Json,
        response: Response,
    ) -> Response {
        if response.status < 500 {
            if let Err(audit_error) =
                self.audit_transition(operation_id, "aborted", operation, detail)
            {
                return audit_error;
            }
        }
        response
    }

    fn require_audit(&self) -> Result<(), Response> {
        let pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
        if pricing.audit.is_none() {
            return Err(Response::error(
                503,
                "audit_unavailable",
                "pricing audit is unavailable",
            ));
        }
        if pricing
            .audit
            .as_ref()
            .is_some_and(|audit| !audit.pending_operations().is_empty())
        {
            return Err(Response::error(
                503,
                "audit_outcome_unknown",
                "a pricing operation has no durable terminal audit record; inspect its database state before retrying",
            ));
        }
        Ok(())
    }

    fn serialize_pricing_operation(&self) -> Result<std::sync::MutexGuard<'_, ()>, Response> {
        self.pricing_operations.lock().map_err(|_| {
            Response::error(
                503,
                "pricing_unavailable",
                "pricing operation state is unavailable",
            )
        })
    }

    /// Load a stored price rule plus its current price.
    fn load_rule(&self, item_id: &str) -> Result<(PriceRule, i64), Response> {
        let doc = self
            .engine
            .get_opt("price_rules", item_id)
            .map_err(db_error)?
            .ok_or_else(|| {
                Response::error(
                    404,
                    "rule_not_found",
                    format!("no price rule for '{item_id}'"),
                )
            })?;
        let mut definition = doc.value.clone();
        let current = definition
            .remove_key("current_price_cents")
            .and_then(|value| value.as_i64())
            .unwrap_or_else(|| definition.get_i64("anchor_price_cents").unwrap_or(1));
        let rule = PriceRule::from_json(&definition).map_err(|err| {
            Response::error(
                422,
                err.code,
                format!("stored rule is invalid: {}", err.message),
            )
        })?;
        Ok((rule, current))
    }

    fn store_rule(&self, rule: &PriceRule, current_price_cents: i64) -> Result<u64, Response> {
        let mut doc = rule.to_json().as_object().cloned().unwrap_or_default();
        let mut value = Json::obj();
        for (key, entry) in doc.iter_mut() {
            value.set(key.clone(), std::mem::take(entry));
        }
        value.set("current_price_cents", Json::int(current_price_cents));
        self.engine
            .put("price_rules", &rule.item_id, value, PutOptions::default())
            .map_err(db_error)
    }

    // ---------------------------------------------------------------- routing

    fn route(&self, request: &Request, principal: &Principal) -> Response {
        let segments = path_segments(&request.path);
        let method = request.method.as_str();
        let parts: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();

        match (method, parts.as_slice()) {
            // ---- public ------------------------------------------------------
            ("GET", ["healthz"]) => Response::json(
                200,
                Json::obj()
                    .with("status", Json::str("ok"))
                    .with("checks", Json::obj()),
            ),
            ("GET", ["readyz"]) => {
                // Readiness means "can serve reads": the storage engine is open and its
                // state is queryable. It deliberately reveals no data.
                let storage_ok = self.engine.try_stats().is_ok();
                Response::json(
                    if storage_ok { 200 } else { 503 },
                    Json::obj()
                        .with("ready", Json::Bool(storage_ok))
                        .with("read_only", Json::Bool(self.config.read_only))
                        .with(
                            "checks",
                            Json::obj().with(
                                "storage",
                                Json::str(if storage_ok { "ok" } else { "failed" }),
                            ),
                        ),
                )
            }
            ("GET", ["v1", "version"]) => ok_data(
                Json::obj()
                    .with("version", Json::str(env!("CARGO_PKG_VERSION")))
                    .with("api_version", Json::str(API_VERSION))
                    .with("storage_version", Json::str(preppydb_storage::VERSION))
                    .with("model_version", Json::str(preppydb_model::MODEL_VERSION))
                    .with("engine", Json::str("preppydb")),
            ),
            ("POST", ["v1", "auth", "login"]) => self.handle_login(request),
            ("GET", ["v1", "auth", "whoami"]) => {
                match Self::require(principal, Role::Read, "whoami") {
                    Ok(()) => ok_data(principal.to_json()),
                    Err(response) => response,
                }
            }
            ("POST", ["v1", "auth", "logout"]) => self.handle_logout(request, principal),

            // ---- UI ----------------------------------------------------------
            ("GET", []) | ("GET", [""]) => {
                Response::html(INDEX_HTML).with_header("Content-Security-Policy", CSP)
            }
            ("GET", ["assets", "style.css"]) => {
                Response::asset("text/css; charset=utf-8", STYLE_CSS)
            }
            ("GET", ["assets", "app.js"]) => {
                Response::asset("application/javascript; charset=utf-8", APP_JS)
            }

            // ---- stats -------------------------------------------------------
            ("GET", ["v1", "stats"]) => self.with_role(principal, Role::Read, "stats", || {
                let mut stats = match self.engine.try_stats() {
                    Ok(stats) => stats.to_json(),
                    Err(error) => return db_error(error),
                };
                stats.set("server", self.server_stats());
                ok_data(stats)
            }),
            ("GET", ["v1", "metrics"]) => self.with_role(principal, Role::Read, "metrics", || {
                let storage = match self.engine.try_stats() {
                    Ok(stats) => stats.metrics,
                    Err(error) => return db_error(error),
                };
                ok_data(
                    Json::obj()
                        .with("server", self.counters.snapshot())
                        .with("storage", storage),
                )
            }),

            // ---- collections -------------------------------------------------
            ("GET", ["v1", "collections"]) => {
                self.with_role(principal, Role::Read, "listing collections", || {
                    let metadata = match self.engine.try_list_collections() {
                        Ok(metadata) => metadata,
                        Err(error) => return db_error(error),
                    };
                    let mut collections = Vec::with_capacity(metadata.len());
                    for meta in metadata {
                        let count = match self.engine.count(&meta.name) {
                            Ok(count) => count,
                            Err(error) => return db_error(error),
                        };
                        collections.push(
                            Json::obj()
                                .with("name", Json::str(meta.name.clone()))
                                .with("documents", Json::uint(count as u64))
                                .with("created_at", Json::int(meta.created_at))
                                .with("has_schema", Json::Bool(meta.schema.is_some()))
                                .with(
                                    "indexes",
                                    Json::Array(
                                        meta.indexes
                                            .keys()
                                            .map(|name| Json::str(name.clone()))
                                            .collect(),
                                    ),
                                )
                                .with(
                                    "max_docs",
                                    meta.max_docs.map(Json::uint).unwrap_or(Json::Null),
                                )
                                .with(
                                    "immutable",
                                    Json::Bool(preppydb_model::is_immutable_collection(&meta.name)),
                                ),
                        );
                    }
                    ok_data(Json::obj().with("collections", Json::Array(collections)))
                })
            }
            ("POST", ["v1", "collections"]) => {
                self.with_role(principal, Role::Admin, "creating collections", || {
                    let body = match request.json_body() {
                        Ok(body) => body,
                        Err(response) => return response,
                    };
                    let meta = match self.parse_collection_meta(&body) {
                        Ok(meta) => meta,
                        Err(response) => return response,
                    };
                    let name = meta.name.clone();
                    match self.engine.create_collection(meta) {
                        Ok(()) => ok_data(Json::obj().with("name", Json::str(name))),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "collections", name]) => {
                self.with_role(principal, Role::Read, "reading a collection", || match self
                    .engine
                    .collection_meta(name)
                {
                    Ok(meta) => ok_data(Json::obj().with("collection", meta.to_json()).with(
                        "documents",
                        Json::uint(self.engine.count(name).unwrap_or(0) as u64),
                    )),
                    Err(err) => db_error(err),
                })
            }
            ("DELETE", ["v1", "collections", name]) => {
                self.with_role(principal, Role::Admin, "dropping collections", || {
                    if is_managed_pricing_collection(name) {
                        return Response::error(
                            403,
                            "managed_collection",
                            format!("'{name}' is managed by the pricing API"),
                        );
                    }
                    if !request.query_bool("confirm") {
                        return Response::error(
                            400,
                            "confirmation_required",
                            "dropping a collection requires ?confirm=true and is irreversible",
                        );
                    }
                    match self.engine.drop_collection(name, true) {
                        Ok(()) => ok_data(Json::obj().with("dropped", Json::str(*name))),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "collections", name, "indexes"]) => {
                self.with_role(principal, Role::Admin, "creating indexes", || {
                    let body = match request.json_body() {
                        Ok(body) => body,
                        Err(response) => return response,
                    };
                    let index_name = match body.get_str("name") {
                        Some(value) => value.to_string(),
                        None => {
                            return Response::error(
                                400,
                                "invalid_request",
                                "index 'name' is required",
                            )
                        }
                    };
                    let field = match body.get_str("field") {
                        Some(value) => value.to_string(),
                        None => {
                            return Response::error(
                                400,
                                "invalid_request",
                                "index 'field' is required",
                            )
                        }
                    };
                    let index = IndexMeta {
                        name: index_name.clone(),
                        field,
                        unique: body
                            .get("unique")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        sparse: body.get("sparse").and_then(|v| v.as_bool()).unwrap_or(true),
                        created_at: preppydb_storage::clock::now_ms(),
                    };
                    match self.engine.create_index(name, index) {
                        Ok(()) => ok_data(Json::obj().with("index", Json::str(index_name))),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("DELETE", ["v1", "collections", name, "indexes", index]) => {
                self.with_role(principal, Role::Admin, "dropping indexes", || {
                    match self.engine.drop_index(name, index) {
                        Ok(()) => ok_data(Json::obj().with("dropped", Json::str(*index))),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "collections", name, "indexes", index]) => {
                self.with_role(principal, Role::Read, "querying an index", || {
                    self.handle_index_query(request, name, index)
                })
            }

            // ---- documents ---------------------------------------------------
            ("POST", ["v1", "collections", name, "query"]) => {
                self.with_role(principal, Role::Read, "querying documents", || {
                    let ruby = match &self.config.ruby_planner_path {
                        Some(path) => path,
                        None => {
                            return Response::error(
                                503,
                                "query_planner_unavailable",
                                "Ruby query planner is not enabled",
                            )
                        }
                    };
                    let mut query = match request.json_body() {
                        Ok(value) if value.as_object().is_some() => value,
                        Ok(_) => {
                            return Response::error(400, "invalid_query", "query must be an object")
                        }
                        Err(response) => return response,
                    };
                    if query
                        .get("collection")
                        .is_some_and(|value| value.as_str() != Some(*name))
                    {
                        return Response::error(
                            400,
                            "invalid_query",
                            "collection does not match URL",
                        );
                    }
                    query.set("collection", Json::str(*name));
                    let metadata = match self.engine.collection_meta(name) {
                        Ok(metadata) => metadata,
                        Err(err) => return db_error(err),
                    };
                    let plan = match crate::ruby_planner::plan_with_ruby(
                        ruby,
                        &query,
                        &metadata,
                        std::time::Duration::from_secs(5),
                    ) {
                        Ok(plan) => plan,
                        Err(_) => {
                            return Response::error(
                                400,
                                "query_planning_failed",
                                "query could not be planned",
                            )
                        }
                    };
                    match crate::query_plan::execute_plan(&self.engine, &plan, &query) {
                        Ok(docs) => {
                            let documents: Vec<Json> =
                                docs.iter().map(|(key, doc)| doc_json(key, doc)).collect();
                            ok_data(
                                Json::obj()
                                    .with("collection", Json::str(*name))
                                    .with("count", Json::uint(documents.len() as u64))
                                    .with("documents", Json::Array(documents)),
                            )
                        }
                        Err(_) => Response::error(
                            409,
                            "query_plan_invalid",
                            "query plan is no longer valid",
                        ),
                    }
                })
            }
            ("GET", ["v1", "collections", name, "documents"]) => {
                self.with_role(principal, Role::Read, "scanning documents", || {
                    let options = ScanOptions {
                        prefix: request.query_param("prefix").map(|s| s.to_string()),
                        limit: request.query_usize("limit", 100).min(1000),
                        offset: request.query_usize("offset", 0),
                        reverse: request.query_bool("reverse"),
                        include_expired: false,
                    };
                    match self.engine.scan(name, &options) {
                        Ok(docs) => {
                            let documents: Vec<Json> =
                                docs.iter().map(|(key, doc)| doc_json(key, doc)).collect();
                            ok_data(
                                Json::obj()
                                    .with("collection", Json::str(*name))
                                    .with("count", Json::uint(documents.len() as u64))
                                    .with("documents", Json::Array(documents)),
                            )
                        }
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "collections", name, "documents"]) => {
                self.with_role(principal, Role::Write, "writing documents", || {
                    self.handle_put_document(request, name, None)
                })
            }
            ("GET", ["v1", "collections", name, "documents", key]) => {
                self.with_role(principal, Role::Read, "reading documents", || {
                    match self.engine.get(name, key) {
                        Ok(doc) => ok_data(doc_json(key, &doc)),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("PUT", ["v1", "collections", name, "documents", key]) => {
                self.with_role(principal, Role::Write, "writing documents", || {
                    self.handle_put_document(request, name, Some(key))
                })
            }
            ("DELETE", ["v1", "collections", name, "documents", key]) => {
                self.with_role(principal, Role::Write, "deleting documents", || {
                    if let Err(response) = self.check_collection_mutable(name, true) {
                        return response;
                    }
                    let expected = request
                        .query_param("expected_version")
                        .and_then(|value| value.parse::<u64>().ok());
                    match self.engine.delete(name, key, expected) {
                        Ok(()) => ok_data(
                            Json::obj()
                                .with("deleted", Json::str(*key))
                                .with("collection", Json::str(*name)),
                        ),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "collections", name, "batch"]) => {
                self.with_role(principal, Role::Write, "writing batches", || {
                    self.handle_batch(request, Some(name))
                })
            }
            ("POST", ["v1", "collections", _name, "ttl-sweep"]) => self.with_role(
                principal,
                Role::Write,
                "sweeping expired documents",
                || match self.engine.sweep_expired() {
                    Ok(removed) => ok_data(Json::obj().with("removed", Json::uint(removed as u64))),
                    Err(err) => db_error(err),
                },
            ),
            ("POST", ["v1", "transactions"]) => {
                self.with_role(principal, Role::Write, "committing transactions", || {
                    self.handle_batch(request, None)
                })
            }
            ("POST", ["v1", "lua", "execute"]) => {
                self.with_role(principal, Role::Admin, "executing Lua mutations", || {
                    self.handle_lua_execution(request)
                })
            }

            // ---- admin -------------------------------------------------------
            ("POST", ["v1", "admin", "checkpoint"]) => {
                self.with_role(principal, Role::Admin, "checkpoints", || {
                    match self.engine.checkpoint() {
                        Ok(written) => ok_data(
                            Json::obj()
                                .with("snapshot", Json::str(written.path.display().to_string()))
                                .with("seq", Json::uint(written.header.seq))
                                .with("documents", Json::uint(written.header.doc_count))
                                .with("bytes", Json::uint(written.bytes))
                                .with("sha256", Json::str(written.body_sha256)),
                        ),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "admin", "compact"]) => {
                self.with_role(principal, Role::Admin, "compaction", || {
                    match self.engine.compact() {
                        Ok(report) => ok_data(report.to_json()),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "admin", "sweep-expired"]) => self.with_role(
                principal,
                Role::Admin,
                "sweeping expired documents",
                || match self.engine.sweep_expired() {
                    Ok(removed) => ok_data(Json::obj().with("removed", Json::uint(removed as u64))),
                    Err(err) => db_error(err),
                },
            ),
            ("POST", ["v1", "admin", "verify"]) => {
                self.with_role(principal, Role::Admin, "verification", || {
                    match self.engine.verify() {
                        Ok(report) => {
                            let status = if report.ok { 200 } else { 409 };
                            Response::json(
                                status,
                                Json::obj()
                                    .with("ok", Json::Bool(report.ok))
                                    .with("data", report.to_json()),
                            )
                        }
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "admin", "backup"]) => {
                self.with_role(principal, Role::Admin, "backups", || {
                    let body = match request.json_body() {
                        Ok(body) => body,
                        Err(response) => return response,
                    };
                    let path = match body.get_str("path") {
                        Some(path) => PathBuf::from(path),
                        None => {
                            return Response::error(
                                400,
                                "invalid_request",
                                "backup 'path' is required",
                            )
                        }
                    };
                    let checkpoint_first = body
                        .get("checkpoint")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    match backup::create_backup(&self.engine, &path, checkpoint_first) {
                        Ok(report) => ok_data(report.to_json()),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "admin", "recovery"]) => {
                self.with_role(principal, Role::Admin, "recovery reports", || {
                    let notes: Vec<Json> = self
                        .engine
                        .recovery_notes()
                        .iter()
                        .map(|note| note.to_json())
                        .collect();
                    let pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
                    let audit_available = pricing.audit.is_some();
                    let pending_operations = pricing
                        .audit
                        .as_ref()
                        .map(|audit| {
                            audit
                                .pending_operations()
                                .values()
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    ok_data(
                        Json::obj()
                            .with("notes", Json::Array(notes))
                            .with("config", self.config.to_json())
                            .with(
                                "pricing_audit",
                                Json::obj()
                                    .with("available", Json::Bool(audit_available))
                                    .with(
                                        "startup_error",
                                        self.audit_startup_error
                                            .as_deref()
                                            .map(Json::str)
                                            .unwrap_or(Json::Null),
                                    )
                                    .with("pending_operations", Json::Array(pending_operations)),
                            ),
                    )
                })
            }
            ("GET", ["v1", "admin", "auth"]) => {
                self.with_role(principal, Role::Admin, "credential listings", || {
                    let auth = self.auth.read().unwrap_or_else(|err| err.into_inner());
                    ok_data(
                        Json::obj()
                            .with("users", auth.users_public())
                            .with("tokens", auth.tokens_public())
                            .with("sessions", Json::uint(auth.session_count() as u64)),
                    )
                })
            }

            // ---- personality model ------------------------------------------
            ("POST", ["v1", "personality", "bootstrap"]) => {
                self.with_role(principal, Role::Admin, "model bootstrap", || {
                    match preppydb_model::bootstrap(&self.engine) {
                        Ok(report) => ok_data(report.to_json()),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "personality", "overview"]) => {
                self.with_role(principal, Role::Read, "the model overview", || {
                    match personality_overview(|name| self.engine.count(name)) {
                        Ok(overview) => ok_data(overview),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "personality", "profiles"]) => {
                self.with_role(principal, Role::Read, "profiles", || {
                    let options = ScanOptions {
                        limit: request.query_usize("limit", 50).min(500),
                        ..Default::default()
                    };
                    match self.engine.scan("personality_profiles", &options) {
                        Ok(docs) => ok_data(Json::obj().with(
                            "profiles",
                            Json::Array(docs.iter().map(|(key, doc)| doc_json(key, doc)).collect()),
                        )),
                        Err(DbError::CollectionNotFound(_)) => {
                            ok_data(Json::obj().with("profiles", Json::Array(vec![])))
                        }
                        Err(err) => db_error(err),
                    }
                })
            }

            // ---- pricing -----------------------------------------------------
            ("GET", ["v1", "pricing", "rules"]) => {
                self.with_role(principal, Role::Read, "pricing rules", || {
                    if let Err(err) = self.ensure_pricing_collections() {
                        return db_error(err);
                    }
                    let options = ScanOptions {
                        limit: 1000,
                        ..Default::default()
                    };
                    match self.engine.scan("price_rules", &options) {
                        Ok(docs) => {
                            let rules: Vec<Json> = docs
                                .iter()
                                .map(|(key, doc)| {
                                    let mut value = doc.value.clone();
                                    let current = value
                                        .remove_key("current_price_cents")
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or_else(|| {
                                            value.get_i64("anchor_price_cents").unwrap_or(0)
                                        });
                                    value.set("key", Json::str(key.clone()));
                                    value.set("current_price_cents", Json::int(current));
                                    value.set("revision", Json::uint(doc.version));
                                    value
                                })
                                .collect();
                            ok_data(Json::obj().with("rules", Json::Array(rules)))
                        }
                        Err(err) => db_error(err),
                    }
                })
            }
            ("PUT", ["v1", "pricing", "rules", item]) => {
                self.with_role(principal, Role::Write, "pricing rules", || {
                    self.handle_put_rule(request, item)
                })
            }
            ("GET", ["v1", "pricing", "rules", item]) => {
                self.with_role(principal, Role::Read, "pricing rules", || {
                    match self.load_rule(item) {
                        Ok((rule, current)) => {
                            let mut value = rule.to_json();
                            value.set("current_price_cents", Json::int(current));
                            ok_data(value)
                        }
                        Err(response) => response,
                    }
                })
            }
            ("POST", ["v1", "pricing", "rules", item, "tick"]) => {
                self.with_role(principal, Role::Write, "price changes", || {
                    self.handle_tick(request, item)
                })
            }
            ("POST", ["v1", "pricing", "offers"]) => {
                self.with_role(principal, Role::Write, "offers", || {
                    self.handle_create_offer(request)
                })
            }
            ("GET", ["v1", "pricing", "offers", offer]) => {
                self.with_role(principal, Role::Read, "offers", || {
                    match self.engine.get("price_offers", offer) {
                        Ok(doc) => ok_data(doc.value.clone()),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("POST", ["v1", "pricing", "purchases"]) => {
                self.with_role(principal, Role::Write, "purchases", || {
                    self.handle_purchase(request)
                })
            }
            ("GET", ["v1", "pricing", "history", item]) => {
                self.with_role(principal, Role::Read, "price history", || {
                    match self.engine.find_by_index(
                        "price_history",
                        "by_item",
                        &Json::str(*item),
                        200,
                    ) {
                        Ok(docs) => {
                            let mut entries: Vec<Json> = docs
                                .iter()
                                .map(|(key, doc)| {
                                    let mut value = doc.value.clone();
                                    value.set("key", Json::str(key.clone()));
                                    value
                                })
                                .collect();
                            entries.sort_by_key(|entry| entry.get_i64("at").unwrap_or(0));
                            ok_data(
                                Json::obj()
                                    .with("item_id", Json::str(*item))
                                    .with("history", Json::Array(entries)),
                            )
                        }
                        Err(DbError::CollectionNotFound(_)) => ok_data(
                            Json::obj()
                                .with("item_id", Json::str(*item))
                                .with("history", Json::Array(vec![])),
                        ),
                        Err(err) => db_error(err),
                    }
                })
            }
            ("GET", ["v1", "pricing", "audit"]) => {
                self.with_role(principal, Role::Admin, "the pricing audit log", || {
                    let limit = request.query_usize("limit", 50).min(1000);
                    let pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
                    match &pricing.audit {
                        Some(audit) => {
                            let entries = audit.entries();
                            let start = entries.len().saturating_sub(limit);
                            ok_data(
                                Json::obj().with(
                                    "entries",
                                    Json::Array(
                                        entries
                                            .iter()
                                            .skip(start)
                                            .map(|entry| entry.to_json())
                                            .collect(),
                                    ),
                                ),
                            )
                        }
                        None => ok_data(Json::obj().with("entries", Json::Array(vec![]))),
                    }
                })
            }
            ("GET", ["v1", "pricing", "audit", "verify"]) => {
                self.with_role(principal, Role::Admin, "the pricing audit log", || {
                    let pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
                    match &pricing.audit {
                        Some(audit) => match audit.verify() {
                            Ok(()) => ok_data(
                                Json::obj()
                                    .with("ok", Json::Bool(true))
                                    .with("entries", Json::uint(audit.entry_count())),
                            ),
                            Err(err) => Response::json(
                                409,
                                Json::obj().with("ok", Json::Bool(false)).with(
                                    "data",
                                    Json::obj()
                                        .with("code", Json::str(err.code))
                                        .with("message", Json::str(err.message)),
                                ),
                            ),
                        },
                        None => {
                            Response::error(503, "audit_unavailable", "audit log is not configured")
                        }
                    }
                })
            }
            ("POST", ["v1", "pricing", "simulate"]) => {
                self.with_role(principal, Role::Read, "simulations", || {
                    self.handle_simulate(request)
                })
            }

            // ---- everything else --------------------------------------------
            ("OPTIONS", _) => Response::empty(204),
            (_, path) if path.first() == Some(&"v1") => Response::error(
                404,
                "unknown_endpoint",
                format!("no endpoint for {} {}", request.method, request.path),
            ),
            _ => Response::error(404, "not_found", format!("no route for {}", request.path)),
        }
    }

    fn with_role<F>(&self, principal: &Principal, role: Role, what: &str, action: F) -> Response
    where
        F: FnOnce() -> Response,
    {
        match Self::require(principal, role, what) {
            Ok(()) => action(),
            Err(response) => response,
        }
    }

    fn handle_lua_execution(&self, request: &Request) -> Response {
        if !self.config.lua_execution_enabled {
            return Response::error(
                503,
                "lua_execution_unavailable",
                "server-side Lua execution is disabled",
            );
        }
        if self.config.read_only {
            return Response::error(
                409,
                "read_only",
                "Lua mutations are unavailable in read-only mode",
            );
        }
        let body = match request.json_body() {
            Ok(body @ Json::Object(_)) => body,
            Ok(_) => return Response::error(400, "invalid_request", "body must be an object"),
            Err(response) => return response,
        };
        let source = match body.get_str("script") {
            Some(source) => source,
            None => return Response::error(400, "invalid_request", "'script' is required"),
        };
        let input = body.get("input").cloned().unwrap_or_else(Json::obj);
        let output = match crate::lua_engine::execute(source, &input) {
            Ok(output) => output,
            Err(_) => {
                return Response::error(
                    422,
                    "lua_execution_failed",
                    "Lua script failed validation or exceeded a runtime limit",
                )
            }
        };
        let mut operations = Vec::with_capacity(output.operations.len());
        for operation in output.operations {
            let Some(fields) = operation.as_object() else {
                return Response::error(
                    422,
                    "lua_plan_invalid",
                    "each operation must be an object",
                );
            };
            let kind = operation.get_str("op");
            let collection = operation.get_str("collection");
            let key = operation.get_str("key");
            let (Some(kind), Some(collection), Some(key)) = (kind, collection, key) else {
                return Response::error(
                    422,
                    "lua_plan_invalid",
                    "each operation needs string op, collection, and key fields",
                );
            };
            if preppydb_model::is_forbidden_collection(collection) {
                return Response::error(403, "forbidden_collection", "collection is not writable");
            }
            let expected_version = operation.get("expected_version").and_then(Json::as_u64);
            let Some(expected_version) = expected_version else {
                return Response::error(
                    422,
                    "lua_plan_invalid",
                    "each mutation needs an expected_version (0 means the document must not exist)",
                );
            };
            match kind {
                "put" if fields.len() == 5 => {
                    let Some(value) = operation.get("value") else {
                        return Response::error(422, "lua_plan_invalid", "put needs a value");
                    };
                    if let Err(response) = self.check_collection_mutable(collection, false) {
                        return response;
                    }
                    operations.push(CheckedOp {
                        op: Op::Put {
                            collection: collection.to_owned(),
                            key: key.to_owned(),
                            value: value.clone(),
                            expires_at: None,
                        },
                        expected_version: Some(expected_version),
                    });
                }
                "delete" if fields.len() == 4 => {
                    if let Err(response) = self.check_collection_mutable(collection, true) {
                        return response;
                    }
                    operations.push(CheckedOp {
                        op: Op::Delete {
                            collection: collection.to_owned(),
                            key: key.to_owned(),
                        },
                        expected_version: Some(expected_version),
                    });
                }
                _ => {
                    return Response::error(
                        422,
                        "lua_plan_invalid",
                        "only put and delete operations with exact fields and a version check are supported",
                    )
                }
            }
        }
        match self.engine.batch_checked(operations) {
            Ok(applied) => ok_data(
                Json::obj()
                    .with("result", output.result)
                    .with("applied_operations", Json::uint(applied as u64)),
            ),
            Err(err) => db_error(err),
        }
    }

    fn server_stats(&self) -> Json {
        Json::obj()
            .with("api_version", Json::str(API_VERSION))
            .with("started_at", Json::int(self.started_at))
            .with(
                "requests",
                Json::uint(self.requests.load(Ordering::Relaxed)),
            )
            .with("counters", self.counters.snapshot())
            .with("read_only", Json::Bool(self.config.read_only))
            .with("workers", Json::uint(self.config.workers as u64))
            .with(
                "rate_limit",
                Json::obj()
                    .with(
                        "per_second",
                        Json::uint(self.config.rate_limit_per_second as u64),
                    )
                    .with("burst", Json::uint(self.config.rate_limit_burst as u64))
                    .with(
                        "buckets",
                        Json::uint(
                            self.limiter
                                .lock()
                                .map(|limiter| limiter.tracked() as u64)
                                .unwrap_or(0),
                        ),
                    ),
            )
    }

    // ---------------------------------------------------------------- handlers

    fn handle_login(&self, request: &Request) -> Response {
        let key = format!("login:{}", request.peer);
        {
            let mut limiter = self
                .login_limiter
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if !limiter.check(&key, preppydb_storage::clock::now_ms()) {
                return Response::error(
                    429,
                    "rate_limited",
                    "too many login attempts; wait a moment and try again",
                );
            }
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let username = match body.get_str("username") {
            Some(value) => value.to_string(),
            None => return Response::error(400, "invalid_request", "'username' is required"),
        };
        let password = match body.get_str("password") {
            Some(value) => value.to_string(),
            None => return Response::error(400, "invalid_request", "'password' is required"),
        };
        let mut auth = self.auth.write().unwrap_or_else(|err| err.into_inner());
        if auth.reload_if_changed().is_err() {
            eprintln!("credential reload failed");
            return Response::error(
                503,
                "authentication_unavailable",
                "authentication temporarily unavailable",
            );
        }
        match auth.login(&username, &password) {
            Ok((token, principal)) => ok_data(
                Json::obj()
                    .with("token", Json::str(token))
                    .with("expires_in_ms", Json::int(preppydb_server_session_ttl()))
                    .with("principal", principal.to_json()),
            ),
            Err(_) => {
                // Deliberately identical for unknown users and wrong passwords.
                Response::error(401, "invalid_credentials", "invalid credentials")
            }
        }
    }

    fn handle_logout(&self, request: &Request, _principal: &Principal) -> Response {
        let token = request
            .header("authorization")
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            })
            .map(|value| value.trim().to_string());
        if let Some(token) = token {
            let mut auth = self.auth.write().unwrap_or_else(|err| err.into_inner());
            if auth.reload_if_changed().is_err() {
                eprintln!("credential reload failed");
                return Response::error(
                    503,
                    "authentication_unavailable",
                    "authentication temporarily unavailable",
                );
            }
            let was_session = auth.logout(&token);
            // Also allow revoking an API token by presenting it (self-service).
            if !was_session {
                let mut revoked = false;
                if let Some(principal) = auth.authenticate_token(&token) {
                    // Only revoke if the caller proved possession of the token.
                    let id = auth.tokens_public().as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|item| item.get_str("name") == Some(principal.name.as_str()))
                            .and_then(|item| item.get_str("id").map(|s| s.to_string()))
                    });
                    if let Some(id) = id {
                        let _ = auth.revoke_token(&id);
                        revoked = true;
                    }
                }
                let _ = revoked;
            }
        }
        ok_data(Json::obj().with("signed_out", Json::Bool(true)))
    }

    fn handle_index_query(&self, request: &Request, collection: &str, index: &str) -> Response {
        let parse_json_param = |name: &str| -> Result<Option<Json>, Response> {
            match request.query_param(name) {
                None => Ok(None),
                Some(raw) => preppydb_json::parse(raw).map(Some).map_err(|err| {
                    Response::error(
                        400,
                        "invalid_query",
                        format!("query parameter '{name}' is not valid JSON: {err}"),
                    )
                }),
            }
        };
        let value = match parse_json_param("value") {
            Ok(value) => value,
            Err(response) => return response,
        };
        let from = match parse_json_param("from") {
            Ok(value) => value,
            Err(response) => return response,
        };
        let to = match parse_json_param("to") {
            Ok(value) => value,
            Err(response) => return response,
        };
        let limit = request.query_usize("limit", 100).min(1000);
        if let Some(value) = value {
            return match self.engine.find_by_index(collection, index, &value, limit) {
                Ok(docs) => ok_data(
                    Json::obj()
                        .with("count", Json::uint(docs.len() as u64))
                        .with(
                            "documents",
                            Json::Array(docs.iter().map(|(key, doc)| doc_json(key, doc)).collect()),
                        ),
                ),
                Err(err) => db_error(err),
            };
        }
        match self
            .engine
            .range_by_index(collection, index, from.as_ref(), to.as_ref(), limit)
        {
            Ok(rows) => ok_data(
                Json::obj()
                    .with("count", Json::uint(rows.len() as u64))
                    .with(
                        "rows",
                        Json::Array(
                            rows.iter()
                                .map(|(key, value, doc)| {
                                    Json::obj()
                                        .with("key", Json::str(key.clone()))
                                        .with("index_value", value.clone())
                                        .with("document", doc_json(key, doc))
                                })
                                .collect(),
                        ),
                    ),
            ),
            Err(err) => db_error(err),
        }
    }

    fn check_collection_mutable(&self, collection: &str, is_delete: bool) -> Result<(), Response> {
        if is_managed_pricing_collection(collection) {
            return Err(Response::error(
                403,
                "managed_collection",
                format!("'{collection}' is managed by the pricing API"),
            ));
        }
        if !preppydb_model::is_immutable_collection(collection) {
            return Ok(());
        }
        if is_delete {
            return Err(Response::error(
                409,
                "immutable_collection",
                format!("'{collection}' is append-only: records cannot be deleted"),
            ));
        }
        Ok(())
    }

    fn handle_put_document(
        &self,
        request: &Request,
        collection: &str,
        key_from_path: Option<&str>,
    ) -> Response {
        if let Err(response) = self.check_collection_mutable(collection, false) {
            return response;
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let key = match key_from_path {
            Some(key) => key.to_string(),
            None => match body.get_str("key") {
                Some(key) => key.to_string(),
                None => {
                    return Response::error(400, "invalid_request", "document 'key' is required")
                }
            },
        };
        if key.is_empty() {
            return Response::error(400, "invalid_request", "document key must not be empty");
        }
        let value = match body.get("value") {
            Some(value) => value.clone(),
            None => {
                return Response::error(
                    400,
                    "invalid_request",
                    "document 'value' is required (an envelope {\"value\": ...} is expected)",
                )
            }
        };
        // Immutable collections accept a record exactly once.
        if preppydb_model::is_immutable_collection(collection) {
            match self.engine.get_opt(collection, &key) {
                Ok(Some(_)) => {
                    return Response::error(
                        409,
                        "immutable_collection",
                        format!("'{collection}/{key}' already exists and records are append-only"),
                    )
                }
                Ok(None) => {}
                Err(err) => return db_error(err),
            }
        }
        let expected_version = match body.get("expected_version") {
            None | Some(Json::Null) => None,
            Some(entry) => match entry.as_u64() {
                Some(value) => Some(value),
                None => {
                    return Response::error(
                        400,
                        "invalid_request",
                        "expected_version must be a non-negative integer",
                    )
                }
            },
        };
        let expires_at = match body.get("expires_at") {
            None | Some(Json::Null) => None,
            Some(entry) => match entry.as_i64() {
                Some(value) => Some(value),
                None => {
                    return Response::error(400, "invalid_request", "expires_at must be an integer")
                }
            },
        };
        let options = PutOptions {
            expected_version,
            expires_at,
        };
        match self.engine.put(collection, &key, value, options) {
            Ok(version) => {
                let created = request.method == "POST";
                Response::json(
                    if created { 201 } else { 200 },
                    Json::obj().with("ok", Json::Bool(true)).with(
                        "data",
                        Json::obj()
                            .with("collection", Json::str(collection))
                            .with("key", Json::str(key))
                            .with("version", Json::uint(version)),
                    ),
                )
            }
            Err(err) => db_error(err),
        }
    }

    fn handle_batch(&self, request: &Request, default_collection: Option<&str>) -> Response {
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let operations = match body.get("operations").and_then(|v| v.as_array()) {
            Some(operations) => operations.clone(),
            None => body
                .get("ops")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        };
        if operations.is_empty() {
            return Response::error(400, "invalid_request", "'operations' must not be empty");
        }
        if operations.len() > 1000 {
            return Response::error(
                422,
                "limit_exceeded",
                "a batch may contain at most 1000 operations",
            );
        }
        let mut checked = Vec::with_capacity(operations.len());
        for operation in &operations {
            let collection = match operation.get_str("collection").or(default_collection) {
                Some(name) => name.to_string(),
                None => {
                    return Response::error(
                        400,
                        "invalid_request",
                        "each operation needs a 'collection'",
                    )
                }
            };
            let kind = match operation.get_str("op") {
                Some(kind) => kind,
                None => {
                    return Response::error(400, "invalid_request", "each operation needs an 'op'")
                }
            };
            let key = match operation.get_str("key") {
                Some(key) => key.to_string(),
                None => {
                    return Response::error(400, "invalid_request", "each operation needs a 'key'")
                }
            };
            let expected_version = operation
                .get("expected_version")
                .and_then(|value| value.as_u64());
            let op = match kind {
                "put" | "set" => {
                    if let Err(response) = self.check_collection_mutable(&collection, false) {
                        return response;
                    }
                    let value = match operation.get("value") {
                        Some(value) => value.clone(),
                        None => {
                            return Response::error(
                                400,
                                "invalid_request",
                                "put operations need a 'value'",
                            )
                        }
                    };
                    Op::Put {
                        collection,
                        key,
                        value,
                        expires_at: operation.get("expires_at").and_then(|v| v.as_i64()),
                    }
                }
                "del" | "delete" => {
                    if let Err(response) = self.check_collection_mutable(&collection, true) {
                        return response;
                    }
                    Op::Delete { collection, key }
                }
                other => {
                    return Response::error(
                        400,
                        "invalid_request",
                        format!("unsupported operation '{other}' (use put or delete)"),
                    )
                }
            };
            checked.push(CheckedOp {
                op,
                expected_version,
            });
        }
        match self.engine.batch_checked(checked) {
            Ok(count) => ok_data(
                Json::obj()
                    .with("operations", Json::uint(count as u64))
                    .with("seq", Json::uint(self.engine.last_seq())),
            ),
            Err(err) => db_error(err),
        }
    }

    // ---------------------------------------------------------------- pricing

    fn handle_put_rule(&self, request: &Request, item: &str) -> Response {
        let _operation = match self.serialize_pricing_operation() {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        if let Err(response) = self.require_audit() {
            return response;
        }
        if let Err(err) = self.ensure_pricing_collections() {
            return db_error(err);
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let mut definition = body.clone();
        if definition.get("item_id").is_none() {
            definition.set("item_id", Json::str(item));
        }
        if definition.get_str("item_id") != Some(item) {
            return Response::error(
                400,
                "invalid_request",
                "the item_id in the body must match the URL",
            );
        }
        // Bump the rule version on every definition change so that outstanding offers
        // become stale instead of silently applying to different bounds.
        let previous_doc = match self.engine.get_opt("price_rules", item) {
            Ok(doc) => doc,
            Err(error) => return db_error(error),
        };
        let previous_version = previous_doc
            .as_ref()
            .and_then(|doc| doc.value.get_i64("version"))
            .unwrap_or(0);
        definition.set("version", Json::uint(previous_version as u64 + 1));
        definition.set("updated_at", Json::int(preppydb_storage::clock::now_ms()));
        let rule = match PriceRule::from_json(&definition) {
            Ok(rule) => rule,
            Err(err) => return Response::error(422, err.code, err.message),
        };
        if let Err(err) = rule.validate() {
            return Response::error(422, err.code, err.message);
        }
        let current = rule.anchor_price_cents;
        let operation_id = format!("{}:rule_updated", request.request_id);
        let audit_detail = Json::obj()
            .with("item_id", Json::str(item))
            .with("version", Json::uint(rule.version))
            .with("rule", rule.to_json());
        let mut expected_doc = rule.to_json();
        expected_doc.set("current_price_cents", Json::int(current));
        if let Err(response) = self.audit_transition_with_recovery(
            &operation_id,
            "started",
            "rule_updated",
            audit_detail.clone(),
            Some(pricing_write_recovery(
                "price_rules",
                item,
                expected_doc,
                previous_doc.map(|doc| doc.value),
            )),
        ) {
            return response;
        }
        if let Err(response) = self.store_rule(&rule, current) {
            return self.audit_known_failure(&operation_id, "rule_updated", audit_detail, response);
        }
        if let Err(response) =
            self.audit_transition(&operation_id, "committed", "rule_updated", audit_detail)
        {
            return response;
        }
        let mut value = rule.to_json();
        value.set("current_price_cents", Json::int(current));
        ok_data(value)
    }

    fn handle_tick(&self, request: &Request, item: &str) -> Response {
        let _operation = match self.serialize_pricing_operation() {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        if let Err(response) = self.require_audit() {
            return response;
        }
        if let Err(err) = self.ensure_pricing_collections() {
            return db_error(err);
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let steps = body
            .get("steps")
            .and_then(|value| value.as_u64())
            .unwrap_or(1)
            .clamp(1, 100) as usize;
        let (rule, mut current) = match self.load_rule(item) {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
        let mut changes = Vec::with_capacity(steps);
        for step in 0..steps {
            let now = preppydb_storage::clock::now_ms();
            let change = {
                let mut pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
                match pricing.engine.next_price(&rule, current, now) {
                    Ok(change) => change,
                    Err(err) => return Response::error(422, err.code, err.message),
                }
            };
            current = change.new_cents;
            let operation_id = format!("{}:price_change:{step}", request.request_id);
            let audit_detail = change.to_json();
            let history_key = operation_id.clone();
            let history_value = Json::obj()
                .with("item_id", Json::str(item))
                .with("price_cents", Json::int(change.discounted_cents))
                .with("previous_cents", Json::int(change.previous_cents))
                .with("rule_version", Json::uint(rule.version))
                .with("at", Json::int(change.at))
                .with("reason", Json::str("random_walk"));
            if let Err(response) = self.audit_transition_with_recovery(
                &operation_id,
                "started",
                "price_change",
                audit_detail.clone(),
                Some(pricing_write_recovery(
                    "price_history",
                    &history_key,
                    history_value,
                    None,
                )),
            ) {
                return response;
            }
            if let Err(response) = self
                .engine
                .put(
                    "price_history",
                    &history_key,
                    Json::obj()
                        .with("item_id", Json::str(item))
                        .with("price_cents", Json::int(change.discounted_cents))
                        .with("previous_cents", Json::int(change.previous_cents))
                        .with("rule_version", Json::uint(rule.version))
                        .with("at", Json::int(change.at))
                        .with("reason", Json::str("random_walk")),
                    PutOptions::default(),
                )
                .map(|_| ())
                .map_err(db_error)
            {
                return self.audit_known_failure(
                    &operation_id,
                    "price_change",
                    audit_detail,
                    response,
                );
            }
            if let Err(response) =
                self.audit_transition(&operation_id, "committed", "price_change", audit_detail)
            {
                return response;
            }
            changes.push(change.to_json());
        }
        let operation_id = format!("{}:rule_price_update", request.request_id);
        let rule_detail = Json::obj()
            .with("item_id", Json::str(item))
            .with("rule_version", Json::uint(rule.version))
            .with("current_price_cents", Json::int(current));
        let stored_rule_before = match self.engine.get_opt("price_rules", item) {
            Ok(doc) => doc.map(|doc| doc.value),
            Err(error) => return db_error(error),
        };
        let mut expected_rule = rule.to_json();
        expected_rule.set("current_price_cents", Json::int(current));
        if let Err(response) = self.audit_transition_with_recovery(
            &operation_id,
            "started",
            "rule_price_update",
            rule_detail.clone(),
            Some(pricing_write_recovery(
                "price_rules",
                item,
                expected_rule,
                stored_rule_before,
            )),
        ) {
            return response;
        }
        if let Err(response) = self.store_rule(&rule, current) {
            return self.audit_known_failure(
                &operation_id,
                "rule_price_update",
                rule_detail,
                response,
            );
        }
        if let Err(response) =
            self.audit_transition(&operation_id, "committed", "rule_price_update", rule_detail)
        {
            return response;
        }
        ok_data(
            Json::obj()
                .with("item_id", Json::str(item))
                .with("current_price_cents", Json::int(current))
                .with("rule_version", Json::uint(rule.version))
                .with("changes", Json::Array(changes.clone()))
                .with("change", changes.last().cloned().unwrap_or(Json::Null)),
        )
    }

    fn handle_create_offer(&self, request: &Request) -> Response {
        let _operation = match self.serialize_pricing_operation() {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        if let Err(response) = self.require_audit() {
            return response;
        }
        if let Err(err) = self.ensure_pricing_collections() {
            return db_error(err);
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let item = match body.get_str("item_id") {
            Some(item) => item.to_string(),
            None => return Response::error(400, "invalid_request", "'item_id' is required"),
        };
        let ttl_ms = body.get("ttl_ms").and_then(|value| value.as_i64());
        if let Some(ttl) = ttl_ms {
            if ttl <= 0 || ttl > 30 * 24 * 60 * 60 * 1000 {
                return Response::error(
                    400,
                    "invalid_request",
                    "ttl_ms must be positive and at most 30 days",
                );
            }
        }
        let (rule, _current) = match self.load_rule(&item) {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
        let offer = {
            let mut pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
            match pricing
                .engine
                .create_offer(&rule, preppydb_storage::clock::now_ms(), ttl_ms)
            {
                Ok(offer) => offer,
                Err(err) => return Response::error(422, err.code, err.message),
            }
        };
        let operation_id = format!("{}:offer_created", request.request_id);
        let previous_offer = match self.engine.get_opt("price_offers", &offer.offer_id) {
            Ok(doc) => doc.map(|doc| doc.value),
            Err(error) => return db_error(error),
        };
        if previous_offer.is_some() {
            return Response::error(
                409,
                "offer_id_collision",
                "generated offer identifier already exists; no offer was changed",
            );
        }
        if let Err(response) = self.audit_transition_with_recovery(
            &operation_id,
            "started",
            "offer_created",
            offer.to_json(),
            Some(pricing_write_recovery(
                "price_offers",
                &offer.offer_id,
                offer.to_json(),
                previous_offer,
            )),
        ) {
            return response;
        }
        match self.engine.put(
            "price_offers",
            &offer.offer_id,
            offer.to_json(),
            PutOptions::default(),
        ) {
            Ok(_) => {
                if let Err(response) = self.audit_transition(
                    &operation_id,
                    "committed",
                    "offer_created",
                    offer.to_json(),
                ) {
                    return response;
                }
                Response::json(
                    201,
                    Json::obj()
                        .with("ok", Json::Bool(true))
                        .with("data", offer.to_json()),
                )
            }
            Err(err) => {
                let response = db_error(err);
                self.audit_known_failure(&operation_id, "offer_created", offer.to_json(), response)
            }
        }
    }

    fn handle_purchase(&self, request: &Request) -> Response {
        let _operation = match self.serialize_pricing_operation() {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        if let Err(response) = self.require_audit() {
            return response;
        }
        if let Err(err) = self.ensure_pricing_collections() {
            return db_error(err);
        }
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let offer_id = match body.get_str("offer_id") {
            Some(value) => value.to_string(),
            None => return Response::error(400, "invalid_request", "'offer_id' is required"),
        };
        let quantity = body
            .get("quantity")
            .and_then(|value| value.as_u64())
            .unwrap_or(1);
        if quantity == 0 || quantity > 10_000 {
            return Response::error(
                400,
                "invalid_request",
                "quantity must be between 1 and 10000",
            );
        }
        let offer_doc = match self.engine.get("price_offers", &offer_id) {
            Ok(doc) => doc,
            Err(DbError::NotFound { .. }) => {
                return Response::error(
                    404,
                    "offer_unknown",
                    format!("offer '{offer_id}' not found"),
                )
            }
            Err(err) => return db_error(err),
        };
        let item = match offer_doc.value.get_str("item_id") {
            Some(item) => item.to_string(),
            None => return Response::error(500, "internal", "stored offer is malformed"),
        };
        let (rule, _current) = match self.load_rule(&item) {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
        let offer = preppydb_pricing::PriceOffer {
            offer_id: offer_id.clone(),
            item_id: item.clone(),
            price_cents: offer_doc.value.get_i64("price_cents").unwrap_or(0),
            rule_version: offer_doc
                .value
                .get("rule_version")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            created_at: offer_doc.value.get_i64("created_at").unwrap_or(0),
            expires_at: offer_doc.value.get("expires_at").and_then(|v| v.as_i64()),
            redeemed: offer_doc
                .value
                .get("redeemed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        };
        let purchase = {
            let mut pricing = self.pricing.lock().unwrap_or_else(|err| err.into_inner());
            match pricing.engine.redeem_offer(
                &rule,
                &offer,
                quantity as u32,
                preppydb_storage::clock::now_ms(),
            ) {
                Ok(purchase) => purchase,
                Err(err) => {
                    let status = match err.code {
                        preppydb_pricing::codes::OFFER_UNKNOWN => 404,
                        preppydb_pricing::codes::OFFER_STALE => 409,
                        preppydb_pricing::codes::OFFER_EXPIRED => 410,
                        preppydb_pricing::codes::OFFER_REDEEMED => 409,
                        _ => 422,
                    };
                    return Response::error(status, err.code, err.message);
                }
            }
        };
        // Purchase and offer state change atomically; the purchase document itself is
        // append-only, so a later rule change can never rewrite the amount charged.
        let mut txn = self.engine.begin();
        if let Err(err) = txn.put("purchases", &purchase.purchase_id, purchase.to_json(), None) {
            return db_error(err);
        }
        let mut updated_offer = offer_doc.value.clone();
        updated_offer.set("redeemed", Json::Bool(true));
        updated_offer.set("purchase_id", Json::str(purchase.purchase_id.clone()));
        let previous_purchase = match self.engine.get_opt("purchases", &purchase.purchase_id) {
            Ok(doc) => doc.map(|doc| doc.value),
            Err(error) => return db_error(error),
        };
        if previous_purchase.is_some() {
            return Response::error(
                503,
                "audit_outcome_unknown",
                "purchase identifier already exists; no new purchase was recorded",
            );
        }
        if let Err(err) = txn.put("price_offers", &offer_id, updated_offer.clone(), None) {
            return db_error(err);
        }
        let operation_id = format!("{}:purchase", request.request_id);
        if let Err(response) = self.audit_transition_with_recovery(
            &operation_id,
            "started",
            "purchase",
            purchase.to_json(),
            Some(pricing_transaction_recovery(vec![
                (
                    "purchases",
                    &purchase.purchase_id,
                    purchase.to_json(),
                    previous_purchase,
                ),
                (
                    "price_offers",
                    &offer_id,
                    updated_offer.clone(),
                    Some(offer_doc.value.clone()),
                ),
            ])),
        ) {
            return response;
        }
        match txn.commit() {
            Ok(outcome) => {
                if let Err(response) = self.audit_transition(
                    &operation_id,
                    "committed",
                    "purchase",
                    purchase.to_json(),
                ) {
                    return response;
                }
                Response::json(
                    201,
                    Json::obj().with("ok", Json::Bool(true)).with(
                        "data",
                        Json::obj()
                            .with("purchase", purchase.to_json())
                            .with("seq", Json::uint(outcome.seq)),
                    ),
                )
            }
            Err(err) => {
                let response = db_error(err);
                self.audit_known_failure(&operation_id, "purchase", purchase.to_json(), response)
            }
        }
    }

    fn handle_simulate(&self, request: &Request) -> Response {
        let body = match request.json_body() {
            Ok(body) => body,
            Err(response) => return response,
        };
        let definition = match body.get("rule") {
            Some(rule) => rule.clone(),
            None => {
                let item = match body.get_str("item_id") {
                    Some(item) => item.to_string(),
                    None => {
                        return Response::error(
                            400,
                            "invalid_request",
                            "provide either 'rule' or 'item_id'",
                        )
                    }
                };
                match self.engine.get_opt("price_rules", &item) {
                    Ok(Some(doc)) => doc.value.clone(),
                    Ok(None) => {
                        return Response::error(
                            404,
                            "rule_not_found",
                            format!("no rule for '{item}'"),
                        )
                    }
                    Err(err) => return db_error(err),
                }
            }
        };
        let rule = match PriceRule::from_json(&definition) {
            Ok(rule) => rule,
            Err(err) => return Response::error(422, err.code, err.message),
        };
        let steps = body
            .get("steps")
            .and_then(|value| value.as_u64())
            .unwrap_or(10)
            .clamp(1, 1000) as usize;
        let seed = body
            .get_str("seed")
            .map(|seed| seed.as_bytes().to_vec())
            .unwrap_or_else(|| b"preppydb-simulation".to_vec());
        let mut engine = PricingEngine::with_seed(&seed);
        let mut working = rule.clone();
        let history = match engine.simulate(&mut working, steps, preppydb_storage::clock::now_ms())
        {
            Ok(history) => history,
            Err(err) => return Response::error(422, err.code, err.message),
        };
        ok_data(
            Json::obj()
                .with("item_id", Json::str(rule.item_id))
                .with(
                    "seed_fingerprint",
                    Json::str(engine.seed_fingerprint().to_string()),
                )
                .with("steps", Json::uint(steps as u64))
                .with(
                    "history",
                    Json::Array(history.iter().map(|change| change.to_json()).collect()),
                ),
        )
    }
}

fn preppydb_server_session_ttl() -> i64 {
    crate::auth::SESSION_TTL_MS
}

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; frame-ancestors 'self'; form-action 'self'; base-uri 'none'";

fn is_managed_pricing_collection(name: &str) -> bool {
    matches!(
        name,
        "price_rules" | "price_offers" | "price_history" | "purchases"
    )
}

fn personality_overview<F>(mut count_collection: F) -> Result<Json, DbError>
where
    F: FnMut(&str) -> std::result::Result<usize, DbError>,
{
    let mut summary = Json::obj();
    let mut total = 0u64;
    for spec in preppydb_model::collections() {
        let (count, exists) = match count_collection(spec.name) {
            Ok(count) => (count as u64, true),
            Err(DbError::CollectionNotFound(_)) => (0, false),
            Err(err) => return Err(err),
        };
        total += count;
        summary.set(
            spec.name,
            Json::obj()
                .with("documents", Json::uint(count))
                .with("exists", Json::Bool(exists))
                .with(
                    "immutable",
                    Json::Bool(preppydb_model::is_immutable_collection(spec.name)),
                ),
        );
    }
    Ok(Json::obj()
        .with("model_version", Json::str(preppydb_model::MODEL_VERSION))
        .with("total_documents", Json::uint(total))
        .with("collections", summary))
}

/// An empty audit chain cannot explain preexisting managed pricing records. Fail
/// closed in that case instead of treating a missing or truncated-to-empty log as
/// a fresh database. This only detects the empty-chain case; a valid non-empty
/// prefix still needs an external trusted anchor to detect suffix deletion.
fn ensure_empty_audit_matches_pricing_state(
    engine: &Engine,
    audit: &AuditLog,
) -> Result<(), String> {
    if audit.entry_count() != 0 {
        return Ok(());
    }

    let collections = engine
        .try_list_collections()
        .map_err(|error| format!("checking pricing state for an empty audit log: {error}"))?;
    for name in ["price_rules", "price_offers", "price_history", "purchases"] {
        if !collections.iter().any(|collection| collection.name == name) {
            continue;
        }
        let records = engine
            .scan(
                name,
                &ScanOptions {
                    limit: 1,
                    include_expired: true,
                    ..ScanOptions::default()
                },
            )
            .map_err(|error| format!("checking {name} for an empty pricing audit log: {error}"))?;
        if !records.is_empty() {
            return Err(format!(
                "pricing audit is empty but managed collection '{name}' already contains records"
            ));
        }
    }
    Ok(())
}

/// Reconcile only exact, self-describing pricing intents after the storage engine has
/// replayed its WAL. Legacy intents and any state that matches neither the recorded
/// before-image nor the complete expected after-image remain unresolved.
fn reconcile_pricing_intents(engine: &Engine, audit: &mut AuditLog) -> Result<(), String> {
    let pending: Vec<Json> = audit.pending_operations().values().cloned().collect();
    for intent in pending {
        let Some(recovery) = intent.get("recovery") else {
            continue;
        };
        let Some(documents) = recovery.get("documents").and_then(Json::as_array) else {
            continue;
        };
        if documents.is_empty() {
            continue;
        }
        let mut all_after = true;
        let mut all_before = true;
        let mut unambiguous_after = true;
        for expected in documents {
            let (Some(collection), Some(key), Some(expected_value)) = (
                expected.get_str("collection"),
                expected.get_str("key"),
                expected.get("expected"),
            ) else {
                all_after = false;
                all_before = false;
                break;
            };
            let before_exists = expected
                .get("before_exists")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let before = expected.get("before");
            let current = match engine.get_opt(collection, key) {
                Ok(doc) => doc.map(|doc| doc.value),
                Err(_) => {
                    all_after = false;
                    all_before = false;
                    break;
                }
            };
            all_after &= current.as_ref() == Some(expected_value);
            let before_matches = if before_exists {
                current.as_ref() == before
            } else {
                current.is_none()
            };
            all_before &= before_matches;
            unambiguous_after &= !(before_exists && before == Some(expected_value));
        }
        let state = if all_after && unambiguous_after {
            Some("committed")
        } else if all_before {
            Some("aborted")
        } else {
            None
        };
        let Some(state) = state else { continue };
        let (Some(operation_id), Some(operation), Some(detail)) = (
            intent.get_str("operation_id"),
            intent.get_str("operation"),
            intent.get("detail"),
        ) else {
            return Err("pricing audit contains a malformed pending intent".to_string());
        };
        audit
            .append_pricing_transition(
                operation_id,
                state,
                preppydb_storage::clock::now_ms(),
                operation,
                detail.clone(),
            )
            .map_err(|error| format!("resolving pricing audit intent: {}", error.message))?;
    }
    Ok(())
}

fn pricing_write_recovery(
    collection: &str,
    key: &str,
    expected: Json,
    before: Option<Json>,
) -> Json {
    pricing_transaction_recovery(vec![(collection, key, expected, before)])
}

fn pricing_transaction_recovery(documents: Vec<(&str, &str, Json, Option<Json>)>) -> Json {
    Json::obj().with(
        "documents",
        Json::Array(
            documents
                .into_iter()
                .map(|(collection, key, expected, before)| {
                    Json::obj()
                        .with("collection", Json::str(collection))
                        .with("key", Json::str(key))
                        .with("expected", expected)
                        .with("before_exists", Json::Bool(before.is_some()))
                        .with("before", before.unwrap_or(Json::Null))
                })
                .collect(),
        ),
    )
}

/// Percent-decoded path segments without the leading slash.
fn path_segments(path: &str) -> Vec<String> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(crate::http::percent_decode)
        .collect()
}

fn ok_data(data: Json) -> Response {
    Response::json(
        200,
        Json::obj().with("ok", Json::Bool(true)).with("data", data),
    )
}

fn doc_json(key: &str, doc: &Doc) -> Json {
    Json::obj()
        .with("key", Json::str(key))
        .with("value", doc.value.clone())
        .with("version", Json::uint(doc.version))
        .with("revision", Json::uint(doc.revision))
        .with("created_at", Json::int(doc.created_at))
        .with("updated_at", Json::int(doc.updated_at))
        .with(
            "expires_at",
            doc.expires_at.map(Json::int).unwrap_or(Json::Null),
        )
}

/// Map storage errors to HTTP status codes and structured bodies.
fn db_error(error: DbError) -> Response {
    let status = match &error {
        DbError::NotFound { .. }
        | DbError::CollectionNotFound(_)
        | DbError::IndexNotFound { .. } => 404,
        DbError::CollectionExists(_) | DbError::IndexExists { .. } => 409,
        DbError::Conflict { .. } | DbError::TransactionConflict { .. } => 409,
        DbError::Validation(_) => 400,
        DbError::LimitExceeded { .. } => 422,
        DbError::Corrupt { .. } => 500,
        DbError::Io { .. } => 500,
        DbError::Config(_) => 500,
        DbError::Locked(_) => 503,
        DbError::CommitOutcomeUnknown(_)
        | DbError::RestoreOutcomeUnknown(_)
        | DbError::RecoveryRequired(_) => 503,
        DbError::ReadOnly(_) => 403,
        DbError::Unsupported(_) => 501,
        DbError::Internal(_) => 500,
    };
    let mut body = Json::obj().with("ok", Json::Bool(false)).with(
        "error",
        Json::obj()
            .with("code", Json::str(error.code()))
            .with("message", Json::str(error.to_string()))
            .with("retryable", Json::Bool(error.is_retryable())),
    );
    if let DbError::TransactionConflict { details } = &error {
        body.set(
            "error",
            body.get("error").cloned().unwrap_or_else(Json::obj).with(
                "details",
                Json::Array(details.iter().map(|d| Json::str(d.clone())).collect()),
            ),
        );
    }
    Response::json(status, body)
}

impl Handler for ApiState {
    fn handle(&self, request: &Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let principal = match self.authenticate(request) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        // Rate limit per credential, or per client address for anonymous traffic.
        let key = if principal.kind == "anonymous" {
            format!("ip:{}", request.peer)
        } else {
            format!("{}:{}", principal.kind, principal.name)
        };
        {
            let mut limiter = self.limiter.lock().unwrap_or_else(|err| err.into_inner());
            if !limiter.check(&key, preppydb_storage::clock::now_ms()) {
                return Response::error(
                    429,
                    "rate_limited",
                    "request rate limit exceeded for this credential",
                )
                .with_header("Retry-After", "1");
            }
        }
        let mut response = self
            .route(request, &principal)
            // Correlates a client log line with the server's own diagnostics.
            .with_header("X-Request-Id", &request.request_id);
        if let Some(origin) = request.header("origin") {
            if self.origin_allowed(origin, request) {
                response = response
                    .with_header("Access-Control-Allow-Origin", origin)
                    .with_header("Vary", "Origin");
                if request.method == "OPTIONS" {
                    response = response
                        .with_header(
                            "Access-Control-Allow-Methods",
                            "GET, POST, PUT, DELETE, OPTIONS",
                        )
                        .with_header(
                            "Access-Control-Allow-Headers",
                            "Authorization, Content-Type",
                        )
                        .with_header("Access-Control-Max-Age", "600");
                }
            }
        }
        response
    }
}

impl ApiState {
    fn origin_allowed(&self, origin: &str, request: &Request) -> bool {
        // Same-origin requests do not send CORS headers at all; this only matters for
        // explicitly configured extra origins.
        if self
            .config
            .cors_origins
            .iter()
            .any(|allowed| allowed == origin)
        {
            return true;
        }
        // Same-origin detection: the Host header matches the Origin authority.
        if let Some(host) = request.header("host") {
            if let Some(authority) = origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"))
            {
                return authority == host;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_decode() {
        assert_eq!(
            path_segments("/v1/collections/my%20coll/documents/a%2Fb"),
            vec!["v1", "collections", "my coll", "documents", "a/b"]
        );
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn error_mapping_matches_codes() {
        let response = db_error(DbError::CollectionNotFound("x".into()));
        assert_eq!(response.status, 404);
        let response = db_error(DbError::Conflict {
            collection: "c".into(),
            key: "k".into(),
            expected: Some(1),
            actual: Some(2),
        });
        assert_eq!(response.status, 409);
        let response = db_error(DbError::Locked("held".into()));
        assert_eq!(response.status, 503);
        let response = db_error(DbError::CommitOutcomeUnknown("reopen required".into()));
        assert_eq!(response.status, 503);
        let response = db_error(DbError::RestoreOutcomeUnknown("inspect target".into()));
        assert_eq!(response.status, 503);
        let response = db_error(DbError::RecoveryRequired("inspect restore intent".into()));
        assert_eq!(response.status, 503);
        let response = db_error(DbError::ReadOnly("x".into()));
        assert_eq!(response.status, 403);
    }

    #[test]
    fn personality_overview_preserves_missing_collection_semantics() {
        let overview =
            personality_overview(|name| Err(DbError::CollectionNotFound(name.into()))).unwrap();
        assert_eq!(
            overview.get("total_documents").and_then(Json::as_u64),
            Some(0)
        );
        for spec in preppydb_model::collections() {
            assert_eq!(
                overview
                    .get("collections")
                    .and_then(|collections| collections.get(spec.name))
                    .and_then(|collection| collection.get("exists"))
                    .and_then(Json::as_bool),
                Some(false)
            );
        }
    }

    #[test]
    fn personality_overview_propagates_engine_health_errors() {
        let error =
            personality_overview(|_| Err(DbError::CommitOutcomeUnknown("reopen required".into())))
                .unwrap_err();
        let response = db_error(error);
        assert_eq!(response.status, 503);
        let body = preppydb_json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        assert_eq!(
            body.get_path("error.code").and_then(Json::as_str),
            Some("commit_outcome_unknown")
        );
    }

    #[test]
    fn doc_json_shape() {
        let doc = Doc {
            value: Json::int(1),
            version: 2,
            revision: 5,
            created_at: 1,
            updated_at: 2,
            expires_at: None,
        };
        let json = doc_json("k", &doc);
        assert_eq!(json.get_str("key"), Some("k"));
        assert_eq!(json.get("version").unwrap().as_u64(), Some(2));
        assert!(json.get("expires_at").unwrap().is_null());
    }

    fn state_with_empty_audit(tag: &str, seed_pricing_record: bool) -> (PathBuf, ApiState) {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-empty-pricing-audit-{}-{tag}-{}",
            std::process::id(),
            preppydb_storage::clock::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Engine::open(preppydb_storage::engine::EngineConfig::new(&dir)).unwrap();
        if seed_pricing_record {
            engine
                .create_collection(CollectionMeta::new("price_rules", 1))
                .unwrap();
            engine
                .put(
                    "price_rules",
                    "existing",
                    Json::obj().with("price", Json::int(100)),
                    PutOptions::default(),
                )
                .unwrap();
        } else {
            engine
                .create_collection(CollectionMeta::new("price_rules", 1))
                .unwrap();
        }
        let config = ServerConfig::new(&dir);
        let auth = AuthStore::open(config.auth_file()).unwrap();
        let state = ApiState::new(engine, config, auth, Arc::new(ServerCounters::default()));
        (dir, state)
    }

    #[test]
    fn empty_pricing_audit_fails_closed_when_managed_data_exists() {
        let (dir, state) = state_with_empty_audit("existing-data", true);
        let diagnostic = state
            .audit_startup_error()
            .expect("an empty audit cannot explain existing managed pricing data");
        assert!(diagnostic.contains("price_rules"));
        assert!(state.pricing.lock().unwrap().audit.is_none());
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_pricing_audit_allows_empty_managed_collections() {
        let (dir, state) = state_with_empty_audit("empty-collection", false);
        assert_eq!(state.audit_startup_error(), None);
        assert!(state.pricing.lock().unwrap().audit.is_some());
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pricing_intent_recovery_is_exact_and_idempotent() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "preppydb-pricing-recovery-{}-{}",
            std::process::id(),
            preppydb_storage::clock::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Engine::open(preppydb_storage::engine::EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("price_rules", 1))
            .unwrap();
        let before = Json::obj().with("price", Json::int(100));
        let after = Json::obj().with("price", Json::int(110));
        let divergent = Json::obj().with("price", Json::int(120));
        engine
            .put(
                "price_rules",
                "committed",
                before.clone(),
                PutOptions::default(),
            )
            .unwrap();
        engine
            .put(
                "price_rules",
                "ambiguous",
                divergent.clone(),
                PutOptions::default(),
            )
            .unwrap();

        let audit_path = dir.join("pricing-audit.log");
        let mut audit = AuditLog::open(&audit_path).unwrap();
        for (id, key, expected, prior) in [
            ("commit", "committed", after.clone(), Some(before.clone())),
            ("abort", "not-applied", after.clone(), None),
            ("unknown", "ambiguous", after.clone(), Some(before.clone())),
        ] {
            audit
                .append_pricing_transition_with_recovery(
                    id,
                    "started",
                    2,
                    "rule_updated",
                    Json::obj().with("key", Json::str(key)),
                    Some(pricing_write_recovery("price_rules", key, expected, prior)),
                )
                .unwrap();
        }
        engine
            .put("price_rules", "committed", after, PutOptions::default())
            .unwrap();

        reconcile_pricing_intents(&engine, &mut audit).unwrap();
        assert_eq!(audit.pending_operations().len(), 1);
        assert!(audit.pending_operations().contains_key("unknown"));
        assert_eq!(audit.entry_count(), 5);

        let mut reopened = AuditLog::open(&audit_path).unwrap();
        reconcile_pricing_intents(&engine, &mut reopened).unwrap();
        assert_eq!(reopened.pending_operations().len(), 1);
        assert_eq!(reopened.entry_count(), 5);
        let outcomes: Vec<(&str, &str)> = reopened
            .entries()
            .iter()
            .filter(|entry| {
                entry.kind == "pricing_operation_committed"
                    || entry.kind == "pricing_operation_aborted"
            })
            .map(|entry| {
                (
                    entry.kind.as_str(),
                    entry.detail.get_str("operation_id").unwrap(),
                )
            })
            .collect();
        assert!(outcomes.contains(&("pricing_operation_committed", "commit")));
        assert!(outcomes.contains(&("pricing_operation_aborted", "abort")));

        drop(reopened);
        drop(audit);
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn purchase_recovery_requires_both_transaction_documents() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "preppydb-purchase-recovery-{}-{}",
            std::process::id(),
            preppydb_storage::clock::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Engine::open(preppydb_storage::engine::EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("purchases", 1))
            .unwrap();
        engine
            .create_collection(CollectionMeta::new("price_offers", 1))
            .unwrap();

        let redeemed_offer = Json::obj()
            .with("offer_id", Json::str("offer-committed"))
            .with("redeemed", Json::Bool(true))
            .with("purchase_id", Json::str("purchase-committed"));
        let committed_purchase = Json::obj().with("purchase_id", Json::str("purchase-committed"));
        engine
            .put(
                "price_offers",
                "offer-committed",
                redeemed_offer.clone(),
                PutOptions::default(),
            )
            .unwrap();

        let open_aborted = Json::obj()
            .with("offer_id", Json::str("offer-aborted"))
            .with("redeemed", Json::Bool(false));
        let redeemed_aborted = open_aborted
            .clone()
            .with("redeemed", Json::Bool(true))
            .with("purchase_id", Json::str("purchase-aborted"));
        engine
            .put(
                "price_offers",
                "offer-aborted",
                open_aborted.clone(),
                PutOptions::default(),
            )
            .unwrap();

        let open_mixed = Json::obj()
            .with("offer_id", Json::str("offer-mixed"))
            .with("redeemed", Json::Bool(false));
        let redeemed_mixed = open_mixed
            .clone()
            .with("redeemed", Json::Bool(true))
            .with("purchase_id", Json::str("purchase-mixed"));
        let mixed_purchase = Json::obj().with("purchase_id", Json::str("purchase-mixed"));
        engine
            .put(
                "price_offers",
                "offer-mixed",
                open_mixed.clone(),
                PutOptions::default(),
            )
            .unwrap();

        let audit_path = dir.join("pricing-audit.log");
        let mut audit = AuditLog::open(&audit_path).unwrap();
        for (operation_id, purchase_id, purchase_value, offer_id, offer_after, offer_before) in [
            (
                "purchase-commit",
                "purchase-committed",
                committed_purchase.clone(),
                "offer-committed",
                redeemed_offer.clone(),
                None,
            ),
            (
                "purchase-abort",
                "purchase-aborted",
                Json::obj().with("purchase_id", Json::str("purchase-aborted")),
                "offer-aborted",
                redeemed_aborted,
                Some(open_aborted),
            ),
            (
                "purchase-mixed",
                "purchase-mixed",
                mixed_purchase.clone(),
                "offer-mixed",
                redeemed_mixed,
                Some(open_mixed),
            ),
        ] {
            audit
                .append_pricing_transition_with_recovery(
                    operation_id,
                    "started",
                    2,
                    "purchase",
                    Json::obj().with("purchase_id", Json::str(purchase_id)),
                    Some(pricing_transaction_recovery(vec![
                        ("purchases", purchase_id, purchase_value, None),
                        ("price_offers", offer_id, offer_after, offer_before),
                    ])),
                )
                .unwrap();
        }
        engine
            .put(
                "purchases",
                "purchase-committed",
                committed_purchase,
                PutOptions::default(),
            )
            .unwrap();
        engine
            .put(
                "purchases",
                "purchase-mixed",
                mixed_purchase,
                PutOptions::default(),
            )
            .unwrap();

        reconcile_pricing_intents(&engine, &mut audit).unwrap();
        assert_eq!(audit.pending_operations().len(), 1);
        assert!(audit.pending_operations().contains_key("purchase-mixed"));
        assert_eq!(audit.entry_count(), 5);
        let aborted_offer = engine
            .get_opt("price_offers", "offer-aborted")
            .unwrap()
            .unwrap()
            .value;
        assert_eq!(
            aborted_offer.get("redeemed").and_then(Json::as_bool),
            Some(false)
        );
        assert!(aborted_offer.get("purchase_id").is_none());

        drop(audit);
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
