//! `preppydb` — administration and diagnostics CLI.
//!
//! The CLI works directly on a database directory (it takes the same single-writer lock
//! as the server, so it refuses to run while a server has the directory open). This
//! keeps maintenance operations honest: they never fight a live writer, and a backup is
//! always taken from a consistent state.
//!
//! Every command prints JSON so its output can be piped into `jq` or a monitoring system.

#![forbid(unsafe_code)]

use preppydb_client::Client;
use preppydb_json::Json;
use preppydb_pricing::{AuditLog, DiscountRule, PriceRule, PricingEngine};
use preppydb_server::auth::{AuthStore, Role};
use preppydb_storage::backup;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::{Engine, EngineConfig, PutOptions, RepairPolicy, ScanOptions};
use preppydb_storage::record::Op;
use preppydb_storage::wal;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        std::process::exit(2);
    }
    match run(&args) {
        Ok(()) => {}
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        r#"preppydb {version} — administration and diagnostics CLI

USAGE
    preppydb <command> [options]

GLOBAL OPTIONS
    --data-dir <PATH>      database directory (required for local commands)
    --read-only            open the database read-only where supported
    --repair               allow recovery to truncate a corrupt log tail (destructive!)

CREDENTIALS
    init [--bootstrap-model]
    token create --name NAME --role read|write|admin [--expires-days N]
    token list
    token revoke --id ID
    user create --username NAME --role ROLE   (password from stdin)
    user list
    user disable --username NAME

DATA
    stats
    collections
    collection create --name NAME [--schema-file FILE] [--max-docs N]
    collection drop --name NAME --confirm     (destructive)
    index create --collection C --name N --field F [--unique]
    index drop --collection C --name N
    get --collection C --key K
    put --collection C --key K --value JSON | --value-file FILE [--expected-version N]
    delete --collection C --key K [--expected-version N]
    scan --collection C [--prefix P] [--limit N]
    batch --file FILE                         (JSON array of put/delete operations)

MAINTENANCE / DIAGNOSTICS
    verify                                    (checksums of manifest, snapshot and WAL)
    checkpoint
    compact
    sweep
    wal
    backup --out FILE [--no-checkpoint]
    backup-verify --archive FILE
    restore --archive FILE --target DIR [--force]
    model bootstrap

PRICING
    pricing simulate --min C --max C --anchor C --steps N --seed S
                     [--step-cents C] [--discount PERCENT]

REMOTE
    health --url URL [--timeout MS]  (optional token from PREPPYDB_TOKEN)
                           probe a running server (exit 1 when unhealthy)

EXIT CODES
    0 success, 1 command failed, 2 usage error
"#,
        version = env!("CARGO_PKG_VERSION")
    );
}

