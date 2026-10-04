//! MODEL-SPEC (W30.2): the reference model server, judged against `api/openapi.yaml` over
//! real HTTP (PLAN.md §10.14 use 3, W30, T-7).
//!
//! The `model-server` binary is built into its own target directory and started on a free
//! port. Every answer it gives is judged with the SPEC check's own validator, extension
//! keywords included:
//!
//! - its status is one the spec declares for the operation, or 405/415 with the
//!   `invalid_request` envelope (§8.4: unsupported methods and media types);
//! - it carries `Content-Type: application/json` and `Cache-Control: no-store`;
//! - its body validates against that status's schema.
//!
//! The scenarios cover every operation and every status a single node can show, the spec's
//! own examples reproduced exactly, every request-body and parameter case of
//! `api/fixtures/` sent as a real request (positive ones accepted, negative ones refused), and
//! the raw strict-JSON and query-decoding rules (§8.1). Topology and lifecycle answers (307,
//! 429, 503, 504) and `history_full` are outside the model server's scope (§10.14).
#![cfg(test)]

// check: MODEL-SPEC

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use jsonschema::Registry;
use serde_json::{Value, json};

use crate::spec::{
    Case, Expect, Fixtures, ParameterTarget, ResponseTarget, SPEC_PATH, Target, expand,
    first_errors, instance_validator, load_fixture, load_yaml, spec_registry, target_pointer,
};
use crate::test_support::{nested_cargo, workspace_root};

/// The `model-server` binary, built once per test run into its own target directory.
fn binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let cargo = nested_cargo("keyval-model-spec");
        let args = ["build", "--quiet", "-p", "model", "--bin", "model-server"];
        let status = cargo.command(&workspace_root(), &args).status().unwrap();
        assert!(
            status.success(),
            "fixture: `cargo {}` builds the model server",
            args.join(" ")
        );
        PathBuf::from(cargo.target_dir().unwrap())
            .join("debug")
            .join("model-server")
    })
}

/// A running model server, killed when dropped.
struct ModelServer {
    child: Child,
    /// `host:port`.
    address: String,
}

/// One HTTP answer.
struct Reply {
    status: u16,
    /// Header names in lower case.
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl ModelServer {
    /// Starts `model-server` with `arguments` on a free port.
    fn start(arguments: &[&str]) -> Self {
        let mut child = Command::new(binary())
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .trim()
            .strip_prefix("model-server listening on http://")
            .unwrap_or_else(|| panic!("fixture: model-server prints its URL; got {line:?}"))
            .to_owned();
        Self { child, address }
    }

    /// Sends one request on its own connection and reads the whole answer.
    fn send(&self, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        );
        if !body.is_empty() {
            let _ = write!(head, "Content-Length: {}\r\n", body.len());
        }
        for (name, value) in headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        parse_reply(&raw)
    }
}

impl Drop for ModelServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An HTTP/1.1 answer's status, headers and `Content-Length` body.
fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("an answer has a head: {:?}", String::from_utf8_lossy(raw)));
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("an answer has a status line: {head:?}"));
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let rest = &raw[split + 4..];
    let len = headers
        .get("content-length")
        .and_then(|len| len.parse::<usize>().ok())
        .unwrap_or(rest.len());
    Reply {
        status,
        headers,
        body: rest.get(..len).unwrap_or(rest).to_vec(),
    }
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
fn percent(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

/// A fresh request ID: 32 lowercase hex digits.
fn request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:032x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The spec, parsed once.
fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| load_yaml(&workspace_root().join(SPEC_PATH)).unwrap())
}

/// The operation of `method` on `path`, by `operationId`.
fn operation(method: &str, path: &str) -> Option<&'static str> {
    Some(match (method, path) {
        ("GET", "/v1/kv") => "getKey",
        ("PUT", "/v1/kv") => "putKey",
        ("DELETE", "/v1/kv") => "deleteKey",
        ("POST", "/v1/cas") => "compareAndSwap",
        ("GET", "/v1/scan") => "scanKeys",
        ("GET", "/v1/health/live") => "getLiveness",
        ("GET", "/v1/health/ready") => "getReadiness",
        ("GET", "/v1/cluster") => "getClusterStatus",
        ("GET", path) if path.starts_with("/v1/requests/") => "getRequestStatus",
        _ => return None,
    })
}

