//! A small HTTP/1.1 reader and writer over `std::io`, enough for a test-only single-node
//! server (owner decision on br-w30-1-model-server-impl-eop): request heads up to
//! [`MAX_HEAD_BYTES`], bodies by `Content-Length` up to the contract's 512 KiB
//! (§8.1 "Request size"), keep-alive, `Expect: 100-continue`, and `Cache-Control: no-store`
//! on every response (§8.1 "No caching"). Chunked request bodies are refused.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use serde_json::Value;

/// The most bytes a request head (request line and headers) may take.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The most bytes a request body may take (§8.1 "Request size", `x-maxBodyBytes`).
pub const MAX_BODY_BYTES: usize = 512 * 1024;

/// A request's body, as far as it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// The body's bytes (empty when the request has none).
    Bytes(Vec<u8>),
    /// A `Content-Length` over [`MAX_BODY_BYTES`]: the body was not read, so the connection
    /// closes after the response (`413 too_large`).
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
    /// Whether the client asked to close the connection, or speaks HTTP/1.0.
    pub close: bool,
}

impl Request {
    /// Every value of the header field `name` (lower-case; RFC 9110 calls headers fields).
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

/// One response: a status and a JSON body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The JSON body.
    pub body: Value,
    /// The methods the path allows, for a `405`.
    pub allow: Option<&'static str>,
    /// Whether the connection closes after this response.
    pub close: bool,
}

impl Response {
    /// A response with `status` and `body` that keeps the connection open.
    pub const fn new(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            allow: None,
            close: false,
        }
    }
}

/// Reads one request from `reader`. `continue_to` receives the interim
/// `100 Continue` a client that sent `Expect: 100-continue` waits for before its body.
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
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        507 => "Insufficient Storage",
        _ => "Status",
    }
}

/// Writes `response` with `Content-Type: application/json`, its length and
/// `Cache-Control: no-store`.
pub fn write_response(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    let body = response.body.to_string();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\n",
        response.status,
        reason(response.status),
        body.len()
    );
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

    use super::*;

    fn read(raw: &[u8]) -> (Result<Request, ReadError>, Vec<u8>) {
        let mut continued = Vec::new();
        let result = read_request(&mut Cursor::new(raw.to_vec()), &mut continued);
        (result, continued)
    }

    #[test]
    fn a_request_with_a_body_is_read_whole() {
        let raw = b"PUT /v1/kv?key=a HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"value\":\"v\"}";
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
                continued.is_empty()
            ),
            (
                "PUT",
                "/v1/kv",
                Some("key=a"),
                vec!["application/json"],
                Body::Bytes(b"{\"value\":\"v\"}".to_vec()),
                false,
                true
            ),
            "rule: a request's line, headers and Content-Length body are read exactly"
        );
    }

    #[test]
    fn an_over_long_body_is_flagged_and_not_read() {
        let raw = format!(
            "PUT /v1/kv?key=a HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let request = read(raw.as_bytes()).0.unwrap();
        assert_eq!(
            (request.body, request.close),
            (Body::TooLarge, true),
            "rule: a body over 512 KiB is too large and closes the connection (§8.1)"
        );
    }

    #[test]
    fn expect_100_continue_is_answered_before_the_body() {
        let raw =
            b"PUT /v1/kv?key=a HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n{}";
        let (request, continued) = read(raw);
        assert!(
            request.is_ok() && continued == b"HTTP/1.1 100 Continue\r\n\r\n",
            "rule: a client waiting on Expect: 100-continue gets the interim response"
        );
    }

    #[test]
    fn malformed_heads_are_refused() {
        let cases: [&[u8]; 6] = [
            b"GARBAGE\r\n\r\n",
            b"GET /v1/kv HTTP/2\r\n\r\n",
            b"GET v1/kv HTTP/1.1\r\n\r\n",
            b"GET /v1/kv HTTP/1.1\r\nNoColon\r\n\r\n",
            b"PUT /v1/kv HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"PUT /v1/kv HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
        ];
        for raw in cases {
            let (result, _) = read(raw);
            assert!(
                matches!(result, Err(ReadError::Malformed(_))),
                "rule: a malformed request is refused; raw={:?} result={result:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn framing_is_strict_and_connection_is_a_token_list() {
        for raw in [
            &b"PUT /v1/kv HTTP/1.1\r\nContent-Length: +1\r\n\r\nx"[..],
            b"GET /v1/kv HTTP/1.1\nHost: x\n\n",
            b"GET /v1/kv HTTP/1.1\r\nBad\tName: x\r\n\r\n",
        ] {
            let (result, _) = read(raw);
            assert!(
                matches!(result, Err(ReadError::Malformed(_))),
                "rule: a signed Content-Length, a bare LF or a header name with whitespace is \
                 refused at once; raw={:?} result={result:?}",
                String::from_utf8_lossy(raw)
            );
        }
        let request = read(b"GET /v1/kv HTTP/1.1\r\nConnection: keep-alive, Close\r\n\r\n")
            .0
            .unwrap();
        assert!(
            request.close,
            "rule: Connection is a token list; any `close` token closes"
        );
    }

    #[test]
    fn an_over_long_head_is_refused_and_eof_closes_quietly() {
        let mut raw = b"GET /v1/kv?key=".to_vec();
        raw.extend(vec![b'a'; MAX_HEAD_BYTES]);
        raw.extend(b" HTTP/1.1\r\n\r\n");
        assert!(
            matches!(read(&raw).0, Err(ReadError::Malformed(_)))
                && matches!(read(b"").0, Err(ReadError::Closed)),
            "rule: a head over the limit is refused, and a closed connection is not an error"
        );
    }

    #[test]
    fn every_response_carries_no_store_and_its_length() {
        let mut out = Vec::new();
        let mut response = Response::new(405, serde_json::json!({"a": 1}));
        response.allow = Some("GET");
        response.close = true;
        write_response(&mut out, &response).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\
             Cache-Control: no-store\r\nAllow: GET\r\nConnection: close\r\n\r\n{\"a\":1}",
            "rule: every response carries Cache-Control: no-store (§8.1)"
        );
    }
}