struct Flags {
    values: std::collections::BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(args: &[String], start: usize) -> Result<Flags, String> {
        let mut flags = Flags {
            values: Default::default(),
            switches: Vec::new(),
        };
        let mut index = start;
        while index < args.len() {
            let arg = &args[index];
            if !arg.starts_with("--") {
                return Err(format!("unexpected argument '{arg}'"));
            }
            let name = arg.trim_start_matches("--").to_string();
            // Switches never take a value; everything else consumes the next argument.
            const SWITCHES: &[&str] = &[
                "read-only",
                "repair",
                "unique",
                "confirm",
                "force",
                "no-checkpoint",
                "bootstrap-model",
                "sparse",
            ];
            if SWITCHES.contains(&name.as_str()) {
                flags.switches.push(name);
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone();
            flags.values.insert(name, value);
            index += 2;
        }
        Ok(flags)
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(|s| s.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn data_dir(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from(self.required("data-dir")?))
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args[0].as_str();
    match command {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" => {
            println!("preppydb {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "init" => cmd_init(&Flags::parse(args, 1)?),
        "token" => cmd_token(args),
        "user" => cmd_user(args),
        "stats" => cmd_stats(&Flags::parse(args, 1)?),
        "collections" => cmd_collections(&Flags::parse(args, 1)?),
        "collection" => cmd_collection(args),
        "index" => cmd_index(args),
        "get" => cmd_get(&Flags::parse(args, 1)?),
        "put" => cmd_put(&Flags::parse(args, 1)?),
        "delete" => cmd_delete(&Flags::parse(args, 1)?),
        "scan" => cmd_scan(&Flags::parse(args, 1)?),
        "batch" => cmd_batch(&Flags::parse(args, 1)?),
        "verify" | "fsck" => cmd_verify(&Flags::parse(args, 1)?),
        "checkpoint" => {
            let flags = Flags::parse(args, 1)?;
            let engine = open_engine(&flags)?;
            let written = engine.checkpoint().map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with("snapshot", Json::str(written.path.display().to_string()))
                    .with("seq", Json::uint(written.header.seq))
                    .with("documents", Json::uint(written.header.doc_count))
                    .with("bytes", Json::uint(written.bytes))
                    .with("sha256", Json::str(written.body_sha256)),
            );
            Ok(())
        }
        "compact" => {
            let flags = Flags::parse(args, 1)?;
            let engine = open_engine(&flags)?;
            let report = engine.compact().map_err(|err| err.to_string())?;
            print_json(report.to_json());
            Ok(())
        }
        "sweep" => {
            let flags = Flags::parse(args, 1)?;
            let engine = open_engine(&flags)?;
            let removed = engine.sweep_expired().map_err(|err| err.to_string())?;
            print_json(Json::obj().with("removed", Json::uint(removed as u64)));
            Ok(())
        }
        "wal" => cmd_wal(&Flags::parse(args, 1)?),
        "backup" => cmd_backup(&Flags::parse(args, 1)?),
        "backup-verify" => cmd_backup_verify(&Flags::parse(args, 1)?),
        "restore" => cmd_restore(&Flags::parse(args, 1)?),
        "model" => cmd_model(args),
        "pricing" => cmd_pricing(args),
        "audit" => cmd_audit(args),
        "health" => cmd_health(&Flags::parse(args, 1)?),
        other => Err(format!("unknown command '{other}' (try --help)")),
    }
}

// ------------------------------------------------------------------ helpers

fn print_json(value: Json) {
    println!("{}", preppydb_json::to_string_pretty(&value));
}

fn open_engine(flags: &Flags) -> Result<Engine, String> {
    let dir = flags.data_dir()?;
    let mut config = EngineConfig::new(&dir);
    config.read_only = flags.has("read-only");
    if flags.has("repair") {
        config.repair = RepairPolicy::TruncateCorruptTail;
    }
    Engine::open(config).map_err(|err| err.to_string())
}

fn read_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("reading {path}: {err}"))
}

fn parse_json_arg(text: &str, what: &str) -> Result<Json, String> {
    preppydb_json::parse(text).map_err(|err| format!("{what} is not valid JSON: {err}"))
}

fn read_stdin_line(prompt: &str) -> Result<String, String> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .map_err(|err| format!("writing prompt: {err}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| format!("reading stdin: {err}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Probe a running server. Used by containers and supervisors as a healthcheck.
///
/// Exit codes: 0 healthy, 1 unhealthy or unreachable, 2 usage error.
fn cmd_health(flags: &Flags) -> Result<(), String> {
    if flags.get("token").is_some() {
        return Err(
            "--token is not accepted because command-line arguments expose secrets; set PREPPYDB_TOKEN instead"
                .to_string(),
        );
    }
    let url = flags
        .get("url")
        .unwrap_or("http://127.0.0.1:7070")
        .to_string();
    let timeout_ms = match flags.get("timeout") {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| "--timeout must be a number of milliseconds".to_string())?,
        None => 3000,
    };
    let token = std::env::var("PREPPYDB_TOKEN").ok();

    let mut client = Client::anonymous(&url).map_err(|err| err.to_string())?;
    client = client.with_timeout(std::time::Duration::from_millis(timeout_ms));
    if let Some(token) = token.as_deref() {
        client.set_token(token);
    }

    let mut report = Json::obj();
    let health = match client.health() {
        Ok(value) => value,
        Err(err) => {
            report.set("status", Json::str("unreachable"));
            report.set("url", Json::str(url.clone()));
            report.set("error", Json::str(err.to_string()));
            print_json(report);
            return Err(format!("cannot reach {url}: {err}"));
        }
    };
    let ready = client.ready().ok();
    report.set("url", Json::str(url.clone()));
    report.set("health", health.clone());
    if let Some(ready) = &ready {
        report.set("ready", ready.clone());
    }
    let healthy = health.get_str("status") == Some("ok")
        && ready
            .as_ref()
            .and_then(|value| value.get("ready").and_then(|v| v.as_bool()))
            .unwrap_or(true);
    report.set(
        "status",
        Json::str(if healthy { "ok" } else { "unhealthy" }),
    );
    print_json(report);
    if healthy {
        Ok(())
    } else {
        Err("server reported an unhealthy state".to_string())
    }
}

// ------------------------------------------------------------------ commands

fn cmd_init(flags: &Flags) -> Result<(), String> {
    let dir = flags.data_dir()?;
    let config = EngineConfig::new(&dir);
    let engine = Engine::open(config).map_err(|err| err.to_string())?;
    let mut report = Json::obj()
        .with("data_dir", Json::str(dir.display().to_string()))
        .with("last_seq", Json::uint(engine.last_seq()))
        .with(
            "collections",
            Json::uint(
                engine
                    .try_list_collections()
                    .map_err(|err| err.to_string())?
                    .len() as u64,
            ),
        );
    if flags.has("bootstrap-model") {
        let bootstrap = preppydb_model::bootstrap(&engine).map_err(|err| err.to_string())?;
        report.set("model", bootstrap.to_json());
    }
    // Credentials are managed separately; make that explicit in the output.
    let auth_path = dir.join("auth.json");
    report.set("credentials", Json::str(auth_path.display().to_string()));
    report.set(
        "next_steps",
        Json::Array(vec![
            Json::str("preppydb token create --name admin --role admin --data-dir <PATH>"),
            Json::str("preppydb-server --data-dir <PATH>"),
        ]),
    );
    print_json(report);
    Ok(())
}

fn cmd_token(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("list");
    let flags = Flags::parse(args, 2)?;
    let dir = flags.data_dir()?;
    let auth_path = dir.join("auth.json");
    let mut auth = AuthStore::open(&auth_path).map_err(|err| err.to_string())?;
    match sub {
        "create" => {
            let name = flags.required("name")?.to_string();
            let role_text = flags.required("role")?;
            let role = Role::parse(role_text)
                .ok_or_else(|| format!("unknown role '{role_text}' (use read, write or admin)"))?;
            let expires_at = match flags.get("expires-days") {
                Some(days) => {
                    let days: i64 = days.parse().map_err(|_| "expires-days must be a number")?;
                    Some(preppydb_storage::clock::now_ms() + days * 24 * 60 * 60 * 1000)
                }
                None => None,
            };
            let (secret, record) = auth
                .create_token(&name, vec![role], expires_at)
                .map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with("id", Json::str(record.id))
                    .with("name", Json::str(record.name))
                    .with("roles", Json::Array(vec![Json::str(role.as_str())]))
                    .with("token", Json::str(secret))
                    .with(
                        "warning",
                        Json::str("store this token now; it is not retrievable later"),
                    ),
            );
            Ok(())
        }
        "list" => {
            print_json(
                Json::obj()
                    .with("tokens", auth.tokens_public())
                    .with("users", auth.users_public()),
            );
            Ok(())
        }
        "revoke" => {
            let id = flags.required("id")?;
            let removed = auth.revoke_token(id).map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with("revoked", Json::Bool(removed))
                    .with("id", Json::str(id)),
            );
            Ok(())
        }
        other => Err(format!(
            "unknown token subcommand '{other}' (create, list, revoke)"
        )),
    }
}

