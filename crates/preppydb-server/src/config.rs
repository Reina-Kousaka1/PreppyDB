//! Server configuration.
//!
//! The configuration is plain JSON (no external parser needed) and is validated
//! strictly: an unknown or unsafe setting stops the server instead of being ignored.

use preppydb_json::Json;
use preppydb_storage::engine::{EngineConfig, RepairPolicy};
use preppydb_storage::wal::SyncMode;
use std::path::{Path, PathBuf};

/// Runtime configuration of the HTTP server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// `host:port` to bind. Loopback by default.
    pub listen: String,
    pub data_path: PathBuf,
    /// Must be set explicitly to bind to a non-loopback address.
    pub allow_public_bind: bool,
    pub max_body_bytes: usize,
    /// Requests per second allowed per credential (0 disables the limiter).
    pub rate_limit_per_second: u32,
    pub rate_limit_burst: u32,
    pub sync_mode: SyncMode,
    pub read_only: bool,
    /// Extra allowed CORS origins (the same origin is always allowed).
    pub cors_origins: Vec<String>,
    /// Seed for the pricing engine; when `None` the OS entropy source is used.
    pub pricing_seed: Option<String>,
    /// Interval for the background TTL sweeper (`None` disables it).
    pub ttl_sweep_interval_ms: Option<u64>,
    pub checkpoint_every_mutations: Option<u64>,
    pub repair: RepairPolicy,
    /// Optional override for the credential store location.
    pub auth_path: Option<PathBuf>,
    /// Optional override for the pricing audit log location.
    pub pricing_audit_path: Option<PathBuf>,
    /// Worker threads for the HTTP server.
    pub workers: usize,
}

impl ServerConfig {
    pub fn new(data_path: impl Into<PathBuf>) -> ServerConfig {
        ServerConfig {
            listen: "127.0.0.1:7070".to_string(),
            data_path: data_path.into(),
            allow_public_bind: false,
            max_body_bytes: 4 * 1024 * 1024,
            rate_limit_per_second: 50,
            rate_limit_burst: 100,
            sync_mode: SyncMode::Always,
            read_only: false,
            cors_origins: Vec::new(),
            pricing_seed: None,
            ttl_sweep_interval_ms: Some(30_000),
            checkpoint_every_mutations: Some(50_000),
            repair: RepairPolicy::Strict,
            auth_path: None,
            pricing_audit_path: None,
            workers: 8,
        }
    }

    pub fn auth_file(&self) -> PathBuf {
        self.auth_path
            .clone()
            .unwrap_or_else(|| self.data_path.join("auth.json"))
    }

    pub fn audit_file(&self) -> PathBuf {
        self.pricing_audit_path
            .clone()
            .unwrap_or_else(|| self.data_path.join("pricing-audit.log"))
    }

    /// Build the storage engine configuration implied by this server config.
    pub fn engine_config(&self) -> EngineConfig {
        let mut cfg = EngineConfig::new(self.data_path.clone());
        cfg.sync_mode = self.sync_mode;
        cfg.read_only = self.read_only;
        cfg.repair = self.repair;
        cfg.checkpoint_every_mutations = self.checkpoint_every_mutations;
        cfg
    }

