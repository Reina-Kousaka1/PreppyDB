//! Rust client for the PreppyDB HTTP API.
//!
//! Deliberately dependency-free and blocking: one TCP connection per request with
//! explicit timeouts. HTTPS is **not** implemented (there is no TLS stack in this
//! workspace), so the client refuses `https://` URLs with a clear error instead of
//! silently sending credentials in plaintext to an unexpected place.
//!
//! ```no_run
//! use preppydb_client::Client;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::new("http://127.0.0.1:7070", "token-here")?;
//! client.put_document("people", "molly", &preppydb_json::Json::int(1))?;
//! let doc = client.get_document("people", "molly")?;
//! # Ok(())
//! # }
//! ```

use preppydb_json::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Client errors.
#[derive(Debug)]
pub enum ClientError {
    /// The server returned a structured error (`error.code`).
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// Transport problem (DNS, connect, timeout, protocol).
    Transport(String),
    /// The response was not valid JSON or not shaped like the API.
    Protocol(String),
    /// The base URL is invalid or uses an unsupported scheme.
    Configuration(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Api {
                status,
                code,
                message,
            } => {
                write!(f, "API error {status} ({code}): {message}")
            }
            ClientError::Transport(message) => write!(f, "transport error: {message}"),
            ClientError::Protocol(message) => write!(f, "protocol error: {message}"),
            ClientError::Configuration(message) => write!(f, "configuration error: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl ClientError {
    /// Whether the same request could succeed later.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Api { status, code, .. } => {
                *status == 429 || *status == 503 || code == "conflict"
            }
            ClientError::Transport(_) => true,
            _ => false,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            ClientError::Api { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

const MAX_RESPONSE_HEADERS: usize = 16 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

fn bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|err| ClientError::Transport(format!("reading response header: {err}")))?;
    if bytes.len() > limit || !bytes.ends_with(b"\n") {
        return Err(ClientError::Protocol(
            "response header too large or incomplete".into(),
        ));
    }
    if !bytes.ends_with(b"\r\n")
        || bytes[..bytes.len() - 2]
            .iter()
            .any(|byte| *byte == b'\r' || *byte == b'\n')
    {
        return Err(ClientError::Protocol(
            "response headers must use canonical CRLF lines".into(),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| ClientError::Protocol("response header is not UTF-8".into()))
}

fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A blocking HTTP client for one PreppyDB server.
#[derive(Clone, Debug)]
pub struct Client {
    host: String,
    port: u16,
    /// Base path prefix (usually empty; useful when mounted below a sub-path).
    prefix: String,
    token: Option<String>,
    timeout: Duration,
    max_response_bytes: usize,
}

impl Client {
    /// Build a client for `base_url` such as `http://127.0.0.1:7070`.
    pub fn new(base_url: &str, token: &str) -> Result<Client> {
        let mut client = Client::from_url(base_url)?;
        client.token = Some(token.to_string());
        Ok(client)
    }

    /// Build a client without credentials (only health/version endpoints work).
    pub fn anonymous(base_url: &str) -> Result<Client> {
        Client::from_url(base_url)
    }

    fn from_url(base_url: &str) -> Result<Client> {
        let trimmed = base_url.trim_end_matches('/');
        if trimmed.starts_with("https://") {
            return Err(ClientError::Configuration(
                "https is not supported by this client (no TLS stack); use an http:// URL to a local reverse proxy or run with TLS termination in front".to_string(),
            ));
        }
        let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
            ClientError::Configuration(format!(
                "base URL must start with http:// (got '{base_url}')"
            ))
        })?;
        let (authority, prefix) = match rest.find('/') {
            Some(index) => (&rest[..index], rest[index..].to_string()),
            None => (rest, String::new()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port.parse().map_err(|_| {
                    ClientError::Configuration(format!("invalid port in '{base_url}'"))
                })?;
                (host.to_string(), port)
            }
            None => (authority.to_string(), 80),
        };
        if host.is_empty() {
            return Err(ClientError::Configuration(
                "base URL has no host".to_string(),
            ));
        }
        Ok(Client {
            host,
            port,
            prefix,
            token: None,
            timeout: Duration::from_secs(10),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Client {
        self.timeout = timeout;
        self
    }

    /// Set the maximum accepted response body size in bytes.
    pub fn with_max_response_bytes(mut self, limit: usize) -> Client {
        self.max_response_bytes = limit;
        self
    }

    pub fn set_token(&mut self, token: &str) {
        self.token = Some(token.to_string());
    }

    // ------------------------------------------------------------------ transport

    fn request(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json> {
        let payload = match body {
            Some(body) => body.to_string(),
            None => String::new(),
        };
        let address = format!("{}:{}", self.host, self.port);
        let mut addresses = address
            .to_socket_addrs()
            .map_err(|err| ClientError::Transport(format!("resolving {address}: {err}")))?;
        let socket = addresses
            .next()
            .ok_or_else(|| ClientError::Transport(format!("{address} did not resolve")))?;
        let mut stream = TcpStream::connect_timeout(&socket, self.timeout)
            .map_err(|err| ClientError::Transport(format!("connecting to {address}: {err}")))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|err| ClientError::Transport(err.to_string()))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|err| ClientError::Transport(err.to_string()))?;

        let mut head = format!(
            "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: preppydb-client/{}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.prefix,
            self.host,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(token) = &self.token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|_| stream.write_all(payload.as_bytes()))
            .and_then(|_| stream.flush())
            .map_err(|err| ClientError::Transport(format!("sending request: {err}")))?;

        let mut reader = BufReader::new(stream);
        let status_line = bounded_line(&mut reader, MAX_RESPONSE_HEADERS)?;
        let status_text = status_line
            .strip_suffix("\r\n")
            .ok_or_else(|| ClientError::Protocol("malformed response status line".into()))?;
        let mut status_parts = status_text.splitn(3, ' ');
        let version = status_parts.next().unwrap_or_default();
        let status_code = status_parts.next().unwrap_or_default();
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || status_code.len() != 3
            || !status_code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(ClientError::Protocol(
                "malformed response status line".into(),
            ));
        }
        let status: u16 = status_code
            .parse()
            .map_err(|_| ClientError::Protocol("invalid response status".into()))?;

        let mut content_length: Option<usize> = None;
        let mut header_bytes = status_line.len();
        loop {
            let line = bounded_line(
                &mut reader,
                MAX_RESPONSE_HEADERS.saturating_sub(header_bytes),
            )?;
            header_bytes += line.len();
            let text = line
                .strip_suffix("\r\n")
                .ok_or_else(|| ClientError::Protocol("malformed response header".into()))?;
            if text.is_empty() {
                break;
            }
            let (name, value) = text
                .split_once(':')
                .ok_or_else(|| ClientError::Protocol("malformed response header".into()))?;
            if !valid_header_name(name) {
                return Err(ClientError::Protocol("invalid response header name".into()));
            }
            if value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
            {
                return Err(ClientError::Protocol(
                    "invalid response header value".into(),
                ));
            }
            let name = name.to_ascii_lowercase();
            let value = value.trim_matches([' ', '\t']);
            if name == "content-length" {
                if content_length.is_some() {
                    return Err(ClientError::Protocol("duplicate Content-Length".into()));
                }
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ClientError::Protocol("invalid Content-Length".into()));
                }
                content_length = Some(
                    value
                        .parse()
                        .map_err(|_| ClientError::Protocol("invalid Content-Length".into()))?,
                );
            } else if name == "transfer-encoding" {
                return Err(ClientError::Protocol(
                    "Transfer-Encoding is unsupported".into(),
                ));
            }
        }
        let length = content_length
            .ok_or_else(|| ClientError::Protocol("response has no Content-Length".into()))?;
        if length > self.max_response_bytes {
            return Err(ClientError::Protocol(
                "response body exceeds configured limit".into(),
            ));
        }
        let mut raw = vec![0u8; length];
        reader
            .read_exact(&mut raw)
            .map_err(|err| ClientError::Transport(format!("reading body: {err}")))?;
        let text = String::from_utf8(raw)
            .map_err(|_| ClientError::Protocol("response body is not UTF-8".into()))?;
        let value = if text.trim().is_empty() {
            Json::obj()
        } else {
            preppydb_json::parse(&text)
                .map_err(|err| ClientError::Protocol(format!("response is not JSON: {err}")))?
        };

        if status >= 400 {
            let code = value
                .get_path("error.code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error")
                .to_string();
            let message = value
                .get_path("error.message")
                .and_then(|v| v.as_str())
                .or_else(|| value.get_str("error"))
                .unwrap_or("request failed")
                .to_string();
            return Err(ClientError::Api {
                status,
                code,
                message,
            });
        }
        Ok(value)
    }

