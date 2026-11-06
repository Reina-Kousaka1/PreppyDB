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
use std::io::Read;
use std::path::{Path, PathBuf};

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

    fn from_json(value: &Json) -> Result<TokenRecord> {
        let id = required_string(value, "id")?;
        let name = required_string(value, "name")?;
        let hash_hex = required_string(value, "hash")?;
        let hash = hex_decode(&hash_hex)
            .map_err(|_| AuthError("auth file contains an invalid token hash".into()))?;
        if id.len() > 64 || name.trim().is_empty() || name.len() > 64 || hash.len() != 32 {
            return Err(AuthError(
                "auth file contains an invalid token record".into(),
            ));
        }
        let created_at = value
            .get_i64("created_at")
            .ok_or_else(|| AuthError("auth file contains an invalid token creation time".into()))?;
        Ok(TokenRecord {
            id,
            name,
            hash_hex,
            roles: parse_roles(value)?,
            created_at,
            expires_at: optional_i64(value, "expires_at")?,
            last_used_at: optional_i64(value, "last_used_at")?,
        })
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

    fn from_json(value: &Json) -> Result<UserRecord> {
        let iterations = match value.get("iterations") {
            None => DEFAULT_ITERATIONS as u64,
            Some(value) => value.as_u64().ok_or_else(|| {
                AuthError("auth file contains invalid PBKDF2 iteration count".into())
            })?,
        };
        if !(10_000..=1_000_000).contains(&iterations) {
            return Err(AuthError(
                "auth file contains unsupported PBKDF2 iteration count".into(),
            ));
        }
        let username = required_string(value, "username")?;
        validate_username(&username)?;
        let salt_hex = required_string(value, "salt")?;
        let salt = hex_decode(&salt_hex)
            .map_err(|_| AuthError("auth file contains an invalid password salt".into()))?;
        let hash_hex = required_string(value, "hash")?;
        let hash = hex_decode(&hash_hex)
            .map_err(|_| AuthError("auth file contains an invalid password hash".into()))?;
        if salt.len() != 16 || hash.len() != 32 {
            return Err(AuthError(
                "auth file contains an invalid password hash record".into(),
            ));
        }
        let created_at = value
            .get_i64("created_at")
            .ok_or_else(|| AuthError("auth file contains an invalid user creation time".into()))?;
        let disabled = value
            .get("disabled")
            .and_then(|item| item.as_bool())
            .ok_or_else(|| AuthError("auth file contains an invalid disabled flag".into()))?;
        Ok(UserRecord {
            username,
            salt_hex,
            iterations: iterations as u32,
            hash_hex,
            roles: parse_roles(value)?,
            created_at,
            disabled,
        })
    }
}

fn required_string(value: &Json, key: &str) -> Result<String> {
    value
        .get_str(key)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AuthError(format!("auth file is missing a valid '{key}'")))
}

fn parse_roles(value: &Json) -> Result<Vec<Role>> {
    let items = value
        .get("roles")
        .and_then(|item| item.as_array())
        .ok_or_else(|| AuthError("auth file is missing a valid roles array".into()))?;
    let mut roles = Vec::with_capacity(items.len());
    for item in items {
        let role = item
            .as_str()
            .and_then(Role::parse)
            .ok_or_else(|| AuthError("auth file contains an unknown role".into()))?;
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    if roles.is_empty() {
        return Err(AuthError("auth file contains an empty roles array".into()));
    }
    Ok(roles)
}

fn optional_i64(value: &Json, key: &str) -> Result<Option<i64>> {
    match value.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(item) => item
            .as_i64()
            .map(Some)
            .ok_or_else(|| AuthError(format!("auth file contains an invalid '{key}'"))),
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
const MAX_ACTIVE_SESSIONS: usize = 10_000;
/// Minimum password length accepted for a login user.
pub const MIN_PASSWORD_LEN: usize = 12;
/// Upper bound for credential file reads, including files that grow during reading.
pub const MAX_AUTH_FILE_BYTES: u64 = 16 * 1024 * 1024;
const DUMMY_PASSWORD_SALT: [u8; 16] = [0; 16];
const DUMMY_PASSWORD_HASH: [u8; 32] = [0; 32];

fn read_credentials(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_AUTH_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_AUTH_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "credential file exceeds size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "credential file is not valid UTF-8",
        )
    })
}

