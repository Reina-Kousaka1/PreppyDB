//! PreppyDB HTTP server binary.
//!
//! Usage:
//!
//! ```text
//! preppydb-server --data-dir /var/lib/preppydb [--config /etc/preppydb.json]
//!                 [--listen 127.0.0.1:7070] [--allow-public-bind]
//!                 [--read-only] [--init-admin USER] [--bootstrap-token NAME]
//! ```
//!
//! The server refuses to start unless at least one credential exists. There is no
//! default password and no anonymous administrative access.

#![forbid(unsafe_code)]

use preppydb_server::api::ApiState;
use preppydb_server::auth::{AuthStore, Role};
use preppydb_server::config::{ResourceProfile, ServerConfig};
use preppydb_server::http::{self, HttpLimits, ServerCounters};
use preppydb_storage::engine::Engine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    if let Err(message) = run() {
        eprintln!("preppydb-server: {message}");
        std::process::exit(1);
    }
}

struct Args {
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    profile: Option<ResourceProfile>,
    listen: Option<String>,
    allow_public_bind: bool,
    read_only: bool,
    init_admin: Option<String>,
    bootstrap_token: Option<String>,
    print_config: bool,
    init_only: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        data_dir: None,
        config_path: None,
        profile: None,
        listen: None,
        allow_public_bind: false,
        read_only: false,
        init_admin: None,
        bootstrap_token: None,
        print_config: false,
        init_only: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("preppydb-server {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--data-dir" => {
                args.data_dir = Some(PathBuf::from(
                    iter.next().ok_or("--data-dir needs a value")?,
                ))
            }
            "--config" => {
                args.config_path = Some(PathBuf::from(iter.next().ok_or("--config needs a value")?))
            }
            "--profile" => {
                let value = iter.next().ok_or("--profile needs a value")?;
                args.profile = Some(ResourceProfile::parse(&value)?);
            }
            "--listen" => args.listen = Some(iter.next().ok_or("--listen needs a value")?),
            "--allow-public-bind" => args.allow_public_bind = true,
            "--read-only" => args.read_only = true,
            "--init-admin" => {
                args.init_admin = Some(iter.next().ok_or("--init-admin needs a username")?)
            }
            "--bootstrap-token" => {
                args.bootstrap_token = Some(iter.next().ok_or("--bootstrap-token needs a name")?)
            }
            "--print-config" => args.print_config = true,
            "--init-only" => args.init_only = true,
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        r#"PreppyDB server {version}

USAGE:
    preppydb-server --data-dir <PATH> [OPTIONS]

OPTIONS:
    --data-dir <PATH>            database directory (required unless in the config)
    --config <FILE>              JSON configuration file
    --profile <NAME>             resource preset: standard or tiny
    --listen <ADDR>              bind address (default 127.0.0.1:7070)
    --allow-public-bind          permit binding to a non-loopback address
    --read-only                  open the database read-only
    --init-admin <USER>          create an admin user; password from $PREPPYDB_ADMIN_PASSWORD
    --bootstrap-token <NAME>     create an admin API token and print it once
    --print-config               print the effective configuration and exit
    --init-only                  create credentials and exit without serving
    -h, --help                   show this help
    -V, --version                print the version

ENVIRONMENT:
    PREPPYDB_ADMIN_PASSWORD      password used with --init-admin (min. 12 characters)

The server needs at least one admin credential. Create it with --init-admin and
PREPPYDB_ADMIN_PASSWORD, or bootstrap an API token with --bootstrap-token.
"#,
        version = env!("CARGO_PKG_VERSION")
    );
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    // ---- configuration -----------------------------------------------------
    let mut config = match &args.config_path {
        Some(path) => ServerConfig::load(path)?,
        None => {
            let data_dir = args
                .data_dir
                .clone()
                .ok_or("--data-dir is required (or provide --config)")?;
            ServerConfig::new(data_dir)
        }
    };
    if let Some(data_dir) = args.data_dir.clone() {
        config.data_path = data_dir;
    }
    if let Some(profile) = args.profile {
        config.apply_profile(profile);
    }
    if let Some(listen) = args.listen.clone() {
        config.listen = listen;
    }
    if args.allow_public_bind {
        config.allow_public_bind = true;
    }
    if args.read_only {
        config.read_only = true;
        config.ttl_sweep_interval_ms = None;
    }
    let problems = config.validate();
    if !problems.is_empty() {
        return Err(format!(
            "invalid configuration:\n  - {}",
            problems.join("\n  - ")
        ));
    }
    if args.print_config {
        println!("{}", preppydb_json::to_string_pretty(&config.to_json()));
        return Ok(());
    }

