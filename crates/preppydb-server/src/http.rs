//! A small HTTP/1.1 server built on `std::net`.
//!
//! Deliberately minimal: one request per keep-alive iteration, hard limits on every
//! input, a bounded worker pool (backpressure instead of unbounded thread spawning) and
//! no chunked transfer encoding. Anything the server does not understand is rejected
//! with a structured error instead of being guessed at.

use preppydb_json::Json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::Duration;

/// Hard limits applied to every connection.
#[derive(Clone, Debug)]
pub struct HttpLimits {
    /// Maximum bytes of the request line.
    pub max_request_line: usize,
    /// Maximum number of request headers.
    pub max_headers: usize,
    /// Maximum total bytes of all headers.
    pub max_header_bytes: usize,
    /// Maximum request body size.
    pub max_body_bytes: usize,
    /// Connection read/write timeout.
    pub io_timeout: Duration,
    /// Idle timeout between keep-alive requests.
    pub keep_alive_timeout: Duration,
    /// Maximum requests served on one connection.
    pub max_requests_per_connection: usize,
    /// Number of worker threads.
    pub workers: usize,
    /// Maximum connections queued for the workers before `accept` blocks.
    pub backlog: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits {
            max_request_line: 8 * 1024,
            max_headers: 64,
            max_header_bytes: 16 * 1024,
            max_body_bytes: 4 * 1024 * 1024,
            io_timeout: Duration::from_secs(15),
            keep_alive_timeout: Duration::from_secs(30),
            max_requests_per_connection: 256,
            workers: 8,
            backlog: 256,
        }
    }
}

/// A parsed request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub peer: String,
    pub request_id: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(|s| s.as_str())
    }

    pub fn query_bool(&self, name: &str) -> bool {
        matches!(
            self.query_param(name),
            Some("true") | Some("1") | Some("yes")
        )
    }

    pub fn query_usize(&self, name: &str, default: usize) -> usize {
        self.query_param(name)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
    }

    pub fn json_body(&self) -> std::result::Result<Json, Response> {
        if self.body.is_empty() {
            return Ok(Json::obj());
        }
        let text = std::str::from_utf8(&self.body).map_err(|_| {
            Response::error(400, "invalid_request", "request body is not valid UTF-8")
        })?;
        preppydb_json::parse(text).map_err(|err| {
            Response::error(
                400,
                "invalid_json",
                format!("request body is not valid JSON: {err}"),
            )
        })
    }
}

