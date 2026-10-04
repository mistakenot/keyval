//! HTTP/1.1 over `std::io` for the client API (PLAN.md §7.12, §8.1; std-only by the owner's
//! decision on W18.3, after the model server's precedent).
//!
//! - A request head may take at most [`MAX_HEAD_BYTES`]. Lines end in CRLF.
//! - A body is read by `Content-Length` up to the §7.11 HTTP body limit,
//!   [`MAX_BODY_BYTES`]. A longer one is not read: the request answers `413 too_large`
//!   and the connection closes. Chunked bodies are refused.
//! - Keep-alive is the default, `Connection: close` and HTTP/1.0 close, and
//!   `Expect: 100-continue` gets its interim response.
//! - Every response is JSON with `Cache-Control: no-store` (§8.1 "No caching").

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use serde_json::Value;

/// The most bytes a request head (request line and headers) may take.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The most bytes a request body may take: §7.11's HTTP request body limit, 512 KiB.
pub const MAX_BODY_BYTES: usize = 512 * 1024;

/// A request's body, as far as it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// The body's bytes (empty when the request has none).
    Bytes(Vec<u8>),
    /// A `Content-Length` over [`MAX_BODY_BYTES`]: not read, so the connection closes after
    /// the response.
    TooLarge,
}

/// One parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The method, e.g. `GET`.
    pub method: String,
    /// The path, before any `?`.
    pub path: String,
    /// The raw query string after `?`, if any.
    pub query: Option<String>,
    /// The headers in order, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Body,
    /// Whether the connection closes after this request's response.
    pub close: bool,
}

impl Request {
    /// Every value of the header field `name` (lower-case).
    #[must_use]
    pub fn field(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Why a request could not be read; the connection closes after the answer, if any.
#[derive(Debug)]
pub enum ReadError {
    /// The peer closed the connection before a new request began.
    Closed,
    /// The request is not well-formed HTTP/1.1 (`400 invalid_request`).
    Malformed(String),
    /// The connection failed or timed out.
    Io(io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("connection closed"),
            Self::Malformed(why) => write!(f, "malformed request: {why}"),
            Self::Io(error) => write!(f, "connection error: {error}"),
        }
    }
}

/// One response: a status, a JSON body and the headers some statuses need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The JSON body.
    pub body: Value,
    /// A `Location` header (a 307 redirect).
    pub location: Option<String>,
    /// An `Allow` header (a 405).
    pub allow: Option<&'static str>,
    /// Whether to close the connection after this response.
    pub close: bool,
}

impl Response {
    /// A response with `status` and `body` and no extra headers.
    #[must_use]
    pub const fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            location: None,
            allow: None,
            close: false,
        }
    }
}

/// Reads one request from `reader`. `continue_to` receives the interim `100 Continue` that a
/// client which sent `Expect: 100-continue` waits for before its body.
///
/// # Errors
///
/// [`ReadError`] when the connection closed, failed, or carried a malformed request.
pub fn read_request(
    reader: &mut impl BufRead,
    continue_to: &mut impl Write,
) -> Result<Request, ReadError> {
    let head = read_head(reader)?;
    let text = String::from_utf8(head)
        .map_err(|_| ReadError::Malformed("the head is not UTF-8".to_owned()))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Malformed(format!(
            "bad request line {request_line:?}"
        )));
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        other => {
            return Err(ReadError::Malformed(format!(
                "unsupported version {other:?}"
            )));
        }
    };
    if method.is_empty() || !target.starts_with('/') {
        return Err(ReadError::Malformed(format!(
            "bad request line {request_line:?}"
        )));
    }
    let mut headers = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadError::Malformed(format!("bad header line {line:?}")));
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ReadError::Malformed(format!("bad header name {name:?}")));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    let mut request = Request {
        method: method.to_owned(),
        path,
        query,
        headers,
        body: Body::Bytes(Vec::new()),
        close: http10,
    };
    if request
        .field("connection")
        .iter()
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("close"))
    {
        request.close = true;
    }
    if !request.field("transfer-encoding").is_empty() {
        return Err(ReadError::Malformed(
            "chunked request bodies are not supported; send Content-Length".to_owned(),
        ));
    }
    let length = match request.field("content-length")[..] {
        [] => 0,
        [value] => Some(value)
            .filter(|value| !value.is_empty() && value.bytes().all(|digit| digit.is_ascii_digit()))
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| ReadError::Malformed(format!("bad Content-Length {value:?}")))?,
        _ => {
            return Err(ReadError::Malformed(
                "Content-Length is repeated".to_owned(),
            ));
        }
    };
    if length > MAX_BODY_BYTES {
        request.body = Body::TooLarge;
        request.close = true;
        return Ok(request);
    }
    if length > 0
        && request
            .field("expect")
            .iter()
            .any(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        continue_to
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .and_then(|()| continue_to.flush())
            .map_err(ReadError::Io)?;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(ReadError::Io)?;
    request.body = Body::Bytes(body);
    Ok(request)
}

