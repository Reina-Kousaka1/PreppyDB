//! PreppyDB server library: HTTP transport, authentication, configuration and API.
//!
//! The `preppydb-server` binary is a thin wrapper around this crate, and
//! `preppydb-cli` reuses the credential store so that tokens created by the CLI are
//! immediately valid for a running server (the server reloads the file when its
//! modification time changes).

#![forbid(unsafe_code)]

pub mod api;
pub mod auth;
pub mod config;
pub mod http;
pub mod ratelimit;

pub use api::{ApiState, API_VERSION};
pub use auth::{AuthStore, Principal, Role};
pub use config::ServerConfig;
pub use http::{Handler, HttpLimits, Request, Response, ServerCounters};
