//! Authentication and authorisation.
//!
//! * Passwords are stored as PBKDF2-HMAC-SHA-256 hashes (120k iterations, 16-byte salt)
//!   — never in plaintext, and never compared with `==`.
//! * API tokens are shown exactly once, at creation time. Only their SHA-256 is stored.
//! * Sessions obtained via `POST /v1/auth/login` live in memory only, so a restart
//!   invalidates them.
//! * There is no default account and no default password: [`AuthStore::open`] creates an
//!   empty store, and the server refuses to start without at least one admin credential.

use preppydb_crypto::pbkdf2::{pbkdf2_hmac_sha256, DEFAULT_ITERATIONS};
use preppydb_crypto::rng::os_random;
use preppydb_crypto::{constant_time_eq, hex_decode, hex_encode, sha256};
use preppydb_json::Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Roles, in increasing order of privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        match text.to_ascii_lowercase().as_str() {
            "read" | "reader" | "read_only" => Some(Role::Read),
            "write" | "read_write" | "writer" => Some(Role::Write),
            "admin" | "administrator" => Some(Role::Admin),
            _ => None,
        }
    }
}

/// An authenticated caller.
#[derive(Clone, Debug)]
pub struct Principal {
    pub name: String,
    pub roles: Vec<Role>,
    /// `token`, `session` or `anonymous`.
    pub kind: &'static str,
}

impl Principal {
    pub fn anonymous() -> Principal {
        Principal {
            name: "anonymous".to_string(),
            roles: Vec::new(),
            kind: "anonymous",
        }
    }

    /// True when the principal has at least `required` (roles are hierarchical).
    pub fn allows(&self, required: Role) -> bool {
        self.roles.iter().any(|role| *role >= required)
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("name", Json::str(self.name.clone()))
            .with("kind", Json::str(self.kind))
            .with(
                "roles",
                Json::Array(self.roles.iter().map(|r| Json::str(r.as_str())).collect()),
            )
    }
}

/// A stored API token (hash only).
#[derive(Clone, Debug)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub hash_hex: String,
    pub roles: Vec<Role>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

impl TokenRecord {
    fn to_json(&self) -> Json {
        Json::obj()
            .with("id", Json::str(self.id.clone()))
            .with("name", Json::str(self.name.clone()))
            .with("hash", Json::str(self.hash_hex.clone()))
            .with(
                "roles",
                Json::Array(self.roles.iter().map(|r| Json::str(r.as_str())).collect()),
            )
            .with("created_at", Json::int(self.created_at))
            .with(
                "expires_at",
                self.expires_at.map(Json::int).unwrap_or(Json::Null),
            )
            .with(
                "last_used_at",
                self.last_used_at.map(Json::int).unwrap_or(Json::Null),
            )
    }

    /// Public projection: never includes the hash.
    pub fn public_json(&self) -> Json {
        Json::obj()
            .with("id", Json::str(self.id.clone()))
            .with("name", Json::str(self.name.clone()))
            .with(
                "roles",
                Json::Array(self.roles.iter().map(|r| Json::str(r.as_str())).collect()),
            )
            .with("created_at", Json::int(self.created_at))
            .with(
                "expires_at",
                self.expires_at.map(Json::int).unwrap_or(Json::Null),
            )
            .with(
                "last_used_at",
                self.last_used_at.map(Json::int).unwrap_or(Json::Null),
            )
    }

    fn from_json(value: &Json) -> TokenRecord {
        TokenRecord {
            id: value.get_str("id").unwrap_or_default().to_string(),
            name: value.get_str("name").unwrap_or_default().to_string(),
            hash_hex: value.get_str("hash").unwrap_or_default().to_string(),
            roles: value
                .get("roles")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().and_then(Role::parse))
                        .collect()
                })
                .unwrap_or_default(),
            created_at: value.get_i64("created_at").unwrap_or(0),
            expires_at: value.get("expires_at").and_then(|v| v.as_i64()),
            last_used_at: value.get("last_used_at").and_then(|v| v.as_i64()),
        }
    }
}