fn cmd_user(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("list");
    let flags = Flags::parse(args, 2)?;
    let dir = flags.data_dir()?;
    let mut auth = AuthStore::open(dir.join("auth.json")).map_err(|err| err.to_string())?;
    match sub {
        "create" => {
            let username = flags.required("username")?.to_string();
            let role_text = flags.required("role")?;
            let role = Role::parse(role_text)
                .ok_or_else(|| format!("unknown role '{role_text}' (use read, write or admin)"))?;
            let password = read_stdin_line("password: ")?;
            let confirmation = read_stdin_line("repeat password: ")?;
            if password != confirmation {
                return Err("passwords do not match".to_string());
            }
            let record = auth
                .create_user(&username, &password, vec![role])
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("user", record.public_json()).with(
                "note",
                Json::str("password stored as a PBKDF2-SHA-256 hash"),
            ));
            Ok(())
        }
        "list" => {
            print_json(Json::obj().with("users", auth.users_public()));
            Ok(())
        }
        "disable" => {
            let username = flags.required("username")?;
            auth.set_user_disabled(username, true)
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("disabled", Json::str(username)));
            Ok(())
        }
        other => Err(format!(
            "unknown user subcommand '{other}' (create, list, disable)"
        )),
    }
}

fn cmd_stats(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    print_json(engine.try_stats().map_err(|err| err.to_string())?.to_json());
    Ok(())
}

