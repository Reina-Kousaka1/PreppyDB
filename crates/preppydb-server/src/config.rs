//! Server configuration.
//!
//! The configuration is plain JSON (no external parser needed) and is validated
//! strictly: an unknown or unsafe setting stops the server instead of being ignored.

use preppydb_json::Json;
use preppydb_storage::engine::{EngineConfig, RepairPolicy};
use preppydb_storage::wal::SyncMode;
use std::path::{Path, PathBuf};

/// Named resource presets. Profiles only change server resource settings; they never
/// weaken storage durability or alter the on-disk format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceProfile {
    Standard,
    Tiny,
}

impl ResourceProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceProfile::Standard => "standard",
            ResourceProfile::Tiny => "tiny",
        }
    }

    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "standard" => Ok(ResourceProfile::Standard),
            "tiny" => Ok(ResourceProfile::Tiny),
            _ => Err(format!(
                "unknown resource profile '{value}' (expected standard or tiny)"
            )),
        }
    }
}

/// Runtime configuration of the HTTP server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Named preset that produced the resource settings; explicit settings may override it.
    pub profile: ResourceProfile,
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
    /// Trusted absolute path to Ruby; absent disables the experimental query endpoint.
    pub ruby_planner_path: Option<PathBuf>,
    /// Enables bounded, admin-only server-side Lua mutation scripts.
    pub lua_execution_enabled: bool,
    /// Worker threads for the HTTP server.
    pub workers: usize,
    /// Bounded accepted-connection queue for HTTP workers.
    pub backlog: usize,
}

impl ServerConfig {
    pub fn new(data_path: impl Into<PathBuf>) -> ServerConfig {
        ServerConfig {
            profile: ResourceProfile::Standard,
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
            ruby_planner_path: None,
            lua_execution_enabled: false,
            workers: 8,
            backlog: 256,
        }
    }

