//! A strict, bounded HTTP/1.1 client over `std::net` (T-12: no production DTO or client).
//!
//! One request per connection (`Connection: close`), no redirect following (a `307` is a
//! response the tests assert on), and hard bounds on header size, body size and time.

use std::fmt::{self, Write as _};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::target::BaseUrl;

/// Largest response head accepted.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Largest response body accepted (a 64 KiB value escapes to at most 6x as JSON, plus a page).
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Connect, read and write timeout.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Method, e.g. `GET`.
    pub method: String,
    /// Origin-form target, path and query, sent byte for byte.
    pub target: String,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Body bytes; sent with `Content-Length`.
    pub body: Vec<u8>,
}

impl Request {
    /// A request without a body.
    #[must_use]
    pub fn new(method: &str, target: &str) -> Self {
        Self {
            method: method.to_owned(),
            target: target.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds a JSON body (`Content-Type: application/json`).
    #[must_use]
    pub fn json(mut self, body: &str) -> Self {
        self.headers
            .push(("Content-Type".to_owned(), "application/json".to_owned()));
        self.body = body.as_bytes().to_vec();
        self
    }
}

/// A response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Headers in arrival order, names as sent.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// The first header with this name (ASCII case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON.
    ///
    /// # Errors
    /// [`HttpError::Malformed`] when it is not one JSON value.
    pub fn json(&self) -> Result<Value, HttpError> {
        serde_json::from_slice(&self.body)
            .map_err(|err| HttpError::Malformed(format!("body is not JSON: {err}")))
    }
}

/// Why an exchange failed. Every variant fails the test (P10); none is retried here.
#[derive(Debug)]
pub enum HttpError {
    /// Socket error, including timeouts.
    Io(io::Error),
    /// The peer's bytes are not a response this client accepts.
    Malformed(String),
    /// A size bound was exceeded.
    TooLarge(&'static str),
    /// The base URL's host did not resolve.
    Unresolvable(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Malformed(why) => write!(f, "malformed response: {why}"),
            Self::TooLarge(what) => write!(f, "{what} exceeds its bound"),
            Self::Unresolvable(host) => write!(f, "cannot resolve {host}"),
        }
    }
}

impl From<io::Error> for HttpError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Sends `request` to `base` and reads the whole response.
///
/// # Errors
/// [`HttpError`] for connection failure, timeout, a malformed or oversized response.
pub fn send(base: &BaseUrl, request: &Request) -> Result<Response, HttpError> {
    let address = (base.host(), base.port())
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| HttpError::Unresolvable(base.to_string()))?;
    let mut stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&encode(base, request))?;
    stream.flush()?;
    read_response(&mut stream)
}

fn encode(base: &BaseUrl, request: &Request) -> Vec<u8> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: application/json\r\n",
        request.method,
        request.target,
        base.host(),
        base.port()
    );
    for (name, value) in &request.headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if !request.body.is_empty() || request.method != "GET" {
        let _ = write!(head, "Content-Length: {}\r\n", request.body.len());
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&request.body);
    bytes
}

/// Reads and parses one response from `source` (until the peer closes).
///
/// # Errors
/// As [`send`].
pub fn read_response(source: &mut impl Read) -> Result<Response, HttpError> {
    let mut raw = Vec::new();
    let limit = (MAX_HEAD_BYTES + MAX_BODY_BYTES) as u64 + 1;
    source.take(limit).read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Response, HttpError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Malformed("no end of headers".to_owned()))?;
    if split > MAX_HEAD_BYTES {
        return Err(HttpError::TooLarge("response head"));
    }
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| HttpError::Malformed("head is not UTF-8".to_owned()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let status = parts
        .next()
        .and_then(|s| (s.len() == 3).then(|| s.parse::<u16>().ok()).flatten())
        .filter(|s| (100..600).contains(s));
    let (true, Some(status)) = (version == "HTTP/1.1" || version == "HTTP/1.0", status) else {
        return Err(HttpError::Malformed(format!(
            "bad status line {status_line:?}"
        )));
    };
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .filter(|(n, _)| !n.is_empty() && !n.contains(char::is_whitespace))
            .ok_or_else(|| HttpError::Malformed(format!("bad header line {line:?}")))?;
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    let rest = &raw[split + 4..];
    response.body = decode_body(&response, rest)?;
    if response.body.len() > MAX_BODY_BYTES {
        return Err(HttpError::TooLarge("response body"));
    }
    Ok(response)
}