fn cmd_collections(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let metadata = engine
        .try_list_collections()
        .map_err(|err| err.to_string())?;
    let mut collections = Vec::with_capacity(metadata.len());
    for meta in metadata {
        let count = engine.count(&meta.name).map_err(|err| err.to_string())?;
        collections.push(
            Json::obj()
                .with("name", Json::str(meta.name.clone()))
                .with("documents", Json::uint(count as u64))
                .with("indexes", Json::uint(meta.indexes.len() as u64))
                .with("schema", Json::Bool(meta.schema.is_some())),
        );
    }
    print_json(Json::obj().with("collections", Json::Array(collections)));
    Ok(())
}

fn cmd_collection(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("list");
    let flags = Flags::parse(args, 2)?;
    let engine = open_engine(&flags)?;
    match sub {
        "create" => {
            let name = flags.required("name")?.to_string();
            let mut meta = CollectionMeta::new(&name, preppydb_storage::clock::now_ms());
            if let Some(path) = flags.get("schema-file") {
                meta.schema = Some(parse_json_arg(&read_file(path)?, "schema")?);
            }
            meta.max_docs = flags
                .get("max-docs")
                .and_then(|value| value.parse::<u64>().ok());
            engine
                .create_collection(meta)
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("created", Json::str(name)));
            Ok(())
        }
        "drop" => {
            let name = flags.required("name")?.to_string();
            if !flags.has("confirm") {
                return Err("refusing to drop a collection without --confirm".to_string());
            }
            engine
                .drop_collection(&name, true)
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("dropped", Json::str(name)));
            Ok(())
        }
        "info" => {
            let name = flags.required("name")?;
            let meta = engine
                .collection_meta(name)
                .map_err(|err| err.to_string())?;
            let count = engine.count(name).map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with("collection", meta.to_json())
                    .with("documents", Json::uint(count as u64)),
            );
            Ok(())
        }
        other => Err(format!("unknown collection subcommand '{other}'")),
    }
}

fn cmd_index(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("list");
    let flags = Flags::parse(args, 2)?;
    let engine = open_engine(&flags)?;
    match sub {
        "create" => {
            let collection = flags.required("collection")?.to_string();
            let name = flags.required("name")?.to_string();
            let field = flags.required("field")?.to_string();
            engine
                .create_index(
                    &collection,
                    IndexMeta {
                        name: name.clone(),
                        field,
                        unique: flags.has("unique"),
                        sparse: !flags.has("unique") || flags.has("sparse"),
                        created_at: preppydb_storage::clock::now_ms(),
                    },
                )
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("created", Json::str(name)));
            Ok(())
        }
        "drop" => {
            let collection = flags.required("collection")?;
            let name = flags.required("name")?;
            engine
                .drop_index(collection, name)
                .map_err(|err| err.to_string())?;
            print_json(Json::obj().with("dropped", Json::str(name)));
            Ok(())
        }
        other => Err(format!("unknown index subcommand '{other}'")),
    }
}