    /// Apply a resource preset without changing durability or storage limits.
    pub fn apply_profile(&mut self, profile: ResourceProfile) {
        self.profile = profile;
        match profile {
            ResourceProfile::Standard => {
                self.max_body_bytes = 4 * 1024 * 1024;
                self.ttl_sweep_interval_ms = Some(30_000);
                self.workers = 8;
                self.backlog = 256;
            }
            ResourceProfile::Tiny => {
                self.max_body_bytes = 512 * 1024;
                // Expired documents remain invisible to reads; physical cleanup is
                // explicit through the admin sweep endpoint/CLI.
                self.ttl_sweep_interval_ms = None;
                self.workers = 1;
                self.backlog = 4;
            }
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
        if self
            .ruby_planner_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            problems.push("ruby_planner_path must be an absolute path".into());
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
        if self.backlog == 0 || self.backlog > 65_536 {
            problems.push("backlog must be between 1 and 65536".to_string());
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
            .with("profile", Json::str(self.profile.as_str()))
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
                "auth_path",
                self.auth_path
                    .as_ref()
                    .map(|path| Json::str(path.to_string_lossy().to_string()))
                    .unwrap_or(Json::Null),
            )
            .with(
                "pricing_audit_path",
                self.pricing_audit_path
                    .as_ref()
                    .map(|path| Json::str(path.to_string_lossy().to_string()))
                    .unwrap_or(Json::Null),
            )
            .with(
                "ruby_planner_path",
                self.ruby_planner_path
                    .as_ref()
                    .map(|path| Json::str(path.to_string_lossy().to_string()))
                    .unwrap_or(Json::Null),
            )
            .with(
                "lua_execution_enabled",
                Json::Bool(self.lua_execution_enabled),
            )
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
            .with("backlog", Json::uint(self.backlog as u64))
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
                            "profile"
                                | "listen"
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
                                | "ruby_planner_path"
                                | "lua_execution_enabled"
                                | "workers"
                                | "backlog"
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
        match value.get("profile") {
            None => {}
            Some(entry) => {
                let profile = entry
                    .as_str()
                    .ok_or("profile must be 'standard' or 'tiny'")?;
                self.apply_profile(ResourceProfile::parse(profile)?);
            }
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
            self.max_body_bytes = usize::try_from(value)
                .map_err(|_| "max_body_bytes exceeds this platform's addressable range")?;
        }
        if let Some(value) = value.get("rate_limit_per_second").and_then(|v| v.as_u64()) {
            self.rate_limit_per_second = u32::try_from(value)
                .map_err(|_| "rate_limit_per_second exceeds the supported range")?;
        }
        if let Some(value) = value.get("rate_limit_burst").and_then(|v| v.as_u64()) {
            self.rate_limit_burst =
                u32::try_from(value).map_err(|_| "rate_limit_burst exceeds the supported range")?;
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
        match value.get("ruby_planner_path") {
            None => (),
            Some(Json::Null) => self.ruby_planner_path = None,
            Some(value) => {
                self.ruby_planner_path = Some(PathBuf::from(
                    value
                        .as_str()
                        .ok_or("ruby_planner_path must be a string or null")?,
                ))
            }
        }
        if let Some(value) = value.get("lua_execution_enabled") {
            self.lua_execution_enabled = value
                .as_bool()
                .ok_or("lua_execution_enabled must be a boolean")?;
        }
        if let Some(value) = value.get("workers").and_then(|v| v.as_u64()) {
            self.workers = usize::try_from(value)
                .map_err(|_| "workers exceeds this platform's addressable range")?;
        }
        if let Some(value) = value.get("backlog").and_then(|v| v.as_u64()) {
            self.backlog = usize::try_from(value)
                .map_err(|_| "backlog exceeds this platform's addressable range")?;
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
    fn print_config_reports_effective_resource_and_path_defaults() {
        let config = ServerConfig::new("/tmp/preppydb");
        let json = config.to_json();
        assert_eq!(
            json.get("max_body_bytes").and_then(Json::as_u64),
            Some(4 * 1024 * 1024)
        );
        assert_eq!(
            json.get("ttl_sweep_interval_ms").and_then(Json::as_u64),
            Some(30_000)
        );
        assert_eq!(
            json.get("checkpoint_every_mutations")
                .and_then(Json::as_u64),
            Some(50_000)
        );
        assert_eq!(json.get("workers").and_then(Json::as_u64), Some(8));
        assert_eq!(json.get("backlog").and_then(Json::as_u64), Some(256));
        assert_eq!(json.get("profile").and_then(Json::as_str), Some("standard"));
        assert_eq!(json.get("sync_mode").and_then(Json::as_str), Some("always"));
        assert_eq!(json.get("auth_path"), Some(&Json::Null));
        assert_eq!(json.get("pricing_audit_path"), Some(&Json::Null));
    }

    #[test]
    fn tiny_profile_bounds_server_resources_without_weakening_storage_durability() {
        let mut config = ServerConfig::new("/tmp/preppydb");
        config.apply_profile(ResourceProfile::Tiny);

        assert_eq!(config.max_body_bytes, 512 * 1024);
        assert_eq!(config.workers, 1);
        assert_eq!(config.backlog, 4);
        assert_eq!(config.ttl_sweep_interval_ms, None);
        assert_eq!(config.sync_mode, SyncMode::Always);
        assert_eq!(config.checkpoint_every_mutations, Some(50_000));
        assert!(config.validate().is_empty(), "{:?}", config.validate());
    }

    #[test]
    fn tiny_profile_json_settings_can_be_overridden_explicitly() {
        let value = preppydb_json::parse(
            r#"{"profile":"tiny","workers":2,"backlog":8,"max_body_bytes":262144,"ttl_sweep_interval_ms":60000}"#,
        )
        .unwrap();
        let mut config = ServerConfig::new("/tmp/preppydb");
        config.merge_json(&value).unwrap();

        assert_eq!(config.profile, ResourceProfile::Tiny);
        assert_eq!(config.workers, 2);
        assert_eq!(config.backlog, 8);
        assert_eq!(config.max_body_bytes, 256 * 1024);
        assert_eq!(config.ttl_sweep_interval_ms, Some(60_000));
    }

    #[test]
    fn unsupported_or_malformed_resource_profile_is_rejected() {
        for input in [r#"{"profile":"fast"}"#, r#"{"profile":1}"#] {
            let mut config = ServerConfig::new("/tmp/preppydb");
            let value = preppydb_json::parse(input).unwrap();
            assert!(config.merge_json(&value).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn lua_execution_is_opt_in_and_requires_a_boolean() {
        let mut config = ServerConfig::new("/tmp/x");
        assert!(!config.lua_execution_enabled);
        config
            .merge_json(&preppydb_json::parse(r#"{"lua_execution_enabled":true}"#).unwrap())
            .unwrap();
        assert!(config.lua_execution_enabled);
        assert_eq!(
            config
                .to_json()
                .get("lua_execution_enabled")
                .and_then(Json::as_bool),
            Some(true)
        );
        assert!(config
            .merge_json(&preppydb_json::parse(r#"{"lua_execution_enabled":"yes"}"#).unwrap())
            .is_err());
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
    fn json_integer_settings_reject_values_that_would_wrap() {
        let too_large_u32 = u32::MAX as u64 + 1;
        for input in [
            format!(r#"{{"rate_limit_per_second":{too_large_u32}}}"#),
            format!(r#"{{"rate_limit_burst":{too_large_u32}}}"#),
        ] {
            let mut config = ServerConfig::new("/tmp/x");
            let value = preppydb_json::parse(&input).unwrap();
            assert!(config.merge_json(&value).is_err(), "accepted {input}");
        }

        if usize::BITS < u64::BITS {
            let too_large_usize = usize::MAX as u64 + 1;
            for key in ["max_body_bytes", "workers"] {
                let input = format!(r#"{{"{key}":{too_large_usize}}}"#);
                let mut config = ServerConfig::new("/tmp/x");
                let value = preppydb_json::parse(&input).unwrap();
                assert!(config.merge_json(&value).is_err(), "accepted {input}");
            }
        }
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