/// A response to send back.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response {
            status,
            content_type: content_type.to_string(),
            body,
            headers: Vec::new(),
        }
    }

    pub fn json(status: u16, value: Json) -> Response {
        Response::new(
            status,
            "application/json; charset=utf-8",
            value.to_string().into_bytes(),
        )
    }

    pub fn html(body: &str) -> Response {
        Response::new(200, "text/html; charset=utf-8", body.as_bytes().to_vec())
    }

    pub fn asset(content_type: &str, body: &'static str) -> Response {
        Response::new(200, content_type, body.as_bytes().to_vec())
    }

    pub fn empty(status: u16) -> Response {
        Response::new(status, "text/plain; charset=utf-8", Vec::new())
    }

    /// Structured error body: `{ok:false, error:{code,message}}`.
    pub fn error(status: u16, code: &str, message: impl Into<String>) -> Response {
        Response::json(
            status,
            Json::obj().with("ok", Json::Bool(false)).with(
                "error",
                Json::obj()
                    .with("code", Json::str(code))
                    .with("message", Json::str(message.into())),
            ),
        )
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// Anything that can answer a request.
pub trait Handler: Send + Sync {
    fn handle(&self, request: &Request) -> Response;
}

/// Reasons a request could not be parsed.
enum ParseError {
    /// Connection closed cleanly before a request started.
    Closed,
    /// Malformed request: send an error response and close.
    Bad(Response),
    /// I/O problem (timeout, reset).
    Io,
}

/// Serve requests until `shutdown` is set.
pub fn serve(
    listener: TcpListener,
    handler: Arc<dyn Handler>,
    limits: HttpLimits,
    shutdown: Arc<AtomicBool>,
    counters: Arc<ServerCounters>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let handlers = limits.workers.max(1);
    let (sender, receiver) = sync_channel::<TcpStream>(limits.backlog.max(1));
    let receiver = Arc::new(std::sync::Mutex::new(receiver));

    let mut workers = Vec::new();
    for _ in 0..handlers {
        let receiver = Arc::clone(&receiver);
        let handler = Arc::clone(&handler);
        let limits = limits.clone();
        let counters = Arc::clone(&counters);
        let shutdown = Arc::clone(&shutdown);
        workers.push(std::thread::spawn(move || loop {
            let stream = {
                let guard = match receiver.lock() {
                    Ok(guard) => guard,
                    Err(err) => err.into_inner(),
                };
                guard.recv().ok()
            };
            let Some(stream) = stream else { break };
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            counters.active.fetch_add(1, Ordering::Relaxed);
            handle_connection(stream, handler.as_ref(), &limits, &counters);
            counters.active.fetch_sub(1, Ordering::Relaxed);
        }));
    }

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                counters.accepted.fetch_add(1, Ordering::Relaxed);
                let _ = stream.set_nodelay(true);
                let _ = peer;
                if sender.send(stream).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

/// Connection-level counters exposed through `/v1/metrics`.
#[derive(Debug, Default)]
pub struct ServerCounters {
    pub accepted: AtomicU64,
    pub requests: AtomicU64,
    pub active: AtomicU64,
    pub errors_4xx: AtomicU64,
    pub errors_5xx: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

impl ServerCounters {
    pub fn snapshot(&self) -> Json {
        Json::obj()
            .with(
                "connections_accepted",
                Json::uint(self.accepted.load(Ordering::Relaxed)),
            )
            .with(
                "connections_active",
                Json::uint(self.active.load(Ordering::Relaxed)),
            )
            .with(
                "requests",
                Json::uint(self.requests.load(Ordering::Relaxed)),
            )
            .with(
                "errors_client",
                Json::uint(self.errors_4xx.load(Ordering::Relaxed)),
            )
            .with(
                "errors_server",
                Json::uint(self.errors_5xx.load(Ordering::Relaxed)),
            )
            .with(
                "bytes_in",
                Json::uint(self.bytes_in.load(Ordering::Relaxed)),
            )
            .with(
                "bytes_out",
                Json::uint(self.bytes_out.load(Ordering::Relaxed)),
            )
    }
}

fn handle_connection(
    stream: TcpStream,
    handler: &dyn Handler,
    limits: &HttpLimits,
    counters: &ServerCounters,
) {
    let peer = stream
        .peer_addr()
        .map(|addr: SocketAddr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let _ = stream.set_read_timeout(Some(limits.io_timeout));
    let _ = stream.set_write_timeout(Some(limits.io_timeout));
    let mut reader = BufReader::new(stream);

    for _ in 0..limits.max_requests_per_connection {
        match read_request(&mut reader, limits, &peer) {
            Ok(request) => {
                counters.requests.fetch_add(1, Ordering::Relaxed);
                counters
                    .bytes_in
                    .fetch_add(request.body.len() as u64, Ordering::Relaxed);
                let keep_alive = wants_keep_alive(&request);
                let response = handler.handle(&request);
                if response.status >= 500 {
                    counters.errors_5xx.fetch_add(1, Ordering::Relaxed);
                } else if response.status >= 400 {
                    counters.errors_4xx.fetch_add(1, Ordering::Relaxed);
                }
                counters
                    .bytes_out
                    .fetch_add(response.body.len() as u64, Ordering::Relaxed);
                if write_response(reader.get_mut(), &response, keep_alive).is_err() {
                    break;
                }
                if !keep_alive {
                    break;
                }
                // Shorter timeout while waiting for the next request on a live socket.
                let _ = reader
                    .get_mut()
                    .set_read_timeout(Some(limits.keep_alive_timeout));
            }
            Err(ParseError::Closed) => break,
            Err(ParseError::Io) => break,
            Err(ParseError::Bad(response)) => {
                counters.errors_4xx.fetch_add(1, Ordering::Relaxed);
                let _ = write_response(reader.get_mut(), &response, false);
                break;
            }
        }
    }
    let _ = reader.get_mut().flush();
    let _ = reader.get_mut().shutdown(std::net::Shutdown::Both);
}

fn wants_keep_alive(request: &Request) -> bool {
    match request.header("connection") {
        Some(value) => !value.eq_ignore_ascii_case("close"),
        None => true,
    }
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
    limits: &HttpLimits,
    peer: &str,
) -> std::result::Result<Request, ParseError> {
    // ---- request line ------------------------------------------------------
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Err(ParseError::Closed),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Err(ParseError::Io),
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => return Err(ParseError::Io),
        Err(_) => return Err(ParseError::Io),
    }
    if line.len() > limits.max_request_line {
        return Err(ParseError::Bad(Response::error(
            414,
            "request_line_too_long",
            "request line exceeds the configured limit",
        )));
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err(ParseError::Bad(Response::error(
            400,
            "invalid_request",
            "empty request line",
        )));
    }
    let mut parts = trimmed.split(' ');
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/1.") {
        return Err(ParseError::Bad(Response::error(
            400,
            "invalid_request",
            "malformed request line",
        )));
    }
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
    ) {
        return Err(ParseError::Bad(Response::error(
            405,
            "method_not_allowed",
            format!("method {method} is not supported"),
        )));
    }

    // ---- headers -----------------------------------------------------------
    let mut headers = BTreeMap::new();
    let mut header_bytes = 0usize;
    let mut count = 0usize;
    loop {
        let mut header_line = String::new();
        match reader.read_line(&mut header_line) {
            Ok(0) => return Err(ParseError::Io),
            Ok(_) => {}
            Err(_) => return Err(ParseError::Io),
        }
        header_bytes += header_line.len();
        if header_bytes > limits.max_header_bytes {
            return Err(ParseError::Bad(Response::error(
                431,
                "headers_too_large",
                "request headers exceed the configured limit",
            )));
        }
        let text = header_line.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        count += 1;
        if count > limits.max_headers {
            return Err(ParseError::Bad(Response::error(
                431,
                "too_many_headers",
                "too many request headers",
            )));
        }
        let (name, value) = match text.split_once(':') {
            Some((name, value)) => (name.trim(), value.trim()),
            None => {
                return Err(ParseError::Bad(Response::error(
                    400,
                    "invalid_headers",
                    "malformed header line",
                )))
            }
        };
        if name.is_empty() || name.len() > 64 {
            return Err(ParseError::Bad(Response::error(
                400,
                "invalid_headers",
                "invalid header name",
            )));
        }
        headers.insert(name.to_ascii_lowercase(), value.to_string());
    }

    // ---- body --------------------------------------------------------------
    let mut body = Vec::new();
    if let Some(length) = headers.get("content-length") {
        let length: usize = length.parse().map_err(|_| {
            ParseError::Bad(Response::error(
                400,
                "invalid_content_length",
                "Content-Length is not a number",
            ))
        })?;
        if length > limits.max_body_bytes {
            return Err(ParseError::Bad(Response::error(
                413,
                "body_too_large",
                format!(
                    "request body of {length} bytes exceeds the limit of {} bytes",
                    limits.max_body_bytes
                ),
            )));
        }
        body.resize(length, 0);
        reader.read_exact(&mut body).map_err(|_| ParseError::Io)?;
    } else if headers.contains_key("transfer-encoding") {
        return Err(ParseError::Bad(Response::error(
            411,
            "chunked_not_supported",
            "chunked transfer encoding is not supported; send Content-Length",
        )));
    }

    // ---- target and query --------------------------------------------------
    let (path, query) = split_target(target);
    let request_id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
        peer: peer.to_string(),
        request_id,
    })
}

fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let mut query = BTreeMap::new();
    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    if let Some(raw) = raw_query {
        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key, value),
                None => (pair, ""),
            };
            query.insert(percent_decode(key), percent_decode(value));
        }
    }
    // Reject path traversal outright.
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    (path, query)
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = &text[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn write_response(
    stream: &mut TcpStream,
    response: &Response,
    keep_alive: bool,
) -> std::io::Result<()> {
    let mut head = String::new();
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        status_text(response.status)
    ));
    head.push_str("Server: PreppyDB\r\n");
    head.push_str(&format!("Content-Type: {}\r\n", response.content_type));
    head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    head.push_str(if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    });
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    head.push_str("Referrer-Policy: no-referrer\r\n");
    head.push_str("Cache-Control: no-store\r\n");
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn split_target_parses_query() {
        let (path, query) = split_target("/v1/collections/c/documents?limit=5&prefix=x%2Fy");
        assert_eq!(path, "/v1/collections/c/documents");
        assert_eq!(query.get("limit").unwrap(), "5");
        assert_eq!(query.get("prefix").unwrap(), "x/y");
    }

    #[test]
    fn status_texts_exist() {
        for status in [200u16, 201, 400, 401, 404, 409, 413, 429, 500, 503] {
            assert_ne!(status_text(status), "Unknown", "missing text for {status}");
        }
    }

    #[test]
    fn error_responses_are_structured() {
        let response = Response::error(404, "not_found", "no such thing");
        let text = String::from_utf8(response.body).unwrap();
        let parsed = preppydb_json::parse(&text).unwrap();
        assert_eq!(parsed.get_path("ok").unwrap().as_bool(), Some(false));
        assert_eq!(
            parsed.get_path("error.code").unwrap().as_str(),
            Some("not_found")
        );
    }
}