    // ---- credentials -------------------------------------------------------
    let auth_path = config.auth_file();
    let mut auth = AuthStore::open(&auth_path).map_err(|err| err.to_string())?;
    if let Some(username) = &args.init_admin {
        let password = std::env::var("PREPPYDB_ADMIN_PASSWORD").map_err(|_| {
            "--init-admin requires PREPPYDB_ADMIN_PASSWORD to be set (never pass passwords on the command line)"
                .to_string()
        })?;
        match auth.create_user(username, &password, vec![Role::Admin]) {
            Ok(_) => eprintln!("created admin user '{username}'"),
            Err(err) => return Err(format!("could not create admin user: {err}")),
        }
    }
    if let Some(name) = &args.bootstrap_token {
        let (secret, record) = auth
            .create_token(name, vec![Role::Admin], None)
            .map_err(|err| err.to_string())?;
        println!("created admin token '{}' (id {}):", record.name, record.id);
        println!("{secret}");
        println!("Store it now: this value is not shown again.");
    }
    if args.init_only {
        // Credential management only: create what was requested and exit.
        println!("credentials written to {}", auth_path.display());
        return Ok(());
    }
    if !auth.has_admin() && !has_admin_token(&auth) {
        return Err(format!(
            "no admin credentials found in {}\n\
             create one of:\n  \
             PREPPYDB_ADMIN_PASSWORD=... preppydb-server --data-dir <PATH> --init-admin admin\n  \
             preppydb-server --data-dir <PATH> --bootstrap-token bootstrap\n\
             (PreppyDB ships no default password.)",
            auth_path.display()
        ));
    }

    // ---- storage -----------------------------------------------------------
    let engine = Engine::open(config.engine_config())
        .map_err(|err| format!("could not open the database: {err}"))?;
    for note in engine.recovery_notes() {
        eprintln!("recovery note [{}]: {}", note.kind, note.detail);
    }

    // ---- HTTP --------------------------------------------------------------
    let counters = Arc::new(ServerCounters::default());
    let state = Arc::new(ApiState::new(
        engine,
        config.clone(),
        auth,
        Arc::clone(&counters),
    ));
    if let Some(error) = state.audit_startup_error() {
        eprintln!(
            "pricing audit log initialization failed ({error}); pricing mutations will fail closed"
        );
    }
    let listener = std::net::TcpListener::bind(&config.listen)
        .map_err(|err| format!("cannot bind {}: {err}", config.listen))?;
    let bound = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| config.listen.clone());

    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_shutdown = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        signal_shutdown.store(true, Ordering::SeqCst);
    })
    .map_err(|err| format!("cannot install shutdown signal handler: {err}"))?;

    let limits = HttpLimits {
        max_body_bytes: config.max_body_bytes,
        workers: config.workers,
        backlog: config.backlog,
        ..Default::default()
    };

    // Background TTL sweeper: expired documents are removed with an explicit,
    // logged `ttl_expiry` batch so the removal is auditable.
    let sweeper = config.ttl_sweep_interval_ms.map(|interval| {
        let state = Arc::clone(&state);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                std::thread::park_timeout(std::time::Duration::from_millis(interval.max(100)));
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match state.engine.sweep_expired() {
                    Ok(0) => {}
                    Ok(removed) => eprintln!("ttl sweeper removed {removed} expired document(s)"),
                    Err(err) => eprintln!("ttl sweeper error: {err}"),
                }
            }
        })
    });

    println!(
        "PreppyDB {} listening on http://{bound}",
        env!("CARGO_PKG_VERSION")
    );
    println!("  data directory : {}", config.data_path.display());
    println!("  credentials    : {}", auth_path.display());
    println!("  sync mode      : {}", config.sync_mode.as_str());
    println!("  read only      : {}", config.read_only);
    if !config.listen.starts_with("127.") {
        println!(
            "  NOTE: bound to a non-loopback address without TLS; terminate TLS in a reverse proxy"
        );
    }

    let result = http::serve(
        listener,
        state.clone(),
        limits,
        Arc::clone(&shutdown),
        Arc::clone(&counters),
    );

    shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = sweeper {
        handle.thread().unpark();
        let _ = handle.join();
    }
    let _ = state.engine.shutdown();
    println!("PreppyDB stopped.");
    result.map_err(|err| format!("server error: {err}"))
}

fn has_admin_token(auth: &AuthStore) -> bool {
    auth.tokens_public()
        .as_array()
        .map(|tokens| {
            tokens.iter().any(|token| {
                token
                    .get("roles")
                    .and_then(|roles| roles.as_array())
                    .map(|roles| roles.iter().any(|role| role.as_str() == Some("admin")))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