fn cmd_get(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let collection = flags.required("collection")?;
    let key = flags.required("key")?;
    match engine.get(collection, key) {
        Ok(doc) => {
            print_json(
                Json::obj()
                    .with("collection", Json::str(collection))
                    .with("key", Json::str(key))
                    .with("value", doc.value)
                    .with("version", Json::uint(doc.version))
                    .with("revision", Json::uint(doc.revision))
                    .with("created_at", Json::int(doc.created_at))
                    .with("updated_at", Json::int(doc.updated_at))
                    .with(
                        "expires_at",
                        doc.expires_at.map(Json::int).unwrap_or(Json::Null),
                    ),
            );
            Ok(())
        }
        Err(err) => Err(err.to_string()),
    }
}

fn cmd_put(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let collection = flags.required("collection")?.to_string();
    let key = flags.required("key")?.to_string();
    let value = match (flags.get("value"), flags.get("value-file")) {
        (Some(value), None) => parse_json_arg(value, "--value")?,
        (None, Some(path)) => parse_json_arg(&read_file(path)?, path)?,
        (Some(_), Some(_)) => return Err("use either --value or --value-file".to_string()),
        (None, None) => return Err("--value or --value-file is required".to_string()),
    };
    let expected_version = flags
        .get("expected-version")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--expected-version must be a number")
        })
        .transpose()?;
    let version = engine
        .put(
            &collection,
            &key,
            value,
            PutOptions {
                expected_version,
                expires_at: None,
            },
        )
        .map_err(|err| err.to_string())?;
    print_json(
        Json::obj()
            .with("collection", Json::str(collection))
            .with("key", Json::str(key))
            .with("version", Json::uint(version))
            .with("seq", Json::uint(engine.last_seq())),
    );
    Ok(())
}

fn cmd_delete(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let collection = flags.required("collection")?.to_string();
    let key = flags.required("key")?.to_string();
    let expected = flags
        .get("expected-version")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--expected-version must be a number")
        })
        .transpose()?;
    engine
        .delete(&collection, &key, expected)
        .map_err(|err| err.to_string())?;
    print_json(
        Json::obj()
            .with("deleted", Json::str(key.clone()))
            .with("collection", Json::str(collection))
            .with("seq", Json::uint(engine.last_seq())),
    );
    Ok(())
}

fn cmd_scan(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let collection = flags.required("collection")?.to_string();
    let options = ScanOptions {
        prefix: flags.get("prefix").map(|s| s.to_string()),
        limit: flags
            .get("limit")
            .and_then(|value| value.parse().ok())
            .unwrap_or(100),
        ..Default::default()
    };
    let docs = engine
        .scan(&collection, &options)
        .map_err(|err| err.to_string())?;
    let documents: Vec<Json> = docs
        .iter()
        .map(|(key, doc)| {
            Json::obj()
                .with("key", Json::str(key.clone()))
                .with("value", doc.value.clone())
                .with("version", Json::uint(doc.version))
        })
        .collect();
    print_json(
        Json::obj()
            .with("collection", Json::str(collection))
            .with("count", Json::uint(documents.len() as u64))
            .with("documents", Json::Array(documents)),
    );
    Ok(())
}

fn cmd_batch(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let path = flags.required("file")?;
    let text = read_file(path)?;
    let parsed = parse_json_arg(&text, path)?;
    let operations = parsed
        .as_array()
        .ok_or_else(|| "batch file must contain a JSON array".to_string())?;
    let mut ops = Vec::with_capacity(operations.len());
    for operation in operations {
        let collection = operation
            .get_str("collection")
            .ok_or("each operation needs 'collection'")?
            .to_string();
        let key = operation
            .get_str("key")
            .ok_or("each operation needs 'key'")?
            .to_string();
        match operation.get_str("op") {
            Some("put") => ops.push(Op::Put {
                collection,
                key,
                value: operation.get("value").cloned().unwrap_or(Json::Null),
                expires_at: operation.get("expires_at").and_then(|v| v.as_i64()),
            }),
            Some("delete") | Some("del") => ops.push(Op::Delete { collection, key }),
            Some(other) => return Err(format!("unsupported operation '{other}'")),
            None => return Err("each operation needs 'op'".to_string()),
        }
    }
    let count = engine.batch(ops).map_err(|err| err.to_string())?;
    print_json(
        Json::obj()
            .with("operations", Json::uint(count as u64))
            .with("seq", Json::uint(engine.last_seq())),
    );
    Ok(())
}