/// A stored user (password hash only).
#[derive(Clone, Debug)]
pub struct UserRecord {
    pub username: String,
    pub salt_hex: String,
    pub iterations: u32,
    pub hash_hex: String,
    pub roles: Vec<Role>,
    pub created_at: i64,
    pub disabled: bool,
}

impl UserRecord {
    fn to_json(&self) -> Json {
        Json::obj()
            .with("username", Json::str(self.username.clone()))
            .with("salt", Json::str(self.salt_hex.clone()))
            .with("iterations", Json::uint(self.iterations as u64))
            .with("hash", Json::str(self.hash_hex.clone()))
            .with(
                "roles",
                Json::Array(self.roles.iter().map(|r| Json::str(r.as_str())).collect()),
            )
            .with("created_at", Json::int(self.created_at))
            .with("disabled", Json::Bool(self.disabled))
    }

    pub fn public_json(&self) -> Json {
        Json::obj()
            .with("username", Json::str(self.username.clone()))
            .with(
                "roles",
                Json::Array(self.roles.iter().map(|r| Json::str(r.as_str())).collect()),
            )
            .with("created_at", Json::int(self.created_at))
            .with("disabled", Json::Bool(self.disabled))
    }

    fn from_json(value: &Json) -> UserRecord {
        UserRecord {
            username: value.get_str("username").unwrap_or_default().to_string(),
            salt_hex: value.get_str("salt").unwrap_or_default().to_string(),
            iterations: value
                .get("iterations")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_ITERATIONS as u64) as u32,
            hash_hex: value.get_str("hash").unwrap_or_default().to_string(),
            roles: value
                .get("roles")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().and_then(Role::parse))
                        .collect()
                })
                .unwrap_or_default(),
            created_at: value.get_i64("created_at").unwrap_or(0),
            disabled: value
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }
}

/// In-memory login session.
#[derive(Clone, Debug)]
struct Session {
    username: String,
    roles: Vec<Role>,
    expires_at: i64,
}

/// Errors from the auth subsystem.
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

type Result<T> = std::result::Result<T, AuthError>;

/// How long a login session stays valid.
pub const SESSION_TTL_MS: i64 = 8 * 60 * 60 * 1000;
/// Minimum password length accepted for a login user.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Persistent credential store.
#[derive(Debug)]
pub struct AuthStore {
    path: PathBuf,
    users: Vec<UserRecord>,
    tokens: Vec<TokenRecord>,
    sessions: BTreeMap<String, Session>,
    loaded_mtime: Option<SystemTime>,
}