    /// Reject unsafe combinations.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.listen.trim().is_empty() {
            problems.push("listen address must not be empty".to_string());
        }
        if is_public_bind(&self.listen) && !self.allow_public_bind {
            problems.push(format!(
                "refusing to bind to '{}' without allow_public_bind=true (use 127.0.0.1 for local-only access, or terminate TLS in a reverse proxy and opt in explicitly)",
                self.listen
            ));
        }
        if self.data_path.as_os_str().is_empty() {
            problems.push("data_path must be set".to_string());
        }
        if self.max_body_bytes < 1024 {
            problems.push("max_body_bytes must be at least 1024".to_string());
        }
        if self.max_body_bytes > 256 * 1024 * 1024 {
            problems.push("max_body_bytes must not exceed 256 MiB".to_string());
        }
        if self.workers == 0 || self.workers > 512 {
            problems.push("workers must be between 1 and 512".to_string());
        }
        if self.rate_limit_per_second > 0 && self.rate_limit_burst < self.rate_limit_per_second {
            problems.push("rate_limit_burst must be >= rate_limit_per_second".to_string());
        }
        if self.read_only && self.ttl_sweep_interval_ms.is_some() {
            problems.push("ttl_sweep_interval_ms must be null in read_only mode".to_string());
        }
        problems
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("listen", Json::str(self.listen.clone()))
            .with("data_path", Json::str(self.data_path.display().to_string()))
            .with("allow_public_bind", Json::Bool(self.allow_public_bind))
            .with("max_body_bytes", Json::uint(self.max_body_bytes as u64))
            .with(
                "rate_limit_per_second",
                Json::uint(self.rate_limit_per_second as u64),
            )
            .with("rate_limit_burst", Json::uint(self.rate_limit_burst as u64))
            .with("sync_mode", Json::str(self.sync_mode.as_str()))
            .with("read_only", Json::Bool(self.read_only))
            .with(
                "cors_origins",
                Json::Array(
                    self.cors_origins
                        .iter()
                        .map(|o| Json::str(o.clone()))
                        .collect(),
                ),
            )
            .with(
                "pricing_seed",
                self.pricing_seed
                    .clone()
                    .map(Json::str)
                    .unwrap_or(Json::Null),
            )
            .with(
                "ttl_sweep_interval_ms",
                self.ttl_sweep_interval_ms
                    .map(Json::uint)
                    .unwrap_or(Json::Null),
            )
            .with(
                "checkpoint_every_mutations",
                self.checkpoint_every_mutations
                    .map(Json::uint)
                    .unwrap_or(Json::Null),
            )
            .with(
                "repair",
                Json::str(match self.repair {
                    RepairPolicy::Strict => "strict",
                    RepairPolicy::TruncateCorruptTail => "truncate",
                }),
            )
            .with("workers", Json::uint(self.workers as u64))
    }

    /// Apply a JSON document (from a config file) on top of the defaults.
    pub fn merge_json(&mut self, value: &Json) -> std::result::Result<(), String> {
        let unknown: Vec<String> = value
            .as_object()
            .map(|map| {
                map.keys()
                    .filter(|key| {
                        !matches!(
                            key.as_str(),
                            "listen"
                                | "data_path"
                                | "allow_public_bind"
                                | "max_body_bytes"
                                | "rate_limit_per_second"
                                | "rate_limit_burst"
                                | "sync_mode"
                                | "read_only"
                                | "cors_origins"
                                | "pricing_seed"
                                | "ttl_sweep_interval_ms"
                                | "checkpoint_every_mutations"
                                | "repair"
                                | "auth_path"
                                | "pricing_audit_path"
                                | "workers"
                        )
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown configuration keys: {}",
                unknown.join(", ")
            ));
        }
        if let Some(value) = value.get_str("listen") {
            self.listen = value.to_string();
        }
        if let Some(value) = value.get_str("data_path") {
            self.data_path = PathBuf::from(value);
        }
        if let Some(value) = value.get("allow_public_bind").and_then(|v| v.as_bool()) {
            self.allow_public_bind = value;
        }
        if let Some(value) = value.get("max_body_bytes").and_then(|v| v.as_u64()) {
            self.max_body_bytes = value as usize;
        }
        if let Some(value) = value.get("rate_limit_per_second").and_then(|v| v.as_u64()) {
            self.rate_limit_per_second = value as u32;
        }
        if let Some(value) = value.get("rate_limit_burst").and_then(|v| v.as_u64()) {
            self.rate_limit_burst = value as u32;
        }
        if let Some(value) = value.get_str("sync_mode") {
            self.sync_mode = SyncMode::parse(value).map_err(|err| err.to_string())?;
        }
        if let Some(value) = value.get("read_only").and_then(|v| v.as_bool()) {
            self.read_only = value;
        }
        if let Some(value) = value.get("cors_origins").and_then(|v| v.as_array()) {
            self.cors_origins = value
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect();
        }
        match value.get("pricing_seed") {
            Some(Json::Null) => self.pricing_seed = None,
            Some(entry) => {
                self.pricing_seed = Some(
                    entry
                        .as_str()
                        .ok_or_else(|| "pricing_seed must be a string or null".to_string())?
                        .to_string(),
                )
            }
            None => {}
        }
        match value.get("ttl_sweep_interval_ms") {
            Some(Json::Null) => self.ttl_sweep_interval_ms = None,
            Some(entry) => {
                self.ttl_sweep_interval_ms = Some(entry.as_u64().ok_or_else(|| {
                    "ttl_sweep_interval_ms must be an integer or null".to_string()
                })?)
            }
            None => {}
        }
        match value.get("checkpoint_every_mutations") {
            Some(Json::Null) => self.checkpoint_every_mutations = None,
            Some(entry) => {
                self.checkpoint_every_mutations = Some(entry.as_u64().ok_or_else(|| {
                    "checkpoint_every_mutations must be an integer or null".to_string()
                })?)
            }
            None => {}
        }
        if let Some(value) = value.get_str("repair") {
            self.repair = RepairPolicy::parse(value).map_err(|err| err.to_string())?;
        }
        if let Some(value) = value.get_str("auth_path") {
            self.auth_path = Some(PathBuf::from(value));
        }
        if let Some(value) = value.get_str("pricing_audit_path") {
            self.pricing_audit_path = Some(PathBuf::from(value));
        }
        if let Some(value) = value.get("workers").and_then(|v| v.as_u64()) {
            self.workers = value as usize;
        }
        Ok(())
    }

    /// Load a JSON configuration file.
    pub fn load(path: &Path) -> std::result::Result<ServerConfig, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        let value = preppydb_json::parse(&text)
            .map_err(|err| format!("{} is not valid JSON: {err}", path.display()))?;
        let mut config = ServerConfig::new("data");
        config.merge_json(&value)?;
        Ok(config)
    }
}