fn cmd_verify(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let report = engine.verify().map_err(|err| err.to_string())?;
    print_json(report.to_json());
    if report.ok {
        Ok(())
    } else {
        Err(format!(
            "verification found {} issue(s)",
            report.issues.len()
        ))
    }
}

fn cmd_wal(flags: &Flags) -> Result<(), String> {
    let dir = flags.data_dir()?;
    let wal_dir = dir.join("wal");
    let mut segments = Vec::new();
    for segment in wal::list_segments(&wal_dir).map_err(|err| err.to_string())? {
        let scan = wal::summarize_segment(&segment).map_err(|err| err.to_string())?;
        segments.push(
            Json::obj()
                .with(
                    "file",
                    Json::str(
                        segment
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    ),
                )
                .with("records", Json::uint(scan.record_count))
                .with("valid_bytes", Json::uint(scan.valid_len))
                .with(
                    "torn_tail",
                    scan.torn_tail.map(Json::str).unwrap_or(Json::Null),
                )
                .with(
                    "corruption",
                    scan.corruption.map(Json::str).unwrap_or(Json::Null),
                )
                .with(
                    "first_seq",
                    scan.first_seq.map(Json::uint).unwrap_or(Json::Null),
                )
                .with(
                    "last_seq",
                    scan.last_seq.map(Json::uint).unwrap_or(Json::Null),
                ),
        );
    }
    print_json(
        Json::obj()
            .with("wal_dir", Json::str(wal_dir.display().to_string()))
            .with("segments", Json::Array(segments)),
    );
    Ok(())
}

fn cmd_backup(flags: &Flags) -> Result<(), String> {
    let engine = open_engine(flags)?;
    let out = PathBuf::from(flags.required("out")?);
    let report = backup::create_backup(&engine, &out, !flags.has("no-checkpoint"))
        .map_err(|err| err.to_string())?;
    print_json(report.to_json());
    Ok(())
}

fn cmd_backup_verify(flags: &Flags) -> Result<(), String> {
    let archive = PathBuf::from(flags.required("archive")?);
    let report = backup::verify_backup(&archive).map_err(|err| err.to_string())?;
    print_json(report);
    Ok(())
}

fn cmd_restore(flags: &Flags) -> Result<(), String> {
    let archive = PathBuf::from(flags.required("archive")?);
    let target = PathBuf::from(flags.required("target")?);
    // Refuse to restore over a database that a process still has open.
    if target.join("LOCK").exists() {
        return Err(format!(
            "{} looks like a live database (LOCK file present); stop the server first",
            target.display()
        ));
    }
    let report = backup::restore_backup(&archive, &target, flags.has("force"))
        .map_err(|err| err.to_string())?;
    print_json(report.to_json());
    Ok(())
}

fn cmd_model(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("bootstrap");
    let flags = Flags::parse(args, 2)?;
    let engine = open_engine(&flags)?;
    match sub {
        "bootstrap" => {
            let report = preppydb_model::bootstrap(&engine).map_err(|err| err.to_string())?;
            print_json(report.to_json());
            Ok(())
        }
        "collections" => {
            let specs: Vec<Json> = preppydb_model::collections()
                .iter()
                .map(|spec| {
                    Json::obj()
                        .with("name", Json::str(spec.name))
                        .with("indexes", Json::uint(spec.indexes.len() as u64))
                        .with(
                            "immutable",
                            Json::Bool(preppydb_model::is_immutable_collection(spec.name)),
                        )
                })
                .collect();
            print_json(
                Json::obj()
                    .with("model_version", Json::str(preppydb_model::MODEL_VERSION))
                    .with("collections", Json::Array(specs)),
            );
            Ok(())
        }
        other => Err(format!(
            "unknown model subcommand '{other}' (bootstrap, collections)"
        )),
    }
}