    fn data(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json> {
        let response = self.request(method, path, body)?;
        Ok(response.get("data").cloned().unwrap_or(Json::Null))
    }

    fn encode(segment: &str) -> String {
        let mut out = String::with_capacity(segment.len());
        for byte in segment.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*byte as char)
                }
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }

    // ------------------------------------------------------------------ endpoints

    pub fn health(&self) -> Result<Json> {
        self.request("GET", "/healthz", None)
    }

    pub fn ready(&self) -> Result<Json> {
        self.request("GET", "/readyz", None)
    }

    pub fn version(&self) -> Result<Json> {
        self.data("GET", "/v1/version", None)
    }

    pub fn whoami(&self) -> Result<Json> {
        self.data("GET", "/v1/auth/whoami", None)
    }

    /// Username/password login; sets the session token on this client.
    pub fn login(&mut self, username: &str, password: &str) -> Result<Json> {
        let body = Json::obj()
            .with("username", Json::str(username))
            .with("password", Json::str(password));
        let data = self.data("POST", "/v1/auth/login", Some(&body))?;
        if let Some(token) = data.get_str("token") {
            self.token = Some(token.to_string());
        }
        Ok(data)
    }

    pub fn stats(&self) -> Result<Json> {
        self.data("GET", "/v1/stats", None)
    }

    pub fn metrics(&self) -> Result<Json> {
        self.data("GET", "/v1/metrics", None)
    }

    pub fn list_collections(&self) -> Result<Vec<String>> {
        let data = self.data("GET", "/v1/collections", None)?;
        Ok(data
            .get("collections")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get_str("name").map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn create_collection(&self, name: &str, schema: Option<Json>) -> Result<()> {
        let mut body = Json::obj().with("name", Json::str(name));
        if let Some(schema) = schema {
            body.set("schema", schema);
        }
        self.data("POST", "/v1/collections", Some(&body))?;
        Ok(())
    }

    pub fn drop_collection(&self, name: &str) -> Result<()> {
        self.data(
            "DELETE",
            &format!("/v1/collections/{}?confirm=true", Self::encode(name)),
            None,
        )?;
        Ok(())
    }

    pub fn collection_info(&self, name: &str) -> Result<Json> {
        self.data(
            "GET",
            &format!("/v1/collections/{}", Self::encode(name)),
            None,
        )
    }

    pub fn create_index(
        &self,
        collection: &str,
        name: &str,
        field: &str,
        unique: bool,
    ) -> Result<()> {
        let body = Json::obj()
            .with("name", Json::str(name))
            .with("field", Json::str(field))
            .with("unique", Json::Bool(unique));
        self.data(
            "POST",
            &format!("/v1/collections/{}/indexes", Self::encode(collection)),
            Some(&body),
        )?;
        Ok(())
    }

    pub fn put_document(&self, collection: &str, key: &str, value: &Json) -> Result<u64> {
        let body = Json::obj()
            .with("key", Json::str(key))
            .with("value", value.clone());
        let data = self.data(
            "POST",
            &format!("/v1/collections/{}/documents", Self::encode(collection)),
            Some(&body),
        )?;
        Ok(data.get("version").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    pub fn put_document_checked(
        &self,
        collection: &str,
        key: &str,
        value: &Json,
        expected_version: Option<u64>,
    ) -> Result<u64> {
        let mut body = Json::obj().with("value", value.clone());
        if let Some(expected) = expected_version {
            body.set("expected_version", Json::uint(expected));
        }
        let data = self.data(
            "PUT",
            &format!(
                "/v1/collections/{}/documents/{}",
                Self::encode(collection),
                Self::encode(key)
            ),
            Some(&body),
        )?;
        Ok(data.get("version").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    /// Fetch a document envelope (`{key, value, version, ...}`).
    pub fn get_document(&self, collection: &str, key: &str) -> Result<Json> {
        self.data(
            "GET",
            &format!(
                "/v1/collections/{}/documents/{}",
                Self::encode(collection),
                Self::encode(key)
            ),
            None,
        )
    }

    pub fn delete_document(&self, collection: &str, key: &str) -> Result<()> {
        self.data(
            "DELETE",
            &format!(
                "/v1/collections/{}/documents/{}",
                Self::encode(collection),
                Self::encode(key)
            ),
            None,
        )?;
        Ok(())
    }

    pub fn scan(&self, collection: &str, prefix: Option<&str>, limit: usize) -> Result<Vec<Json>> {
        let mut path = format!(
            "/v1/collections/{}/documents?limit={}",
            Self::encode(collection),
            limit
        );
        if let Some(prefix) = prefix {
            path.push_str(&format!("&prefix={}", Self::encode(prefix)));
        }
        let data = self.data("GET", &path, None)?;
        Ok(data
            .get("documents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Atomic batch of `put`/`delete` operations with optional compare-and-set.
    pub fn batch(&self, collection: &str, operations: Json) -> Result<Json> {
        self.data(
            "POST",
            &format!("/v1/collections/{}/batch", Self::encode(collection)),
            Some(&Json::obj().with("operations", operations)),
        )
    }

    pub fn find_by_index(&self, collection: &str, index: &str, value: &Json) -> Result<Vec<Json>> {
        let path = format!(
            "/v1/collections/{}/indexes/{}?value={}",
            Self::encode(collection),
            Self::encode(index),
            Self::encode(&value.to_string())
        );
        let data = self.data("GET", &path, None)?;
        Ok(data
            .get("documents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub fn checkpoint(&self) -> Result<Json> {
        self.data("POST", "/v1/admin/checkpoint", None)
    }

    pub fn compact(&self) -> Result<Json> {
        self.data("POST", "/v1/admin/compact", None)
    }

    pub fn verify(&self) -> Result<Json> {
        // `/v1/admin/verify` answers 409 when it finds issues; report the body instead
        // of turning a successful verification *run* into a transport error.
        match self.request("POST", "/v1/admin/verify", None) {
            Ok(value) => Ok(value.get("data").cloned().unwrap_or(value)),
            Err(ClientError::Api { status, code, .. }) if status == 409 && code == "conflict" => {
                Ok(Json::obj().with("ok", Json::Bool(false)))
            }
            Err(err) => Err(err),
        }
    }

    pub fn backup(&self, path: &str) -> Result<Json> {
        self.data(
            "POST",
            "/v1/admin/backup",
            Some(&Json::obj().with("path", Json::str(path))),
        )
    }

    pub fn recovery(&self) -> Result<Json> {
        self.data("GET", "/v1/admin/recovery", None)
    }

    pub fn bootstrap_model(&self) -> Result<Json> {
        self.data("POST", "/v1/personality/bootstrap", None)
    }

    pub fn personality_overview(&self) -> Result<Json> {
        self.data("GET", "/v1/personality/overview", None)
    }

    pub fn personality_profiles(&self) -> Result<Vec<Json>> {
        let data = self.data("GET", "/v1/personality/profiles", None)?;
        Ok(data
            .get("profiles")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub fn pricing_rules(&self) -> Result<Vec<Json>> {
        let data = self.data("GET", "/v1/pricing/rules", None)?;
        Ok(data
            .get("rules")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub fn put_price_rule(&self, rule: &Json) -> Result<Json> {
        let item = rule
            .get_str("item_id")
            .ok_or_else(|| ClientError::Configuration("rule needs an item_id".into()))?;
        self.data(
            "PUT",
            &format!("/v1/pricing/rules/{}", Self::encode(item)),
            Some(rule),
        )
    }

    pub fn tick_price(&self, item: &str, steps: u32) -> Result<Json> {
        self.data(
            "POST",
            &format!("/v1/pricing/rules/{}/tick", Self::encode(item)),
            Some(&Json::obj().with("steps", Json::uint(steps as u64))),
        )
    }

    pub fn create_offer(&self, item: &str, ttl_ms: Option<i64>) -> Result<Json> {
        let mut body = Json::obj().with("item_id", Json::str(item));
        if let Some(ttl) = ttl_ms {
            body.set("ttl_ms", Json::int(ttl));
        }
        self.data("POST", "/v1/pricing/offers", Some(&body))
    }

    pub fn purchase(&self, offer_id: &str, quantity: u32) -> Result<Json> {
        let body = Json::obj()
            .with("offer_id", Json::str(offer_id))
            .with("quantity", Json::uint(quantity as u64));
        self.data("POST", "/v1/pricing/purchases", Some(&body))
    }

    pub fn price_history(&self, item: &str) -> Result<Vec<Json>> {
        let data = self.data(
            "GET",
            &format!("/v1/pricing/history/{}", Self::encode(item)),
            None,
        )?;
        Ok(data
            .get("history")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub fn simulate(&self, rule: &Json, steps: u32, seed: &str) -> Result<Json> {
        let body = Json::obj()
            .with("rule", rule.clone())
            .with("steps", Json::uint(steps as u64))
            .with("seed", Json::str(seed));
        self.data("POST", "/v1/pricing/simulate", Some(&body))
    }

    pub fn audit_verify(&self) -> Result<Json> {
        self.data("GET", "/v1/pricing/audit/verify", None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_from_local_server(response: Vec<u8>, limit: usize) -> Result<Json> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        });
        let result = Client::anonymous(&format!("http://{address}"))
            .unwrap()
            .with_max_response_bytes(limit)
            .request("GET", "/", None);
        server.join().unwrap();
        result
    }

    #[test]
    fn rejects_unbounded_or_ambiguous_server_responses() {
        let mut oversized_header = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        oversized_header.extend(vec![b'a'; MAX_RESPONSE_HEADERS + 1]);
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999999\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nX: incomplete".to_vec(),
            b"HTTP/1.1 200 OK\nContent-Length: 0\n\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length : 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: +0\r\n\r\n".to_vec(),
            b"HTTP/9.9 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nX: bad\x01value\r\nContent-Length: 0\r\n\r\n".to_vec(),
            oversized_header,
        ] {
            assert!(matches!(
                response_from_local_server(response, 16),
                Err(ClientError::Protocol(_))
            ));
        }
        assert!(response_from_local_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            2
        )
        .is_ok());
    }

    #[test]
    fn url_parsing() {
        let client = Client::new("http://127.0.0.1:7070", "t").unwrap();
        assert_eq!(client.port, 7070);
        assert_eq!(client.host, "127.0.0.1");
        assert_eq!(client.prefix, "");
        let client = Client::new("http://example.org/api/", "t").unwrap();
        assert_eq!(client.port, 80);
        assert_eq!(client.prefix, "/api");
    }

    #[test]
    fn https_is_rejected_with_a_clear_message() {
        let err = Client::anonymous("https://example.org").unwrap_err();
        assert!(matches!(err, ClientError::Configuration(_)));
        assert!(err.to_string().contains("https is not supported"));
    }

    #[test]
    fn bad_urls_are_rejected() {
        assert!(Client::anonymous("ftp://x").is_err());
        assert!(Client::anonymous("http://").is_err());
        assert!(Client::anonymous("http://host:notaport").is_err());
    }

    #[test]
    fn segment_encoding() {
        assert_eq!(Client::encode("plain"), "plain");
        assert_eq!(Client::encode("a b/c"), "a%20b%2Fc");
        assert_eq!(Client::encode("emoji-✓"), "emoji-%E2%9C%93");
    }

    #[test]
    fn error_helpers() {
        let error = ClientError::Api {
            status: 409,
            code: "conflict".into(),
            message: "stale".into(),
        };
        assert!(error.is_retryable());
        assert_eq!(error.status(), Some(409));
        assert_eq!(error.code(), Some("conflict"));
        let transport = ClientError::Transport("down".into());
        assert!(transport.is_retryable());
        assert_eq!(transport.status(), None);
    }
}
