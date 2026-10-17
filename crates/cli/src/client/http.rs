//! The CLI's HTTP/1.1 transport: one request per connection over `std::net`.
//!
//! It is bounded in head size, body size and time (CD-9). No redirect is followed here: a
//! `307` is an answer the retry policy acts on (§8.4).
//!
//! [`Transport`] is the seam the client tests replace with a scripted fake. The two ways a
//! send can fail differ in what they tell the caller about the write (A-5): a failed
//! *connect* sent nothing, so the attempt was not submitted ([`TransportError::NotSent`]);
//! anything later may have reached the node ([`TransportError::Unknown`]).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// The largest response head accepted, in bytes.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// The largest response body accepted, in bytes: a scan page of 1 MiB of decoded bytes
/// escapes to at most six times that as JSON (§8.5), plus the envelope.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A request to one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method, e.g. `GET`.
    pub method: &'static str,
    /// The request target: path and query, sent byte for byte.
    pub target: String,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// The body; empty for none.
    pub body: Vec<u8>,
}

/// A response, as far as the client needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The `Location` header, if any.
    pub location: Option<String>,
    /// The body.
    pub body: Vec<u8>,
}

/// Why no response came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The connection was never established, so nothing was sent: the attempt was not
    /// submitted.
    NotSent(String),
    /// The request may have reached the node: the connection broke, timed out or answered
    /// something that is not a response. Nothing is known about the attempt.
    Unknown(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(why) => write!(f, "not sent: {why}"),
            Self::Unknown(why) => write!(f, "no usable response: {why}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Sends one request to one node and returns its response.
pub trait Transport {
    /// Sends `request` to `node`, waiting at most `connect_timeout` to connect and
    /// `response_timeout` for each read and write after that.
    ///
    /// # Errors
    ///
    /// [`TransportError::NotSent`] if the connection could not be made;
    /// [`TransportError::Unknown`] after that.
    fn send(
        &self,
        node: SocketAddr,
        request: &HttpRequest,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> Result<HttpResponse, TransportError>;
}

/// The real transport: TCP, `Connection: close`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpTransport;

impl Transport for TcpTransport {
    fn send(
        &self,
        node: SocketAddr,
        request: &HttpRequest,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        let mut stream = TcpStream::connect_timeout(&node, connect_timeout)
            .map_err(|err| TransportError::NotSent(format!("connect to {node}: {err}")))?;
        let unknown =
            |what: &str, err: io::Error| TransportError::Unknown(format!("{what} {node}: {err}"));
        stream
            .set_read_timeout(Some(response_timeout))
            .and_then(|()| stream.set_write_timeout(Some(response_timeout)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|err| unknown("configure the connection to", err))?;
        stream
            .write_all(&encode_request(node, request))
            .and_then(|()| stream.flush())
            .map_err(|err| unknown("send to", err))?;
        read_response(&mut stream, Instant::now() + response_timeout)
            .map_err(|why| TransportError::Unknown(format!("response from {node}: {why}")))
    }
}

/// The request as bytes.
fn encode_request(node: SocketAddr, request: &HttpRequest) -> Vec<u8> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {node}\r\nConnection: close\r\nAccept: application/json\r\n",
        request.method, request.target
    );
    for (name, value) in &request.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if !request.body.is_empty() || request.method != "GET" {
        head.push_str("Content-Length: ");
        head.push_str(&request.body.len().to_string());
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&request.body);
    bytes
}

/// Reads one response: the head, then a `Content-Length` body or, without one, to the end
/// of the stream. The whole read, not each `read`, ends by `deadline`.
fn read_response(stream: &mut impl Read, deadline: Instant) -> Result<HttpResponse, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            if at > MAX_HEAD_BYTES {
                return Err(format!("the head exceeds {MAX_HEAD_BYTES} bytes"));
            }
            break at;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(format!("the head exceeds {MAX_HEAD_BYTES} bytes"));
        }
        if Instant::now() >= deadline {
            return Err("the response did not arrive in time".to_owned());
        }
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("the connection closed inside the head".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffer[..head_end])
        .map_err(|_| "the head is not UTF-8".to_owned())?
        .to_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| {
            let mut parts = line.split(' ');
            (parts.next()?.starts_with("HTTP/1.")).then_some(())?;
            parts.next()?.parse::<u16>().ok()
        })
        .ok_or_else(|| "the status line is malformed".to_owned())?;
    let mut location = None;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("the header line {line:?} has no colon"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("location") {
            location = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| format!("Content-Length {value:?} is not a number"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    if chunked {
        return Err("a chunked response is not supported".to_owned());
    }
    let mut body = buffer.split_off(head_end + 4);
    match length {
        Some(length) if length > MAX_BODY_BYTES => {
            return Err(format!(
                "the body ({length} bytes) exceeds {MAX_BODY_BYTES}"
            ));
        }
        Some(length) => {
            while body.len() < length {
                if Instant::now() >= deadline {
                    return Err("the response did not arrive in time".to_owned());
                }
                let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
                if read == 0 {
                    return Err(format!(
                        "the connection closed after {} of {length} body bytes",
                        body.len()
                    ));
                }
                body.extend_from_slice(&chunk[..read]);
            }
            body.truncate(length);
        }
        None => loop {
            if Instant::now() >= deadline {
                return Err("the response did not arrive in time".to_owned());
            }
            let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
            if body.len() > MAX_BODY_BYTES {
                return Err(format!("the body exceeds {MAX_BODY_BYTES} bytes"));
            }
        },
    }
    Ok(HttpResponse {
        status,
        location,
        body,
    })
}

/// The index of the first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Percent-encodes `text` for a query value (RFC 3986): every byte but an unreserved one is
/// `%XX`, so a space is `%20` and a plus `%2B` (§8.1).
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0F] {
                out.push(
                    char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase()),
                );
            }
        }
    }
    out
}