/// Judges answers against the spec.
struct Judge {
    registry: Registry<'static>,
}

impl Judge {
    fn new() -> Self {
        Self {
            registry: spec_registry(spec()).unwrap(),
        }
    }

    /// Why the schema at `target` refuses `body`, if it does.
    fn refusal(&self, target: &Target, body: &Value) -> Option<String> {
        let pointer = match target_pointer(spec(), target) {
            Ok(pointer) => pointer,
            Err(why) => return Some(why),
        };
        match instance_validator(&self.registry, &pointer) {
            Ok(validator) => first_errors(&validator, body),
            Err(why) => Some(why),
        }
    }

    /// What is wrong with `reply` to `method target`, by the spec; empty if nothing.
    fn judge(&self, method: &str, target: &str, reply: &Reply) -> Vec<String> {
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        let mut wrong = Vec::new();
        let header = |name: &str| reply.headers.get(name).map(String::as_str);
        if !header("content-type").is_some_and(|value| value.starts_with("application/json")) {
            wrong.push(format!("Content-Type is {:?}", header("content-type")));
        }
        if header("cache-control") != Some("no-store") {
            wrong.push(format!("Cache-Control is {:?}", header("cache-control")));
        }
        let body: Value = match serde_json::from_slice(&reply.body) {
            Ok(body) => body,
            Err(err) => {
                wrong.push(format!("the body is not JSON: {err}"));
                Value::Null
            }
        };
        let status = reply.status.to_string();
        if [405, 415].contains(&reply.status) {
            let envelopes = ["InvalidReadRequestError", "InvalidWriteRequestError"];
            let refusals: Vec<String> = envelopes
                .iter()
                .filter_map(|name| self.refusal(&Target::Schema((*name).to_owned()), &body))
                .collect();
            if refusals.len() == envelopes.len() {
                wrong.push(format!(
                    "a {status} body is no invalid_request envelope (§8.4): {}",
                    refusals.join(" / ")
                ));
            }
        } else if let Some(operation) = operation(method, path) {
            let response = Target::Response(ResponseTarget {
                operation: operation.to_owned(),
                status: status.clone(),
            });
            if let Some(why) = self.refusal(&response, &body) {
                wrong.push(format!("{operation} {status}: {why}"));
            }
        } else {
            wrong.push(format!("{method} {path} is no operation of the spec"));
        }
        wrong
            .into_iter()
            .map(|why| {
                format!(
                    "{method} {target} -> {status} {}: {why}",
                    String::from_utf8_lossy(&reply.body)
                )
            })
            .collect()
    }
}

/// A model server, a judge, and every answer's verdict and (method, path, status).
struct Session {
    server: ModelServer,
    judge: Judge,
    wrong: Vec<String>,
    seen: BTreeSet<(String, String, u16)>,
}

impl Session {
    fn start(arguments: &[&str]) -> Self {
        Self {
            server: ModelServer::start(arguments),
            judge: Judge::new(),
            wrong: Vec::new(),
            seen: BTreeSet::new(),
        }
    }

    /// Sends a request, judges its answer, and returns its status and JSON body.
    fn send(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Value) {
        let reply = self.server.send(method, target, headers, body);
        self.wrong.extend(self.judge.judge(method, target, &reply));
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        let path = if path.starts_with("/v1/requests/") {
            "/v1/requests/{request_id}"
        } else {
            path
        };
        self.seen
            .insert((method.to_owned(), path.to_owned(), reply.status));
        (
            reply.status,
            serde_json::from_slice(&reply.body).unwrap_or(Value::Null),
        )
    }

    /// A write: `method` on `path` for `key` under request ID `rid`, with a JSON body.
    fn write(
        &mut self,
        method: &str,
        path: &str,
        key: &str,
        rid: &str,
        body: &Value,
    ) -> (u16, Value) {
        let target = format!("{path}?key={}", percent(key));
        let headers = [
            ("Idempotency-Key", rid),
            ("Content-Type", "application/json"),
        ];
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        self.send(method, &target, &headers, &bytes)
    }

    fn put(&mut self, key: &str, rid: &str, value: &str) -> (u16, Value) {
        self.write("PUT", "/v1/kv", key, rid, &json!({ "value": value }))
    }