/// Reads up to and including the blank line that ends a request head.
fn read_head(reader: &mut impl BufRead) -> Result<Vec<u8>, ReadError> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let read = reader
            .by_ref()
            .take(u64::try_from(MAX_HEAD_BYTES + 1 - before).unwrap_or(0))
            .read_until(b'\n', &mut head)
            .map_err(ReadError::Io)?;
        if read == 0 {
            return if head.is_empty() {
                Err(ReadError::Closed)
            } else {
                Err(ReadError::Malformed(
                    "the connection closed inside a request head".to_owned(),
                ))
            };
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(ReadError::Malformed(format!(
                "the request head exceeds {MAX_HEAD_BYTES} bytes"
            )));
        }
        if head.ends_with(b"\n") && !head.ends_with(b"\r\n") {
            return Err(ReadError::Malformed(
                "request lines end in CRLF, not a bare LF".to_owned(),
            ));
        }
        if head.ends_with(b"\r\n\r\n") {
            head.truncate(head.len() - 4);
            return Ok(head);
        }
        if head == b"\r\n" {
            // Tolerate a stray empty line between requests (RFC 9112 §2.2).
            head.clear();
        }
    }
}

/// The reason phrase of a status code this server uses.
const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        507 => "Insufficient Storage",
        _ => "Status",
    }
}

/// Writes `response` with `Content-Type: application/json`, its length and
/// `Cache-Control: no-store`.
///
/// # Errors
///
/// The connection's error.
pub fn write_response(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    let body = response.body.to_string();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\n",
        response.status,
        reason(response.status),
        body.len()
    );
    if let Some(location) = &response.location {
        head.push_str("Location: ");
        head.push_str(location);
        head.push_str("\r\n");
    }
    if let Some(allow) = response.allow {
        head.push_str("Allow: ");
        head.push_str(allow);
        head.push_str("\r\n");
    }
    if response.close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::{
        Body, MAX_BODY_BYTES, MAX_HEAD_BYTES, ReadError, Response, read_request, write_response,
    };

    fn read(raw: &[u8]) -> (Result<super::Request, ReadError>, Vec<u8>) {
        let mut continued = Vec::new();
        let result = read_request(&mut Cursor::new(raw.to_vec()), &mut continued);
        (result, continued)
    }

    fn malformed(raw: &[u8]) -> bool {
        match read(raw).0 {
            Err(ReadError::Malformed(_)) => true,
            Ok(_) | Err(ReadError::Closed | ReadError::Io(_)) => false,
        }
    }

    #[test]
    fn a_request_is_read_with_its_query_headers_and_body() {
        let raw = b"PUT /v1/kv?key=a HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                    Expect: 100-continue\r\nContent-Length: 13\r\n\r\n{\"value\":\"v\"}";
        let (request, continued) = read(raw);
        let request = request.unwrap();
        assert_eq!(
            (
                request.method.as_str(),
                request.path.as_str(),
                request.query.as_deref(),
                request.field("content-type"),
                request.body.clone(),
                request.close,
                continued
            ),
            (
                "PUT",
                "/v1/kv",
                Some("key=a"),
                vec!["application/json"],
                Body::Bytes(b"{\"value\":\"v\"}".to_vec()),
                false,
                b"HTTP/1.1 100 Continue\r\n\r\n".to_vec()
            ),
            "rule: the method, path, raw query, headers (lower-cased names) and the body by \
             Content-Length are read; Expect: 100-continue gets its interim response; the \
             connection stays open"
        );
    }

    #[test]
    fn oversized_heads_and_bodies_and_chunked_bodies_are_refused() {
        let long_head = format!(
            "GET /v1/kv?key={} HTTP/1.1\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        let too_large = format!(
            "PUT /v1/kv?key=a HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let (large, _) = read(too_large.as_bytes());
        let large = large.unwrap();
        assert_eq!(
            (
                malformed(long_head.as_bytes()),
                large.body,
                large.close,
                malformed(b"PUT /v1/kv HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
                malformed(b"GET /v1/kv HTTP/1.1\nHost: x\n\n"),
                malformed(b"GET /v1/kv HTTP/2\r\n\r\n"),
                malformed(b"PUT / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx"),
            ),
            (true, Body::TooLarge, true, true, true, true, true),
            "rule: a head over 16 KiB, a chunked body, bare-LF lines, another HTTP version and \
             a repeated Content-Length are malformed; a body over 512 KiB is not read, and the \
             connection closes after its 413 (§7.11)"
        );
    }

    #[test]
    fn every_response_is_json_and_never_cached() {
        let mut out = Vec::new();
        let mut response = Response::json(307, json!({"a": 1}));
        response.location = Some("http://127.0.0.1:7002/v1/kv?key=a".to_owned());
        response.close = true;
        write_response(&mut out, &response).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 307 Temporary Redirect\r\nContent-Type: application/json\r\n\
             Content-Length: 7\r\nCache-Control: no-store\r\nLocation: \
             http://127.0.0.1:7002/v1/kv?key=a\r\nConnection: close\r\n\r\n{\"a\":1}",
            "rule: every response carries Cache-Control: no-store (§8.1) and JSON"
        );
    }
}