fn is_public_bind(listen: &str) -> bool {
    let host = match listen.rsplit_once(':') {
        Some((host, _port)) => host,
        None => listen,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => true, // a hostname: treat as public
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let config = ServerConfig::new("/tmp/preppydb");
        assert!(config.validate().is_empty(), "{:?}", config.validate());
        assert!(config.listen.starts_with("127.0.0.1"));
        assert!(!config.allow_public_bind);
    }

    #[test]
    fn public_bind_requires_opt_in() {
        let mut config = ServerConfig::new("/tmp/preppydb");
        config.listen = "0.0.0.0:8080".to_string();
        assert!(!config.validate().is_empty());
        config.allow_public_bind = true;
        assert!(config.validate().is_empty());
    }

    #[test]
    fn json_roundtrip_and_unknown_keys() {
        let mut config = ServerConfig::new("/tmp/x");
        let json = config.to_json();
        config.merge_json(&json).unwrap();
        assert_eq!(config.listen, "127.0.0.1:7070");
        let bad = preppydb_json::parse(r#"{"listen":"127.0.0.1:1","bogus":1}"#).unwrap();
        assert!(config.merge_json(&bad).is_err());
    }

    #[test]
    fn validation_catches_nonsense() {
        let mut config = ServerConfig::new("/tmp/x");
        config.max_body_bytes = 10;
        config.workers = 0;
        config.rate_limit_per_second = 100;
        config.rate_limit_burst = 10;
        let problems = config.validate();
        assert!(problems.len() >= 3, "{problems:?}");
    }

    #[test]
    fn engine_config_mirrors_server_settings() {
        let mut config = ServerConfig::new("/tmp/db");
        config.sync_mode = SyncMode::Never;
        config.read_only = true;
        let engine = config.engine_config();
        assert_eq!(engine.sync_mode, SyncMode::Never);
        assert!(engine.read_only);
        assert_eq!(engine.path, PathBuf::from("/tmp/db"));
    }
}