    /// Asserts that every answer so far matched the spec.
    fn assert_all_valid(&self, scenario: &str) {
        assert!(
            self.wrong.is_empty(),
            "rule: every model-server answer matches api/openapi.yaml (W30, T-7); scenario={scenario}\n{}",
            self.wrong.join("\n")
        );
    }
}

impl Session {
    /// Sends a request and asserts its status: a fixture step of a scenario.
    fn expect(
        &mut self,
        what: &str,
        status: u16,
        (method, target): (&str, &str),
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let (got, reply) = self.send(method, target, headers, body);
        assert_eq!(
            got, status,
            "fixture: {what} answers {status}; body {reply}"
        );
    }

    /// Asserts that the session reached every `(method, path, status)` in `wanted`.
    fn assert_reached(&self, wanted: &[(&str, &str, u16)]) {
        let missing: Vec<_> = wanted
            .iter()
            .filter(|(method, path, status)| {
                !self
                    .seen
                    .contains(&((*method).to_owned(), (*path).to_owned(), *status))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "rule: the scenario reaches every status a single node shows (P10); missing \
             {missing:?}"
        );
    }
}

/// The headers of a write under request ID `rid`.
fn write_headers(rid: &str) -> [(&str, &str); 2] {
    [
        ("Idempotency-Key", rid),
        ("Content-Type", "application/json"),
    ]
}

#[test]
fn every_write_status_a_single_node_shows_matches_the_spec() {
    let mut session = Session::start(&[]);
    let (put_id, delete_id) = (request_id(), request_id());
    let one = br#"{"value":"one"}"#;
    let put = ("PUT", "/v1/kv?key=a");
    session.expect("a first PUT", 200, put, &write_headers(&put_id), one);
    session.expect("its replay", 200, put, &write_headers(&put_id), one);
    let two = br#"{"value":"two"}"#;
    session.expect(
        "the ID for another command",
        409,
        put,
        &write_headers(&put_id),
        two,
    );
    let delete = ("DELETE", "/v1/kv?key=a");
    let delete_headers = [("Idempotency-Key", delete_id.as_str())];
    session.expect("a DELETE", 200, delete, &delete_headers, b"");
    let cas = ("POST", "/v1/cas?key=c");
    let absent = br#"{"expected":{"kind":"absent"},"value":"v"}"#;
    session.expect(
        "a CAS on an absent key",
        200,
        cas,
        &write_headers(&request_id()),
        absent,
    );
    let stale = br#"{"expected":{"kind":"version","version":1},"value":"w"}"#;
    session.expect(
        "a CAS on a stale version",
        409,
        cas,
        &write_headers(&request_id()),
        stale,
    );
    let big = json!({ "value": "v".repeat(65_537) }).to_string();
    let big_put = ("PUT", "/v1/kv?key=big");
    session.expect(
        "a value over 64 KiB",
        413,
        big_put,
        &write_headers(&request_id()),
        big.as_bytes(),
    );
    let huge = vec![b'x'; 512 * 1024 + 1];
    session.expect(
        "a body over 512 KiB",
        413,
        big_put,
        &write_headers(&request_id()),
        &huge,
    );
    let empty_key = ("PUT", "/v1/kv?key=");
    session.expect(
        "an empty key",
        400,
        empty_key,
        &write_headers(&request_id()),
        one,
    );
    session.assert_all_valid("writes");
    session.assert_reached(&[
        ("PUT", "/v1/kv", 200),
        ("PUT", "/v1/kv", 400),
        ("PUT", "/v1/kv", 409),
        ("PUT", "/v1/kv", 413),
        ("DELETE", "/v1/kv", 200),
        ("POST", "/v1/cas", 200),
        ("POST", "/v1/cas", 409),
    ]);

    let mut full = Session::start(&["--max-live-keys", "1"]);
    let first = ("PUT", "/v1/kv?key=one");
    full.expect(
        "the only key the quota allows",
        200,
        first,
        &write_headers(&request_id()),
        one,
    );
    let second = ("PUT", "/v1/kv?key=two");
    full.expect(
        "a key past the quota",
        507,
        second,
        &write_headers(&request_id()),
        one,
    );
    full.assert_all_valid("capacity");
    full.assert_reached(&[("PUT", "/v1/kv", 507)]);
}

#[test]
fn every_read_status_a_single_node_shows_matches_the_spec() {
    let mut session = Session::start(&[]);
    let known = request_id();
    let put = ("PUT", "/v1/kv?key=a");
    session.expect(
        "a PUT",
        200,
        put,
        &write_headers(&known),
        br#"{"value":"one"}"#,
    );
    session.expect("a GET", 200, ("GET", "/v1/kv?key=a"), &[], b"");
    session.expect(
        "a GET of no key",
        404,
        ("GET", "/v1/kv?key=missing"),
        &[],
        b"",
    );
    session.expect("a SCAN", 200, ("GET", "/v1/scan"), &[], b"");
    session.expect("a SCAN page", 200, ("GET", "/v1/scan?limit=1"), &[], b"");
    session.expect(
        "a SCAN of limit 0",
        400,
        ("GET", "/v1/scan?limit=0"),
        &[],
        b"",
    );
    let known_status = format!("/v1/requests/{known}");
    session.expect("a known request", 200, ("GET", &known_status), &[], b"");
    let unknown_status = format!("/v1/requests/{}", request_id());
    session.expect(
        "an unknown request",
        200,
        ("GET", &unknown_status),
        &[],
        b"",
    );
    let malformed = ("GET", "/v1/requests/NOT-AN-ID");
    session.expect("a malformed request ID", 400, malformed, &[], b"");
    for path in ["/v1/health/live", "/v1/health/ready", "/v1/cluster"] {
        session.expect(path, 200, ("GET", path), &[], b"");
    }
    session.assert_all_valid("reads");
    session.assert_reached(&[
        ("GET", "/v1/kv", 200),
        ("GET", "/v1/kv", 404),
        ("GET", "/v1/scan", 200),
        ("GET", "/v1/scan", 400),
        ("GET", "/v1/requests/{request_id}", 200),
        ("GET", "/v1/requests/{request_id}", 400),
        ("GET", "/v1/health/live", 200),
        ("GET", "/v1/health/ready", 200),
        ("GET", "/v1/cluster", 200),
    ]);
}

#[test]
fn unsupported_methods_and_media_types_answer_with_the_invalid_request_envelope() {
    let mut session = Session::start(&[]);
    let patch = ("PATCH", "/v1/kv?key=a");
    session.expect("an unsupported method", 405, patch, &[], b"");
    let rid = request_id();
    let untyped = [("Idempotency-Key", rid.as_str())];
    let put = ("PUT", "/v1/kv?key=a");
    session.expect(
        "a body without application/json",
        415,
        put,
        &untyped,
        br#"{"value":"x"}"#,
    );
    session.assert_all_valid("405 and 415");
    session.assert_reached(&[("PATCH", "/v1/kv", 405), ("PUT", "/v1/kv", 415)]);
}

/// The example `name` of the operation at `pointer` (a request body or a response), as
/// JSON.
fn example(pointer: &str, name: &str) -> Value {
    let at = format!("{pointer}/content/application~1json/examples/{name}/value");
    spec()
        .pointer(&at)
        .cloned()
        .unwrap_or_else(|| panic!("fixture: the spec has an example at {at}"))
}

/// Writes filler keys until the next write is index `next`.
fn fill_to(session: &mut Session, next: u64) {
    let (status, first) = session.put("filler-0", &request_id(), "x");
    assert_eq!(status, 200, "fixture: a filler write applies; {first}");
    let mut index = first["index"].as_u64().unwrap();
    assert!(
        index < next,
        "fixture: the log starts below {next}; first index {index}"
    );
    while index + 1 < next {
        let (_, reply) = session.put(&format!("filler-{index}"), &request_id(), "x");
        index = reply["index"].as_u64().unwrap();
    }
}

#[test]
fn the_specs_examples_round_trip() {
    let put = "/paths/~1v1~1kv/put";
    let cas = "/paths/~1v1~1cas/post";

    // PLAN.md §8.6's PUT of `greeting` on node 2, the 42nd entry of its log.
    let mut session = Session::start(&["--node-id", "2"]);
    fill_to(&mut session, 42);
    let (status, reply) = session.write(
        "PUT",
        "/v1/kv",
        "greeting",
        "d70ff69df7084a088e4a531837cc4e5a",
        &example(&format!("{put}/requestBody"), "greeting"),
    );
    assert_eq!(
        (status, reply),
        (200, example(&format!("{put}/responses/200"), "greeting")),
        "rule: the spec's PUT example is the model server's answer, exactly (§8.6)"
    );

    // PLAN.md §8.6's CAS of `counter`: version 41 expected, 57 current, decided at 58.
    let mut session = Session::start(&[]);
    fill_to(&mut session, 57);
    let (status, _) = session.put("counter", &request_id(), "57");
    assert_eq!(status, 200, "fixture: counter is written at index 57");
    let (status, reply) = session.write(
        "POST",
        "/v1/cas",
        "counter",
        "7f3c0b8e2a6d4f19b1c5e0a9d8f7c6b5",
        &example(&format!("{cas}/requestBody"), "counter"),
    );
    let mut documented = example(&format!("{cas}/responses/409"), "version-mismatch");
    let mut answered = reply;
    for body in [&mut documented, &mut answered] {
        if let Some(error) = body.get_mut("error").and_then(Value::as_object_mut) {
            // The message is human text; every contract field is compared.
            error.remove("message");
        }
    }
    assert_eq!(
        (status, answered),
        (409, documented),
        "rule: the spec's CAS example is the model server's answer, but for the message \
         (§8.6)"
    );

    // Every request example the spec documents is accepted.
    for (pointer, name, path) in [
        (put, "greeting", "/v1/kv"),
        (cas, "counter", "/v1/cas"),
        (cas, "create-only", "/v1/cas"),
    ] {
        let method = if path == "/v1/kv" { "PUT" } else { "POST" };
        let body = example(&format!("{pointer}/requestBody"), name);
        let (status, reply) = session.write(
            method,
            path,
            &format!("example-{name}"),
            &request_id(),
            &body,
        );
        assert!(
            [200, 409].contains(&status),
            "rule: the request example {name} is a valid request (§8.6); answered {status} {reply}"
        );
    }
    session.assert_all_valid("examples");
}

/// One fixture case as an HTTP request.
struct FixtureRequest {
    method: &'static str,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// The request for one fixture case, or `None` if the case is no request (a response or
/// component schema).
fn fixture_request(case: &Case) -> Option<FixtureRequest> {
    let instance = expand(&case.instance).unwrap();
    let (operation, field) = match &case.target {
        Target::Request(operation) => (operation.as_str(), None),
        Target::Parameter(ParameterTarget { operation, name }) => {
            (operation.as_str(), Some(name.as_str()))
        }
        Target::Schema(_) | Target::Response(_) => return None,
    };
    let (method, path, body) = match operation {
        "getKey" => ("GET", "/v1/kv", Value::Null),
        "putKey" => ("PUT", "/v1/kv", json!({ "value": "x" })),
        "deleteKey" => ("DELETE", "/v1/kv", Value::Null),
        "compareAndSwap" => (
            "POST",
            "/v1/cas",
            json!({ "expected": { "kind": "absent" }, "value": "x" }),
        ),
        "scanKeys" => ("GET", "/v1/scan", Value::Null),
        "getRequestStatus" => ("GET", "/v1/requests/", Value::Null),
        other => panic!("fixture: no request shape for {other}"),
    };
    let text = || match &instance {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let mut query: Vec<(String, String)> = Vec::new();
    let mut path = path.to_owned();
    let mut rid = request_id();
    let mut body = body;
    match field {
        None => body = instance.clone(),
        Some("Idempotency-Key") => rid = text(),
        Some("request_id") => path.push_str(&percent(&text())),
        Some(name) => query.push((name.to_owned(), text())),
    }
    if path == "/v1/requests/" {
        path.push_str(&rid);
    }
    if ["/v1/kv", "/v1/cas"].contains(&path.as_str())
        && !query.iter().any(|(name, _)| name == "key")
    {
        query.push(("key".to_owned(), format!("fixture-{}", case.name)));
    }
    let mut headers = Vec::new();
    if method != "GET" {
        headers.push(("Idempotency-Key".to_owned(), rid));
    }
    let bytes = if body.is_null() {
        Vec::new()
    } else {
        headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        body.to_string().into_bytes()
    };
    let query: Vec<String> = query
        .iter()
        .map(|(name, value)| format!("{name}={}", percent(value)))
        .collect();
    let target = if query.is_empty() {
        path
    } else {
        format!("{path}?{}", query.join("&"))
    };
    Some(FixtureRequest {
        method,
        target,
        headers,
        body: bytes,
    })
}

/// Sends every request case of `file` and checks it is accepted (positive) or refused
/// (negative) as the spec says, returning how many cases were sent.
fn send_fixture_cases(session: &mut Session, file: &str) -> usize {
    let fixtures: Fixtures = load_fixture(
        &workspace_root(),
        file,
        "keyval.spec.fixtures/v1",
        |fixtures: &Fixtures| &fixtures.schema,
    )
    .unwrap();
    let mut sent = 0;
    let mut misjudged = Vec::new();
    for case in &fixtures.cases {
        let Some(FixtureRequest {
            method,
            target,
            headers,
            body,
        }) = fixture_request(case)
        else {
            continue;
        };
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let (status, reply) = session.send(method, &target, &headers, &body);
        let refused = [400, 413].contains(&status);
        let right = match fixtures.expect {
            Expect::Valid => !refused,
            Expect::Invalid => refused,
        };
        if !right {
            misjudged.push(format!(
                "{file} case {} ({}): {method} {target} -> {status} {reply}",
                case.name, case.rule
            ));
        }
        sent += 1;
    }
    assert!(
        misjudged.is_empty(),
        "rule: the model server accepts every positive request case and refuses every \
         negative one (§8.1, §8.5)\n{}",
        misjudged.join("\n")
    );
    sent
}

#[test]
fn every_fixture_request_is_answered_as_the_spec_says() {
    let mut session = Session::start(&[]);
    let positive = send_fixture_cases(&mut session, "positive.json");
    let negative = send_fixture_cases(&mut session, "negative.json");
    session.assert_all_valid("fixtures");
    assert!(
        positive >= 15 && negative >= 20,
        "rule: the fixture requests are not vacuous; sent {positive} positive and {negative} \
         negative"
    );
}

#[test]
fn raw_strict_json_and_query_decoding_rules_hold() {
    let mut session = Session::start(&[]);
    let put = |session: &mut Session, target: &str, body: &[u8]| {
        let rid = request_id();
        let headers = [
            ("Idempotency-Key", rid.as_str()),
            ("Content-Type", "application/json"),
        ];
        session.send("PUT", target, &headers, body)
    };
    let cases: [(&str, &str, &[u8]); 10] = [
        (
            "a repeated JSON member",
            "/v1/kv?key=k",
            br#"{"value":"a","value":"b"}"#,
        ),
        (
            "trailing bytes after the JSON",
            "/v1/kv?key=k",
            br#"{"value":"a"} x"#,
        ),
        ("a null value", "/v1/kv?key=k", br#"{"value":null}"#),
        ("a JSON array body", "/v1/kv?key=k", br#"["value"]"#),
        (
            "invalid UTF-8 in the body",
            "/v1/kv?key=k",
            b"{\"value\":\"\xff\"}",
        ),
        (
            "an unescaped + in the query",
            "/v1/kv?key=a+b",
            br#"{"value":"a"}"#,
        ),
        (
            "a bad percent escape",
            "/v1/kv?key=%G1",
            br#"{"value":"a"}"#,
        ),
        (
            "percent-encoded invalid UTF-8",
            "/v1/kv?key=%FF",
            br#"{"value":"a"}"#,
        ),
        (
            "a repeated parameter",
            "/v1/kv?key=a&key=b",
            br#"{"value":"a"}"#,
        ),
        (
            "an unknown parameter",
            "/v1/kv?key=a&ttl=5",
            br#"{"value":"a"}"#,
        ),
    ];
    let mut accepted = Vec::new();
    for (case, target, body) in cases {
        let (status, reply) = put(&mut session, target, body);
        if status != 400 {
            accepted.push(format!("{case}: {target} -> {status} {reply}"));
        }
    }
    assert!(
        accepted.is_empty(),
        "rule: strict JSON and RFC 3986 query decoding refuse each with 400 (§8.1)\n{}",
        accepted.join("\n")
    );
    let (status, _) = put(
        &mut session,
        "/v1/kv?key=caf%C3%A9%20au%20lait",
        br#"{"value":"v"}"#,
    );
    let (got, reply) = session.send("GET", "/v1/kv?key=caf%C3%A9%20au%20lait", &[], b"");
    assert_eq!(
        (status, got, reply["value"].as_str()),
        (200, 200, Some("v")),
        "rule: a percent-encoded multibyte key decodes and round-trips (§8.1)"
    );
    session.assert_all_valid("raw rules");
}