impl AuthStore {
    /// Open (or create) the credential file.
    pub fn open(path: impl Into<PathBuf>) -> Result<AuthStore> {
        let path = path.into();
        let mut store = AuthStore {
            path,
            users: Vec::new(),
            tokens: Vec::new(),
            sessions: BTreeMap::new(),
            loaded_mtime: None,
        };
        store.load()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&mut self) -> Result<()> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let value = preppydb_json::parse(&text)
                    .map_err(|err| AuthError(format!("auth file is not valid JSON: {err}")))?;
                self.users = value
                    .get("users")
                    .and_then(|v| v.as_array())
                    .map(|items| items.iter().map(UserRecord::from_json).collect())
                    .unwrap_or_default();
                self.tokens = value
                    .get("tokens")
                    .and_then(|v| v.as_array())
                    .map(|items| items.iter().map(TokenRecord::from_json).collect())
                    .unwrap_or_default();
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.users.clear();
                self.tokens.clear();
            }
            Err(err) => return Err(AuthError(format!("reading {}: {err}", self.path.display()))),
        }
        self.loaded_mtime = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(())
    }

    /// Re-read the file when another process (the CLI) changed it.
    pub fn reload_if_changed(&mut self) {
        let mtime = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok());
        if mtime != self.loaded_mtime && mtime.is_some() {
            let _ = self.load();
        }
    }

    fn save(&self) -> Result<()> {
        let payload = Json::obj()
            .with("format_version", Json::int(1))
            .with(
                "users",
                Json::Array(self.users.iter().map(|user| user.to_json()).collect()),
            )
            .with(
                "tokens",
                Json::Array(self.tokens.iter().map(|token| token.to_json()).collect()),
            );
        let text = preppydb_json::to_string_pretty(&payload);
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| AuthError(format!("creating {}: {err}", parent.display())))?;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        preppydb_storage::lock::write_private(&tmp, text.as_bytes())
            .map_err(|err| AuthError(err.to_string()))?;
        preppydb_storage::fsutil::replace_file(&tmp, &self.path)
            .map_err(|err| AuthError(format!("replacing auth file: {err}")))?;
        Ok(())
    }

    // ------------------------------------------------------------------ queries

    pub fn user_count(&self) -> usize {
        self.users.iter().filter(|user| !user.disabled).count()
    }

    pub fn has_admin(&self) -> bool {
        self.users
            .iter()
            .any(|user| !user.disabled && user.roles.contains(&Role::Admin))
    }

    /// Users, without password material.
    pub fn users_public(&self) -> Json {
        Json::Array(self.users.iter().map(|user| user.public_json()).collect())
    }

    /// Tokens, without hashes.
    pub fn tokens_public(&self) -> Json {
        Json::Array(
            self.tokens
                .iter()
                .map(|token| token.public_json())
                .collect(),
        )
    }

    // ------------------------------------------------------------------ users

    /// Create a login user. The plaintext password is hashed and discarded.
    pub fn create_user(
        &mut self,
        username: &str,
        password: &str,
        roles: Vec<Role>,
    ) -> Result<UserRecord> {
        validate_username(username)?;
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AuthError(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        if roles.is_empty() {
            return Err(AuthError("a user needs at least one role".to_string()));
        }
        if self.users.iter().any(|user| user.username == username) {
            return Err(AuthError(format!("user '{username}' already exists")));
        }
        let mut salt = [0u8; 16];
        os_random(&mut salt).map_err(|err| AuthError(err.to_string()))?;
        let hash = pbkdf2_hmac_sha256(password.as_bytes(), &salt, DEFAULT_ITERATIONS, 32);
        let record = UserRecord {
            username: username.to_string(),
            salt_hex: hex_encode(&salt),
            iterations: DEFAULT_ITERATIONS,
            hash_hex: hex_encode(&hash),
            roles,
            created_at: preppydb_storage::clock::now_ms(),
            disabled: false,
        };
        self.users.push(record.clone());
        self.save()?;
        Ok(record)
    }

    /// Replace a user's password.
    pub fn set_password(&mut self, username: &str, password: &str) -> Result<()> {
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AuthError(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        let mut salt = [0u8; 16];
        os_random(&mut salt).map_err(|err| AuthError(err.to_string()))?;
        let hash = pbkdf2_hmac_sha256(password.as_bytes(), &salt, DEFAULT_ITERATIONS, 32);
        let user = self
            .users
            .iter_mut()
            .find(|user| user.username == username)
            .ok_or_else(|| AuthError(format!("user '{username}' not found")))?;
        user.salt_hex = hex_encode(&salt);
        user.hash_hex = hex_encode(&hash);
        user.iterations = DEFAULT_ITERATIONS;
        self.save()
    }

    pub fn set_user_disabled(&mut self, username: &str, disabled: bool) -> Result<()> {
        let user = self
            .users
            .iter_mut()
            .find(|user| user.username == username)
            .ok_or_else(|| AuthError(format!("user '{username}' not found")))?;
        user.disabled = disabled;
        self.save()
    }

    // ------------------------------------------------------------------ tokens

    /// Create an API token. The plaintext value is returned exactly once.
    pub fn create_token(
        &mut self,
        name: &str,
        roles: Vec<Role>,
        expires_at: Option<i64>,
    ) -> Result<(String, TokenRecord)> {
        if name.trim().is_empty() || name.len() > 64 {
            return Err(AuthError(
                "token name must be 1..=64 characters".to_string(),
            ));
        }
        if roles.is_empty() {
            return Err(AuthError("a token needs at least one role".to_string()));
        }
        if self.tokens.len() >= 1000 {
            return Err(AuthError("token limit reached (1000)".to_string()));
        }
        let secret = os_random_str(32)?;
        let id = format!("tok_{}", os_random_str(8)?);
        let record = TokenRecord {
            id: id.clone(),
            name: name.to_string(),
            hash_hex: hex_encode(&sha256(secret.as_bytes())),
            roles,
            created_at: preppydb_storage::clock::now_ms(),
            expires_at,
            last_used_at: None,
        };
        self.tokens.push(record.clone());
        self.save()?;
        Ok((secret, record))
    }

    /// Revoke a token by id. Returns whether it existed.
    pub fn revoke_token(&mut self, id: &str) -> Result<bool> {
        let before = self.tokens.len();
        self.tokens.retain(|token| token.id != id);
        let removed = self.tokens.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    // ------------------------------------------------------------------ auth

    /// Verify a bearer token.
    pub fn authenticate_token(&mut self, token: &str) -> Option<Principal> {
        if token.len() < 8 || token.len() > 512 {
            return None;
        }
        let digest = hex_encode(&sha256(token.as_bytes()));
        let now = preppydb_storage::clock::now_ms();
        let mut found: Option<Principal> = None;
        for record in self.tokens.iter_mut() {
            if constant_time_eq(record.hash_hex.as_bytes(), digest.as_bytes()) {
                if let Some(expires_at) = record.expires_at {
                    if now > expires_at {
                        return None;
                    }
                }
                record.last_used_at = Some(now);
                found = Some(Principal {
                    name: record.name.clone(),
                    roles: record.roles.clone(),
                    kind: "token",
                });
                break;
            }
        }
        found
    }

    /// Verify a session token issued by [`AuthStore::login`].
    pub fn authenticate_session(&self, token: &str) -> Option<Principal> {
        let digest = hex_encode(&sha256(token.as_bytes()));
        let session = self.sessions.get(&digest)?;
        if preppydb_storage::clock::now_ms() > session.expires_at {
            return None;
        }
        Some(Principal {
            name: session.username.clone(),
            roles: session.roles.clone(),
            kind: "session",
        })
    }

    /// Username/password login; returns a session token valid for [`SESSION_TTL_MS`].
    pub fn login(&mut self, username: &str, password: &str) -> Result<(String, Principal)> {
        let user = self
            .users
            .iter()
            .find(|user| user.username == username)
            .ok_or_else(|| AuthError("invalid credentials".to_string()))?
            .clone();
        if user.disabled {
            return Err(AuthError("account is disabled".to_string()));
        }
        if password.chars().count() > 512 {
            return Err(AuthError("invalid credentials".to_string()));
        }
        let salt =
            hex_decode(&user.salt_hex).map_err(|_| AuthError("corrupt credential".to_string()))?;
        let expected =
            hex_decode(&user.hash_hex).map_err(|_| AuthError("corrupt credential".to_string()))?;
        let actual =
            pbkdf2_hmac_sha256(password.as_bytes(), &salt, user.iterations, expected.len());
        if !constant_time_eq(&actual, &expected) {
            return Err(AuthError("invalid credentials".to_string()));
        }
        let token = os_random_str(32)?;
        let now = preppydb_storage::clock::now_ms();
        self.sessions.insert(
            hex_encode(&sha256(token.as_bytes())),
            Session {
                username: user.username.clone(),
                roles: user.roles.clone(),
                expires_at: now + SESSION_TTL_MS,
            },
        );
        // Keep the session table bounded.
        if self.sessions.len() > 10_000 {
            self.sessions.retain(|_, session| session.expires_at > now);
        }
        Ok((
            token,
            Principal {
                name: user.username,
                roles: user.roles,
                kind: "session",
            },
        ))
    }

    /// Invalidate a session token.
    pub fn logout(&mut self, token: &str) -> bool {
        self.sessions
            .remove(&hex_encode(&sha256(token.as_bytes())))
            .is_some()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

fn validate_username(username: &str) -> Result<()> {
    if username.len() < 3 || username.len() > 64 {
        return Err(AuthError("username must be 3..=64 characters".to_string()));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AuthError(
            "username may only contain letters, digits, '_', '-' and '.'".to_string(),
        ));
    }
    Ok(())
}

fn os_random_str(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    os_random(&mut buffer).map_err(|err| AuthError(err.to_string()))?;
    Ok(preppydb_crypto::base64_url_encode(&buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (AuthStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-auth-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        (AuthStore::open(&path).unwrap(), dir)
    }

    #[test]
    fn tokens_authenticate_and_revoke() {
        let (mut store, dir) = store("tokens");
        let (secret, record) = store
            .create_token("deploy", vec![Role::Admin], None)
            .unwrap();
        let principal = store.authenticate_token(&secret).unwrap();
        assert!(principal.allows(Role::Admin));
        assert_eq!(principal.kind, "token");
        assert!(store.authenticate_token("wrong-token-value").is_none());
        // The hash is never part of the public projection.
        let public = store.tokens_public().to_string();
        assert!(!public.contains(&record.hash_hex));
        assert!(public.contains(&record.id));
        assert!(store.revoke_token(&record.id).unwrap());
        assert!(store.authenticate_token(&secret).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let (mut store, dir) = store("expiry");
        let (secret, _) = store
            .create_token(
                "old",
                vec![Role::Read],
                Some(preppydb_storage::clock::now_ms() - 1),
            )
            .unwrap();
        assert!(store.authenticate_token(&secret).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn role_hierarchy() {
        let admin = Principal {
            name: "a".into(),
            roles: vec![Role::Admin],
            kind: "token",
        };
        let writer = Principal {
            name: "w".into(),
            roles: vec![Role::Write],
            kind: "token",
        };
        let reader = Principal {
            name: "r".into(),
            roles: vec![Role::Read],
            kind: "token",
        };
        assert!(admin.allows(Role::Read) && admin.allows(Role::Write) && admin.allows(Role::Admin));
        assert!(writer.allows(Role::Write) && !writer.allows(Role::Admin));
        assert!(reader.allows(Role::Read) && !reader.allows(Role::Write));
        assert!(!Principal::anonymous().allows(Role::Read));
    }

    #[test]
    fn password_login_and_weak_password_rejection() {
        let (mut store, dir) = store("login");
        assert!(store
            .create_user("admin", "short", vec![Role::Admin])
            .is_err());
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        assert!(store.login("admin", "wrong password here").is_err());
        let (session, principal) = store.login("admin", "correct horse battery").unwrap();
        assert!(principal.allows(Role::Admin));
        assert!(store.authenticate_session(&session).is_some());
        assert!(store.logout(&session));
        assert!(store.authenticate_session(&session).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credentials_persist_without_plaintext() {
        let (mut store, dir) = store("persist");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        let (secret, _) = store.create_token("ci", vec![Role::Write], None).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            !text.contains("correct horse battery"),
            "password leaked into the file"
        );
        assert!(!text.contains(&secret), "token leaked into the file");
        let mut reopened = AuthStore::open(store.path()).unwrap();
        assert!(reopened.authenticate_token(&secret).is_some());
        assert!(reopened.login("admin", "correct horse battery").is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn no_default_credentials() {
        let (store, dir) = store("defaults");
        assert_eq!(store.user_count(), 0);
        assert!(!store.has_admin());
        assert_eq!(store.tokens_public().len(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn usernames_are_validated() {
        let (mut store, dir) = store("names");
        assert!(store
            .create_user("ab", "correct horse battery", vec![Role::Read])
            .is_err());
        assert!(store
            .create_user("bad name", "correct horse battery", vec![Role::Read])
            .is_err());
        assert!(store
            .create_user("good.name", "correct horse battery", vec![Role::Read])
            .is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