fn decode_body(response: &Response, rest: &[u8]) -> Result<Vec<u8>, HttpError> {
    if response
        .header("Transfer-Encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        return decode_chunked(rest);
    }
    match response.header("Content-Length") {
        Some(length) => {
            let length: usize = length
                .parse()
                .map_err(|_| HttpError::Malformed(format!("bad Content-Length {length:?}")))?;
            if length > MAX_BODY_BYTES {
                return Err(HttpError::TooLarge("response body"));
            }
            if rest.len() < length {
                return Err(HttpError::Malformed(format!(
                    "body truncated: {} of {length} bytes",
                    rest.len()
                )));
            }
            Ok(rest[..length].to_vec())
        }
        None => Ok(rest.to_vec()),
    }
}

fn decode_chunked(mut rest: &[u8]) -> Result<Vec<u8>, HttpError> {
    let bad = |why: &str| HttpError::Malformed(format!("chunked body: {why}"));
    let mut body = Vec::new();
    loop {
        let eol = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| bad("no chunk size line"))?;
        let size_text = std::str::from_utf8(&rest[..eol]).map_err(|_| bad("size not UTF-8"))?;
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|_| bad("size not hex"))?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge("response body"));
        }
        if rest.len() < size + 2 || &rest[size..size + 2] != b"\r\n" {
            return Err(bad("chunk truncated"));
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn parse(text: &str) -> Result<Response, HttpError> {
        parse_response(text.as_bytes())
    }

    #[test]
    fn parses_a_content_length_response() {
        let r = parse("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://n2:1/v1/keys/a\r\nContent-Length: 2\r\n\r\n{}extra").unwrap();
        assert_eq!(r.status, 307);
        assert_eq!(r.header("location"), Some("http://n2:1/v1/keys/a"));
        assert_eq!(
            r.body, b"{}",
            "rule: only Content-Length bytes are the body"
        );
        assert_eq!(r.json().unwrap(), serde_json::json!({}));
    }

    #[test]
    fn parses_chunked_and_close_delimited_bodies() {
        let r = parse(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.body, b"abcde");
        let r = parse("HTTP/1.0 200 OK\r\n\r\nall of it").unwrap();
        assert_eq!(r.body, b"all of it");
    }

    #[test]
    fn rejects_malformed_responses() {
        for (text, what) in [
            ("", "empty"),
            ("HTTP/1.1 200 OK\r\n", "no end of headers"),
            ("HTTP/2 200 OK\r\n\r\n", "wrong version"),
            ("HTTP/1.1 20 OK\r\n\r\n", "short status"),
            ("HTTP/1.1 999 X\r\n\r\n", "status out of range"),
            (
                "HTTP/1.1 200 OK\r\nbad header\r\n\r\n",
                "header without colon",
            ),
            ("HTTP/1.1 200 OK\r\n: v\r\n\r\n", "empty header name"),
            ("HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n", "bad length"),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab",
                "truncated body",
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                "bad chunk size",
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab\r\n",
                "short chunk",
            ),
        ] {
            assert!(
                parse(text).is_err(),
                "rule: {what} is rejected; text={text:?}"
            );
        }
    }

    #[test]
    fn enforces_size_bounds() {
        let huge = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(
            matches!(parse(&huge), Err(HttpError::TooLarge(_))),
            "rule: declared body bound"
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        assert!(
            matches!(parse(&head), Err(HttpError::TooLarge(_))),
            "rule: head bound"
        );
    }

    #[test]
    fn round_trips_against_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut seen = vec![0u8; 4096];
            let n = conn.read(&mut seen).unwrap();
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nCache-Control: no-store\r\n\r\n{\"ok\":true}").unwrap();
            String::from_utf8(seen[..n].to_vec()).unwrap()
        });
        let base = BaseUrl::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let reply = send(
            &base,
            &Request::new("PUT", "/v1/keys/a%2Fb").json("{\"value\":\"x\"}"),
        )
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("Cache-Control"), Some("no-store"));
        let sent = server.join().unwrap();
        assert!(
            sent.starts_with("PUT /v1/keys/a%2Fb HTTP/1.1\r\n"),
            "sent={sent:?}"
        );
        assert!(
            sent.contains("Content-Length: 13\r\n") && sent.ends_with("{\"value\":\"x\"}"),
            "sent={sent:?}"
        );
    }

    #[test]
    fn a_dead_port_is_an_error_not_a_pass() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let base = BaseUrl::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        assert!(send(&base, &Request::new("GET", "/")).is_err());
    }
}