fn parse_credentials(text: &str) -> Result<(Vec<UserRecord>, Vec<TokenRecord>)> {
    let value = preppydb_json::parse_with_limit(
        text,
        preppydb_json::ParseLimits {
            max_depth: 32,
            max_string_len: MAX_AUTH_FILE_BYTES as usize,
            max_items: 200_000,
            max_bytes: MAX_AUTH_FILE_BYTES as usize,
        },
    )
    .map_err(|err| AuthError(format!("auth file is not valid JSON: {err}")))?;
    if value.as_object().is_none() {
        return Err(AuthError("auth file root must be an object".into()));
    }
    if value
        .get("format_version")
        .is_some_and(|version| version.as_i64() != Some(1))
    {
        return Err(AuthError(
            "auth file has an unsupported format version".into(),
        ));
    }
    let users = value
        .get("users")
        .map(|v| {
            v.as_array()
                .ok_or_else(|| AuthError("auth file 'users' must be an array".into()))?
                .iter()
                .map(UserRecord::from_json)
                .collect()
        })
        .transpose()?
        .unwrap_or_default();
    let tokens = value
        .get("tokens")
        .map(|v| {
            v.as_array()
                .ok_or_else(|| AuthError("auth file 'tokens' must be an array".into()))?
                .iter()
                .map(TokenRecord::from_json)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok((users, tokens))
}

/// Persistent credential store.
#[derive(Debug)]
pub struct AuthStore {
    path: PathBuf,
    users: Vec<UserRecord>,
    tokens: Vec<TokenRecord>,
    sessions: BTreeMap<String, Session>,
    loaded_digest: Option<[u8; 32]>,
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
            loaded_digest: None,
        };
        store.load()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&mut self) -> Result<()> {
        match read_credentials(&self.path) {
            Ok(text) => {
                let (users, tokens) = parse_credentials(&text)?;
                self.users = users;
                self.tokens = tokens;
                self.loaded_digest = Some(sha256(text.as_bytes()));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.users.clear();
                self.tokens.clear();
                self.loaded_digest = None;
            }
            Err(err) => return Err(AuthError(format!("reading {}: {err}", self.path.display()))),
        }
        Ok(())
    }

    /// Reload changed credentials and invalidate sessions carrying old roles.
    /// Any read or parse error must be surfaced so callers can fail closed.
    pub fn reload_if_changed(&mut self) -> Result<()> {
        let text = read_credentials(&self.path)
            .map_err(|err| AuthError(format!("reading credential file: {err}")))?;
        let digest = sha256(text.as_bytes());
        if Some(digest) != self.loaded_digest {
            // Parse the exact bytes whose digest was compared. A second file read here
            // could race an atomic replacement and install content that did not match
            // the change decision made above.
            let (users, tokens) = parse_credentials(&text)?;
            self.sessions.clear();
            self.users = users;
            self.tokens = tokens;
            self.loaded_digest = Some(digest);
        }
        Ok(())
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
        if text.len() as u64 > MAX_AUTH_FILE_BYTES {
            return Err(AuthError("credential file exceeds size limit".into()));
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| AuthError(format!("creating {}: {err}", parent.display())))?;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        match std::fs::symlink_metadata(&tmp) {
            Ok(_) => std::fs::remove_file(&tmp)
                .map_err(|err| AuthError(format!("removing stale {}: {err}", tmp.display())))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AuthError(format!("inspecting {}: {err}", tmp.display()))),
        }
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
        if let Err(error) = self.save() {
            self.users.pop();
            return Err(error);
        }
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
        let previous_salt = user.salt_hex.clone();
        let previous_hash = user.hash_hex.clone();
        let previous_iterations = user.iterations;
        user.salt_hex = hex_encode(&salt);
        user.hash_hex = hex_encode(&hash);
        user.iterations = DEFAULT_ITERATIONS;
        if let Err(error) = self.save() {
            if let Some(user) = self.users.iter_mut().find(|user| user.username == username) {
                user.salt_hex = previous_salt;
                user.hash_hex = previous_hash;
                user.iterations = previous_iterations;
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn set_user_disabled(&mut self, username: &str, disabled: bool) -> Result<()> {
        let user = self
            .users
            .iter_mut()
            .find(|user| user.username == username)
            .ok_or_else(|| AuthError(format!("user '{username}' not found")))?;
        let previous_disabled = user.disabled;
        user.disabled = disabled;
        if let Err(error) = self.save() {
            if let Some(user) = self.users.iter_mut().find(|user| user.username == username) {
                user.disabled = previous_disabled;
            }
            return Err(error);
        }
        Ok(())
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
        if let Err(error) = self.save() {
            self.tokens.pop();
            return Err(error);
        }
        Ok((secret, record))
    }

    /// Revoke a token by id. Returns whether it existed.
    pub fn revoke_token(&mut self, id: &str) -> Result<bool> {
        let previous = self.tokens.clone();
        let before = self.tokens.len();
        self.tokens.retain(|token| token.id != id);
        let removed = self.tokens.len() != before;
        if removed {
            if let Err(error) = self.save() {
                self.tokens = previous;
                return Err(error);
            }
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
        if password.chars().count() > 512 {
            return Err(AuthError("invalid credentials".to_string()));
        }
        let user = self
            .users
            .iter()
            .find(|user| user.username == username)
            .cloned();
        let (salt, iterations, expected) = match &user {
            Some(user) => {
                let salt = hex_decode(&user.salt_hex)
                    .map_err(|_| AuthError("corrupt credential".to_string()))?;
                let expected = hex_decode(&user.hash_hex)
                    .map_err(|_| AuthError("corrupt credential".to_string()))?;
                (salt, user.iterations, expected)
            }
            None => (
                DUMMY_PASSWORD_SALT.to_vec(),
                DEFAULT_ITERATIONS,
                DUMMY_PASSWORD_HASH.to_vec(),
            ),
        };
        let actual = pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, expected.len());
        let password_matches = constant_time_eq(&actual, &expected);
        let Some(user) = user else {
            return Err(AuthError("invalid credentials".to_string()));
        };
        if user.disabled || !password_matches {
            return Err(AuthError("invalid credentials".to_string()));
        }
        let now = preppydb_storage::clock::now_ms();
        self.sessions.retain(|_, session| session.expires_at > now);
        if self.sessions.len() >= MAX_ACTIVE_SESSIONS {
            return Err(AuthError("session limit reached".to_string()));
        }
        let token = os_random_str(32)?;
        self.sessions.insert(
            hex_encode(&sha256(token.as_bytes())),
            Session {
                username: user.username.clone(),
                roles: user.roles.clone(),
                expires_at: now + SESSION_TTL_MS,
            },
        );
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
        let missing_user = store.login("missing", "wrong password here").unwrap_err();
        assert!(store
            .create_user("admin", "short", vec![Role::Admin])
            .is_err());
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        let wrong_password = store.login("admin", "wrong password here").unwrap_err();
        assert_eq!(missing_user.0, wrong_password.0);
        store.set_user_disabled("admin", true).unwrap();
        let disabled_user = store.login("admin", "correct horse battery").unwrap_err();
        assert_eq!(missing_user.0, disabled_user.0);
        store.set_user_disabled("admin", false).unwrap();
        let (session, principal) = store.login("admin", "correct horse battery").unwrap();
        assert!(principal.allows(Role::Admin));
        assert!(store.authenticate_session(&session).is_some());
        assert!(store.logout(&session));
        assert!(store.authenticate_session(&session).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn login_prunes_expired_sessions_and_caps_active_sessions() {
        let (mut store, dir) = store("session-cap");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        let now = preppydb_storage::clock::now_ms();
        for index in 0..MAX_ACTIVE_SESSIONS {
            store.sessions.insert(
                format!("session-{index}"),
                Session {
                    username: "admin".into(),
                    roles: vec![Role::Admin],
                    expires_at: now + SESSION_TTL_MS,
                },
            );
        }
        assert_eq!(
            store.login("admin", "correct horse battery").unwrap_err().0,
            "session limit reached"
        );
        assert_eq!(store.sessions.len(), MAX_ACTIVE_SESSIONS);

        let expired_key = "session-0";
        store.sessions.get_mut(expired_key).unwrap().expires_at = now - 1;
        assert!(store.login("admin", "correct horse battery").is_ok());
        assert_eq!(store.sessions.len(), MAX_ACTIVE_SESSIONS);
        assert!(!store.sessions.contains_key(expired_key));
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

    #[cfg(unix)]
    #[test]
    fn credential_save_does_not_follow_a_symlinked_temporary_file() {
        use std::os::unix::fs::symlink;

        let (mut store, dir) = store("save-symlink");
        let outside_dir = std::env::temp_dir().join(format!(
            "preppydb-auth-sentinel-{}-{}",
            std::process::id(),
            preppydb_storage::clock::now_ms()
        ));
        std::fs::create_dir_all(&outside_dir).unwrap();
        let sentinel = outside_dir.join("sentinel");
        std::fs::write(&sentinel, b"preserve").unwrap();
        let temp = store.path().with_extension("json.tmp");
        symlink(&sentinel, &temp).unwrap();

        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();

        assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
        assert!(std::fs::symlink_metadata(store.path()).unwrap().is_file());
        assert!(!temp.exists());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(outside_dir).unwrap();
    }

    #[test]
    fn reload_detects_same_length_role_change_and_rejects_corruption() {
        let (mut store, dir) = store("reload-role");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        store.reload_if_changed().unwrap();
        let (session, _) = store.login("admin", "correct horse battery").unwrap();
        let original = std::fs::read_to_string(store.path()).unwrap();
        let changed = original.replace("\"admin\"", "\"write\"");
        assert_eq!(original.len(), changed.len());
        assert_ne!(original, changed);
        std::fs::write(store.path(), changed).unwrap();
        store.reload_if_changed().unwrap();
        assert!(store.authenticate_session(&session).is_none());
        std::fs::write(store.path(), "{invalid").unwrap();
        assert!(store.reload_if_changed().is_err());
        std::fs::remove_file(store.path()).unwrap();
        assert!(store.reload_if_changed().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_reload_does_not_partially_replace_active_credentials() {
        let (mut store, dir) = store("reload-atomic-failure");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        store.reload_if_changed().unwrap();
        let (session, _) = store.login("admin", "correct horse battery").unwrap();
        let original = std::fs::read(store.path()).unwrap();

        std::fs::write(store.path(), b"{malformed").unwrap();
        assert!(store.reload_if_changed().is_err());
        // The API rejects requests while reload fails. Internally, a failed parse
        // must not install a partial/empty credential set or invalidate sessions.
        assert_eq!(store.users.len(), 1);
        assert!(store.authenticate_session(&session).is_some());

        std::fs::write(store.path(), original).unwrap();
        store.reload_if_changed().unwrap();
        assert!(store.authenticate_session(&session).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_auth_persistence_does_not_publish_mutations_in_memory() {
        let (mut store, dir) = store("failed-save-rollback");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        let temp = store.path().with_extension("json.tmp");

        // A directory at the temporary-file path makes exclusive temp creation
        // fail before the credential file can be replaced.
        std::fs::create_dir(&temp).unwrap();
        assert!(store.set_user_disabled("admin", true).is_err());
        assert!(store.has_admin(), "failed save disabled admin in memory");
        std::fs::remove_dir(&temp).unwrap();

        std::fs::create_dir(&temp).unwrap();
        assert!(store
            .set_password("admin", "new correct horse battery")
            .is_err());
        std::fs::remove_dir(&temp).unwrap();
        assert!(store.login("admin", "correct horse battery").is_ok());
        assert!(store.login("admin", "new correct horse battery").is_err());

        let (secret, token) = store
            .create_token("service", vec![Role::Write], None)
            .unwrap();
        std::fs::create_dir(&temp).unwrap();
        assert!(store.revoke_token(&token.id).is_err());
        std::fs::remove_dir(&temp).unwrap();
        assert!(store.authenticate_token(&secret).is_some());

        std::fs::create_dir(&temp).unwrap();
        assert!(store
            .create_token("not-persisted", vec![Role::Read], None)
            .is_err());
        std::fs::remove_dir(&temp).unwrap();
        assert_eq!(store.tokens.len(), 1);

        std::fs::create_dir(&temp).unwrap();
        assert!(store
            .create_user("writer", "another correct horse", vec![Role::Write])
            .is_err());
        std::fs::remove_dir(&temp).unwrap();
        assert_eq!(store.user_count(), 1);
        assert!(store.users.iter().all(|user| user.username != "writer"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_credentials_fail_closed_on_open_and_reload() {
        let (mut store, dir) = store("oversized-credentials");
        let file = std::fs::File::create(store.path()).unwrap();
        file.set_len(MAX_AUTH_FILE_BYTES + 1).unwrap();
        drop(file);
        assert!(store.reload_if_changed().is_err());
        assert!(AuthStore::open(store.path()).is_err());
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

    #[test]
    fn imported_password_iterations_are_bounded_without_integer_wraparound() {
        let imported_user = |iterations| {
            Json::obj()
                .with("username", Json::str("reader"))
                .with("salt", Json::str("00000000000000000000000000000000"))
                .with("iterations", iterations)
                .with(
                    "hash",
                    Json::str("0000000000000000000000000000000000000000000000000000000000000000"),
                )
                .with("roles", Json::array(vec![Json::str("read")]))
                .with("created_at", Json::int(0))
                .with("disabled", Json::Bool(false))
        };
        for invalid in [
            Json::Null,
            Json::Bool(true),
            Json::str("120000"),
            Json::int(-1),
        ] {
            let value = imported_user(invalid);
            assert!(UserRecord::from_json(&value).is_err());
        }
        for count in [0, 1, 9_999, 1_000_001, u32::MAX as u64 + 1] {
            let value = imported_user(Json::uint(count));
            assert!(UserRecord::from_json(&value).is_err(), "accepted {count}");
        }
        for count in [10_000, DEFAULT_ITERATIONS, 1_000_000] {
            let value = imported_user(Json::uint(count as u64));
            assert_eq!(UserRecord::from_json(&value).unwrap().iterations, count);
        }
    }

    #[test]
    fn malformed_credential_records_fail_closed() {
        let (mut store, dir) = store("strict-credentials");
        store
            .create_user("admin", "correct horse battery", vec![Role::Admin])
            .unwrap();
        let original = std::fs::read_to_string(store.path()).unwrap();
        let mut value = preppydb_json::parse(&original).unwrap();
        value.get_mut("users").unwrap().as_array_mut().unwrap()[0].set(
            "roles",
            Json::array(vec![Json::str("admin"), Json::str("owner")]),
        );
        assert!(parse_credentials(&value.to_string()).is_err());

        let mut value = preppydb_json::parse(&original).unwrap();
        value.get_mut("users").unwrap().as_array_mut().unwrap()[0].remove_key("hash");
        assert!(parse_credentials(&value.to_string()).is_err());

        let mut value = preppydb_json::parse(&original).unwrap();
        value.set("format_version", Json::int(2));
        assert!(parse_credentials(&value.to_string()).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
