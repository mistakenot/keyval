//! The model server over a real socket (W30.1): keep-alive, `Cache-Control: no-store` on
//! every response, and a malformed request answered and closed.
#![cfg(test)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;

use serde_json::Value as Json;

use super::{Config, Server};
use crate::Limits;

const RID: &str = "0123456789abcdef0123456789abcdef";

/// A server on a free port, serving on its own thread; its address.
fn start() -> std::net::SocketAddr {
    let server = Server::bind(
        "127.0.0.1:0",
        Config::new(1, "keyval-model", Limits::DEFAULT).unwrap(),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    thread::spawn(move || server.serve());
    address
}

/// One response off `reader`: its status, headers (lower-cased names) and JSON body.
fn response(reader: &mut BufReader<TcpStream>) -> (u16, Vec<(String, String)>, Json) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse().unwrap())
        .unwrap();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap())
}

fn has(headers: &[(String, String)], name: &str, value: &str) -> bool {
    headers
        .iter()
        .any(|(header, observed)| header == name && observed == value)
}

#[test]
fn one_connection_serves_several_requests_each_marked_no_store() {
    let address = start();
    let stream = TcpStream::connect(address).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let body = r#"{"value":"v"}"#;
    write!(
        writer,
        "PUT /v1/kv?key=k HTTP/1.1\r\nHost: x\r\nIdempotency-Key: {RID}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}GET /v1/kv?key=k HTTP/1.1\r\nHost: x\r\n\r\n\
         GET /v1/health/live HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    let put = response(&mut reader);
    let get = response(&mut reader);
    let live = response(&mut reader);
    let statuses = (put.0, get.0, live.0, get.2["value"].clone());
    assert_eq!(
        statuses,
        (200, 200, 200, Json::from("v")),
        "rule: a kept-alive connection serves pipelined requests in order"
    );
    assert!(
        [&put.1, &get.1, &live.1]
            .iter()
            .all(|headers| has(headers, "cache-control", "no-store")
                && has(headers, "content-type", "application/json"))
            && has(&live.1, "connection", "close"),
        "rule: every response carries Cache-Control: no-store (§8.1), and Connection: close ends \
         the connection; headers={:?}",
        [&put.1, &get.1, &live.1]
    );
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert!(
        rest.is_empty(),
        "rule: the server closes after Connection: close"
    );
}

#[test]
fn a_malformed_request_is_answered_400_and_closed() {
    let address = start();
    let stream = TcpStream::connect(address).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writer
        .write_all(b"PUT /v1/kv?key=k HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
        .unwrap();
    let (status, headers, body) = response(&mut reader);
    assert_eq!(
        (
            status,
            body["error"]["code"].clone(),
            has(&headers, "connection", "close"),
            has(&headers, "cache-control", "no-store")
        ),
        (400, Json::from("invalid_request"), true, true),
        "rule: a request the server cannot parse is 400 invalid_request, and the connection closes"
    );
}

#[test]
fn an_over_long_body_still_gets_its_413() {
    let address = start();
    for attempt in 0..5 {
        let stream = TcpStream::connect(address).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let body = vec![b'x'; super::MAX_BODY_BYTES + 1];
        let mut request = format!(
            "PUT /v1/kv?key=k HTTP/1.1\r\nIdempotency-Key: {RID}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend(&body);
        writer.write_all(&request).unwrap();
        let (status, headers, reply) = response(&mut reader);
        assert_eq!(
            (
                status,
                reply["error"]["code"].clone(),
                has(&headers, "connection", "close")
            ),
            (413, Json::from("too_large"), true),
            "rule: a body over 512 KiB is answered 413 too_large, and the client reads it rather \
             than a reset, because the server drains before closing (§8.1); attempt {attempt}"
        );
    }
}

#[test]
fn http_1_0_and_head_close_after_their_response() {
    let address = start();
    for (raw, expected) in [
        ("GET /v1/health/live HTTP/1.0\r\n\r\n", 200),
        ("HEAD /v1/kv?key=k HTTP/1.1\r\nHost: x\r\n\r\n", 405),
    ] {
        let stream = TcpStream::connect(address).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        writer.write_all(raw.as_bytes()).unwrap();
        let (status, headers, _) = response(&mut reader);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(
            status == expected && has(&headers, "connection", "close") && rest.is_empty(),
            "rule: an HTTP/1.0 request, and a HEAD whose body the client will not read, close \
             after the response; raw={raw:?} status={status} headers={headers:?}"
        );
    }
}