fn cmd_pricing(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("simulate");
    let flags = Flags::parse(args, 2)?;
    match sub {
        "simulate" => {
            let min = flags
                .get("min")
                .ok_or("--min is required")?
                .parse::<i64>()
                .map_err(|_| "--min must be an integer (cents)")?;
            let max = flags
                .get("max")
                .ok_or("--max is required")?
                .parse::<i64>()
                .map_err(|_| "--max must be an integer (cents)")?;
            let anchor = flags
                .get("anchor")
                .ok_or("--anchor is required")?
                .parse::<i64>()
                .map_err(|_| "--anchor must be an integer (cents)")?;
            let steps = flags
                .get("steps")
                .unwrap_or("20")
                .parse::<usize>()
                .map_err(|_| "--steps must be a number")?;
            let seed = flags.get("seed").unwrap_or("preppydb-cli");
            let mut rule = PriceRule::new("simulated", min, max, anchor);
            if let Some(step) = flags.get("step-cents") {
                rule.step_cents = step.parse().map_err(|_| "--step-cents must be a number")?;
            }
            if let Some(percent) = flags.get("discount") {
                rule.discounts.push(DiscountRule {
                    label: "simulation".to_string(),
                    percent: percent.parse().map_err(|_| "--discount must be a number")?,
                    starts_at: None,
                    ends_at: None,
                });
            }
            rule.validate().map_err(|err| err.to_string())?;
            let mut engine = PricingEngine::with_seed(seed.as_bytes());
            let history = engine
                .simulate(&mut rule, steps, preppydb_storage::clock::now_ms())
                .map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with(
                        "seed_fingerprint",
                        Json::str(engine.seed_fingerprint().to_string()),
                    )
                    .with("rule", rule.to_json())
                    .with(
                        "history",
                        Json::Array(history.iter().map(|change| change.to_json()).collect()),
                    ),
            );
            Ok(())
        }
        "show" => {
            let engine = open_engine(&flags)?;
            let item = flags.required("item")?;
            match engine.get("price_rules", item) {
                Ok(doc) => {
                    print_json(doc.value);
                    Ok(())
                }
                Err(err) => Err(err.to_string()),
            }
        }
        other => Err(format!(
            "unknown pricing subcommand '{other}' (simulate, show)"
        )),
    }
}

fn cmd_audit(args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("verify");
    let flags = Flags::parse(args, 2)?;
    let dir = flags.data_dir()?;
    let path = flags
        .get("file")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("pricing-audit.log"));
    match sub {
        "verify" => {
            let log = AuditLog::open(&path).map_err(|err| err.to_string())?;
            print_json(
                Json::obj()
                    .with("file", Json::str(path.display().to_string()))
                    .with("ok", Json::Bool(true))
                    .with("entries", Json::uint(log.entry_count())),
            );
            Ok(())
        }
        "show" => {
            let log = AuditLog::open(&path).map_err(|err| err.to_string())?;
            print_json(Json::obj().with(
                "entries",
                Json::Array(log.entries().iter().map(|entry| entry.to_json()).collect()),
            ));
            Ok(())
        }
        other => Err(format!("unknown audit subcommand '{other}' (verify, show)")),
    }
}

#[allow(dead_code)]
fn ensure_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|err| format!("creating {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{cmd_health, Flags};

    #[test]
    fn health_refuses_bearer_tokens_in_process_arguments() {
        let args = vec!["--token".to_string(), "private-sentinel".to_string()];
        let flags = Flags::parse(&args, 0).unwrap();
        let error = cmd_health(&flags).unwrap_err();
        assert!(error.contains("PREPPYDB_TOKEN"));
        assert!(!error.contains("private-sentinel"));
    }
}
