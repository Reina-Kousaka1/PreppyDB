//! Error types for the storage engine.
//!
//! Every error carries a stable machine-readable `code` so the HTTP layer and the
//! clients can react without string matching.

use std::fmt;

/// Stable error codes exposed through the API (`error.code`).
pub mod codes {
    pub const NOT_FOUND: &str = "not_found";
    pub const COLLECTION_NOT_FOUND: &str = "collection_not_found";
    pub const COLLECTION_EXISTS: &str = "collection_exists";
    pub const INDEX_EXISTS: &str = "index_exists";
    pub const INDEX_NOT_FOUND: &str = "index_not_found";
    pub const CONFLICT: &str = "conflict";
    pub const VALIDATION_FAILED: &str = "validation_failed";
    pub const LIMIT_EXCEEDED: &str = "limit_exceeded";
    pub const CORRUPT_DATA: &str = "corrupt_data";
    pub const IO_ERROR: &str = "io_error";
    pub const CONFIG_ERROR: &str = "config_error";
    pub const LOCKED: &str = "database_locked";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const INTERNAL: &str = "internal";
    pub const READ_ONLY: &str = "read_only";
}

/// Storage engine error.
#[derive(Debug)]
pub enum DbError {
    NotFound {
        collection: String,
        key: String,
    },
    CollectionNotFound(String),
    CollectionExists(String),
    IndexExists {
        collection: String,
        index: String,
    },
    IndexNotFound {
        collection: String,
        index: String,
    },
    /// Optimistic concurrency check failed for a document.
    Conflict {
        collection: String,
        key: String,
        expected: Option<u64>,
        actual: Option<u64>,
    },
    /// Optimistic concurrency check failed for a whole transaction (read set changed).
    TransactionConflict {
        details: Vec<String>,
    },
    Validation(String),
    LimitExceeded {
        limit: &'static str,
        max: u64,
        actual: u64,
    },
    Corrupt {
        context: String,
        detail: String,
    },
    Io {
        context: String,
        detail: String,
    },
    Config(String),
    Locked(String),
    ReadOnly(String),
    Unsupported(String),
    Internal(String),
}

impl DbError {
    pub fn code(&self) -> &'static str {
        match self {
            DbError::NotFound { .. } => codes::NOT_FOUND,
            DbError::CollectionNotFound(_) => codes::COLLECTION_NOT_FOUND,
            DbError::CollectionExists(_) => codes::COLLECTION_EXISTS,
            DbError::IndexExists { .. } => codes::INDEX_EXISTS,
            DbError::IndexNotFound { .. } => codes::INDEX_NOT_FOUND,
            DbError::Conflict { .. } | DbError::TransactionConflict { .. } => codes::CONFLICT,
            DbError::Validation(_) => codes::VALIDATION_FAILED,
            DbError::LimitExceeded { .. } => codes::LIMIT_EXCEEDED,
            DbError::Corrupt { .. } => codes::CORRUPT_DATA,
            DbError::Io { .. } => codes::IO_ERROR,
            DbError::Config(_) => codes::CONFIG_ERROR,
            DbError::Locked(_) => codes::LOCKED,
            DbError::ReadOnly(_) => codes::READ_ONLY,
            DbError::Unsupported(_) => codes::UNSUPPORTED,
            DbError::Internal(_) => codes::INTERNAL,
        }
    }

    /// Whether retrying the same request later could succeed (used by clients).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DbError::Conflict { .. } | DbError::TransactionConflict { .. } | DbError::Locked(_)
        )
    }

    pub fn validation(message: impl Into<String>) -> DbError {
        DbError::Validation(message.into())
    }

    pub fn io(context: impl Into<String>, error: std::io::Error) -> DbError {
        DbError::Io {
            context: context.into(),
            detail: error.to_string(),
        }
    }

    pub fn corrupt(context: impl Into<String>, detail: impl Into<String>) -> DbError {
        DbError::Corrupt {
            context: context.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::NotFound { collection, key } => {
                write!(f, "document '{key}' not found in collection '{collection}'")
            }
            DbError::CollectionNotFound(name) => write!(f, "collection '{name}' not found"),
            DbError::CollectionExists(name) => write!(f, "collection '{name}' already exists"),
            DbError::IndexExists { collection, index } => {
                write!(
                    f,
                    "index '{index}' already exists on collection '{collection}'"
                )
            }
            DbError::IndexNotFound { collection, index } => {
                write!(f, "index '{index}' not found on collection '{collection}'")
            }
            DbError::Conflict {
                collection,
                key,
                expected,
                actual,
            } => write!(
                f,
                "version conflict on '{collection}/{key}': expected {}, found {}",
                expected
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "any".into()),
                actual
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "absent".into())
            ),
            DbError::TransactionConflict { details } => {
                write!(f, "transaction conflict: {}", details.join("; "))
            }
            DbError::Validation(message) => write!(f, "invalid document: {message}"),
            DbError::LimitExceeded { limit, max, actual } => {
                write!(f, "limit '{limit}' exceeded: {actual} > {max}")
            }
            DbError::Corrupt { context, detail } => {
                write!(f, "data corruption detected ({context}): {detail}")
            }
            DbError::Io { context, detail } => write!(f, "I/O error ({context}): {detail}"),
            DbError::Config(message) => write!(f, "configuration error: {message}"),
            DbError::Locked(message) => write!(f, "database is locked: {message}"),
            DbError::ReadOnly(message) => write!(f, "database is read-only: {message}"),
            DbError::Unsupported(message) => write!(f, "unsupported operation: {message}"),
            DbError::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(error: std::io::Error) -> Self {
        DbError::Io {
            context: "filesystem".to_string(),
            detail: error.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, DbError>;
