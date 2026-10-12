//! Shared fixtures for the corpus: targets, requests, identifiers and response checks.

#![cfg(test)]

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use model::server::Config;
use model::server::Server;
use model::{HistoryLimits, Limits};
use serde_json::Value;

use super::fixtures::{self, Fault};
use crate::http::{self, Request, Response};
use crate::poll::{self, Index, Policy, PollError, SystemClock};
use crate::spec::Spec;
use crate::tag::{Class, Tag, validate_all};
use crate::target::{BaseUrl, Kind, Targets};

/// Orders semantic runs on the real cluster against the tests that kill and restart its
/// nodes: a semantic run holds the shared side, a disruptive topology test the exclusive one,
/// so no semantic request meets a half-dead cluster.
static CLUSTER_GATE: RwLock<()> = RwLock::new(());

/// The shared side of [`CLUSTER_GATE`]: a test that only talks to a healthy cluster.
pub fn cluster_shared() -> RwLockReadGuard<'static, ()> {
    CLUSTER_GATE.read().unwrap_or_else(PoisonError::into_inner)
}

/// The exclusive side of [`CLUSTER_GATE`]: a test that kills or restarts cluster nodes.
pub fn cluster_exclusive() -> RwLockWriteGuard<'static, ()> {
    CLUSTER_GATE.write().unwrap_or_else(PoisonError::into_inner)
}

/// Redirect hops a write follows before the last answer is returned as is.
const MAX_REDIRECTS: usize = 3;

/// Splits an absolute `Location` into its base URL and origin-form target.
fn split_location(location: &str) -> (BaseUrl, String) {
    let rest = location
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("rule: Location is an absolute http URL; got {location:?}"));
    let slash = rest.find('/').unwrap_or(rest.len());
    let base = BaseUrl::parse(&format!("http://{}", &rest[..slash]))
        .unwrap_or_else(|error| panic!("rule: Location names a node; {location:?}: {error}"));
    (base, rest[slash..].to_owned())
}

/// A write request of any method, with an optional `Idempotency-Key` and JSON body.
fn write_request(method: &str, target: &str, id: Option<&str>, body: Option<&str>) -> Request {
    let mut request = Request::new(method, target);
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(id) = id {
        request
            .headers
            .push(("Idempotency-Key".to_owned(), id.to_owned()));
    }
    request
}

/// One target a test talks to.
#[derive(Debug, Clone)]
pub struct Node {
    kind: Kind,
    base: BaseUrl,
    /// A wrong-server fixture to exhibit (W31.3); none outside the fixture tests.
    fault: Option<Fault>,
}

impl Node {
    /// Sends `request`; a transport or protocol failure fails the test (P10).
    pub fn send(&self, request: &Request) -> Response {
        let mut response = http::send(&self.base, request).unwrap_or_else(|error| {
            panic!(
                "rule: every exchange completes; {} {} to {} failed: {error}",
                request.method, request.target, self.base
            )
        });
        if let Some(fault) = self.fault {
            fault.apply(request, &mut response);
        }
        response
    }

    /// `GET target`.
    pub fn get(&self, target: &str) -> Response {
        self.send(&Request::new("GET", target))
    }

    /// A write of any method, with an optional `Idempotency-Key` and JSON body.
    pub fn write(
        &self,
        method: &str,
        target: &str,
        id: Option<&str>,
        body: Option<&str>,
    ) -> Response {
        self.follow_redirects(&write_request(method, target, id, body))
    }

    /// The same write, sent once to this node: a `307` is returned, not followed.
    pub fn write_unfollowed(
        &self,
        method: &str,
        target: &str,
        id: Option<&str>,
        body: Option<&str>,
    ) -> Response {
        self.send(&write_request(method, target, id, body))
    }

    /// Sends a write the way a standard client does (PLAN.md §8.7): a `307` is followed to
    /// its `Location` with the same request ID and body, so a semantic test sees the leader's
    /// answer whichever node it was aimed at. Topology tests use [`Node::send`] and assert on
    /// the `307` itself.
    fn follow_redirects(&self, request: &Request) -> Response {
        let mut at = self.clone();
        for _ in 0..MAX_REDIRECTS {
            let response = at.send(request);
            if response.status != 307 {
                return response;
            }
            let location = response
                .header("Location")
                .unwrap_or_else(|| panic!("rule: a 307 carries Location (F-7); from {}", at.base));
            let (base, target) = split_location(location);
            assert_eq!(
                target, request.target,
                "rule: a 307 Location keeps the same path and query (F-7); from {}",
                at.base
            );
            at.base = base;
        }
        at.send(request)
    }

    /// `PUT /v1/kv` of a raw (not yet escaped) key.
    pub fn put(&self, key: &str, value: &str, id: &str) -> Response {
        let body = serde_json::json!({ "value": value }).to_string();
        self.write(
            "PUT",
            &format!("/v1/kv?key={}", enc(key)),
            Some(id),
            Some(&body),
        )
    }

    /// `DELETE /v1/kv` of a raw key.
    pub fn delete(&self, key: &str, id: &str) -> Response {
        self.write(
            "DELETE",
            &format!("/v1/kv?key={}", enc(key)),
            Some(id),
            None,
        )
    }

    /// `POST /v1/cas` of a raw key.
    pub fn cas(&self, key: &str, expected: &Value, value: &str, id: &str) -> Response {
        let body = serde_json::json!({ "expected": expected, "value": value }).to_string();
        self.write(
            "POST",
            &format!("/v1/cas?key={}", enc(key)),
            Some(id),
            Some(&body),
        )
    }

    /// `GET /v1/requests/{id}`.
    pub fn status(&self, id: &str) -> Response {
        self.get(&format!("/v1/requests/{id}"))
    }

    /// `GET /v1/requests/{id}`, polled until the node has applied `wanted` (C-7): a node that
    /// has not applied the write yet answers `unknown`, which says nothing about the write.
    pub fn status_after(&self, id: &str, wanted: Index) -> Response {
        self.poll_get(&format!("/v1/requests/{id}"), wanted)
    }

    /// `GET /v1/kv` of a raw key, once, with no staleness discipline.
    pub fn read(&self, key: &str) -> Response {
        self.get(&format!("/v1/kv?key={}", enc(key)))
    }

    /// `GET /v1/kv` of a raw key, polled until the node has applied `wanted` (C-7).
    pub fn read_after(&self, key: &str, wanted: Index) -> Response {
        self.poll_get(&format!("/v1/kv?key={}", enc(key)), wanted)
    }

    /// `GET /v1/scan?query`, polled until the node has applied `wanted` (C-7).
    pub fn scan_after(&self, query: &str, wanted: Index) -> Response {
        self.poll_get(&format!("/v1/scan?{query}"), wanted)
    }

    /// `GET /v1/kv` of a raw key, polled within `policy`; running out of budget is an `Err`
    /// the caller must treat as a failure (§10.8, P10).
    pub fn try_read_after(
        &self,
        key: &str,
        wanted: Index,
        policy: Policy,
    ) -> Result<Response, PollError<String>> {
        self.try_poll_get(&format!("/v1/kv?key={}", enc(key)), wanted, policy)
    }

    /// `GET /v1/cluster`, polled until the node has applied `wanted` (C-7): the view of a
    /// follower trails the leader's write.
    pub fn cluster_after(&self, wanted: Index) -> Response {
        let read = || -> Result<(Index, Response), String> {
            let response = self.get("/v1/cluster");
            let view = response.json().map_err(|error| error.to_string())?;
            let applied = view
                .get("applied_index")
                .and_then(Value::as_u64)
                .map(Index)
                .ok_or_else(|| format!("rule: cluster status has applied_index; body={view}"))?;
            Ok((applied, response))
        };
        poll::wait_applied(wanted, Policy::DEFAULT, &mut SystemClock::start(), read)
            .unwrap_or_else(|error| panic!("rule: a cluster view catches up (C-7); {error}"))
    }

    /// Polls `GET target` within [`Policy::DEFAULT`]; running out of budget fails the test.
    fn poll_get(&self, target: &str, wanted: Index) -> Response {
        self.try_poll_get(target, wanted, Policy::DEFAULT)
            .unwrap_or_else(|error| panic!("rule: a read catches up (C-7); GET {target}: {error}"))
    }

    fn try_poll_get(
        &self,
        target: &str,
        wanted: Index,
        policy: Policy,
    ) -> Result<Response, PollError<String>> {
        let read = || {
            let response = self.get(target);
            let body = response.json().map_err(|error| error.to_string())?;
            let applied = applied_in(&body).ok_or_else(|| {
                format!(
                    "rule: a read reports served_by.applied_index; status={} body={body}",
                    response.status
                )
            })?;
            Ok((applied, response))
        };
        poll::wait_applied(wanted, policy, &mut SystemClock::start(), read)
    }
}

/// `served_by.applied_index` of a success body, or of a `not_found` error's details.
pub fn applied_in(body: &Value) -> Option<Index> {
    Index::applied_in(body).or_else(|| body.pointer("/error/details").and_then(Index::applied_in))
}

/// A response as a short string for assertion messages.
pub fn say(response: &Response) -> String {
    format!(
        "status={} body={}",
        response.status,
        String::from_utf8_lossy(&response.body)
    )
}

/// The response's JSON body.
pub fn body(response: &Response) -> Value {
    response
        .json()
        .unwrap_or_else(|error| panic!("rule: a body is JSON; {error}; {}", say(response)))
}

/// Asserts a `200` and returns the body, after the checks every response owes.
pub fn ok(response: &Response, why: &str) -> Value {
    assert_eq!(response.status, 200, "rule: {why}; {}", say(response));
    assert_no_store(response, why);
    body(response)
}

/// Asserts an error response: status, `error.code`, the envelope and `no-store`. Returns the
/// error's `details`.
pub fn error(response: &Response, status: u16, code: &str, why: &str) -> Value {
    assert_eq!(response.status, status, "rule: {why}; {}", say(response));
    assert_no_store(response, why);
    let body = body(response);
    let error = &body["error"];
    assert_eq!(
        error["code"],
        code,
        "rule: {why}; expected error code {code}; {}",
        say(response)
    );
    assert!(
        error["message"].is_string() && error["details"].is_object(),
        "rule: an error has a message and a details object (§8); {}",
        say(response)
    );
    error["details"].clone()
}

/// Asserts `Cache-Control: no-store` (§8, C-6): every response, errors included.
pub fn assert_no_store(response: &Response, why: &str) {
    assert_eq!(
        response.header("Cache-Control"),
        Some("no-store"),
        "rule: every response is Cache-Control: no-store; {why}; {}",
        say(response)
    );
}

/// Asserts that `value` is an object with exactly the keys `names`.
pub fn assert_keys(value: &Value, names: &[&str], why: &str) {
    let mut have: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("rule: {why}; not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    have.sort_unstable();
    let mut want = names.to_vec();
    want.sort_unstable();
    assert_eq!(have, want, "rule: {why}; body={value}");
}

/// The `index` of a write result.
pub fn index_of(value: &Value) -> Index {
    Index::write_index_in(value)
        .unwrap_or_else(|| panic!("rule: a write result has an index; body={value}"))
}

/// Percent-escapes every byte that is not an unreserved character, so `+` is `%2B` and a
/// space is `%20`.
pub fn enc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A process-and-run-unique number, so concurrent targets and reruns against a shared node
/// never meet each other's keys or request IDs.
fn run_nonce() -> u64 {
    static RUN: OnceLock<u64> = OnceLock::new();
    *RUN.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let mixed = nanos ^ (u128::from(std::process::id()) << 64);
        u64::try_from(mixed & u128::from(u64::MAX)).unwrap_or(0)
    })
}

fn next_serial() -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(1);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// A fresh request ID: 32 lowercase hex characters.
pub fn fresh_id() -> String {
    format!("{:016x}{:016x}", run_nonce(), next_serial())
}

/// A fresh key prefix that ends in `/`, unique to this call.
pub fn fresh_prefix() -> String {
    format!("conf/{:016x}/{}/", run_nonce(), next_serial())
}

/// The `start=…&end=…` query selecting exactly the keys under `prefix` (which ends in `/`).
pub fn prefix_range(prefix: &str) -> String {
    let end = format!("{}0", &prefix[..prefix.len() - 1]);
    format!("start={}&end={}", enc(prefix), enc(&end))
}

fn spec() -> Spec {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/openapi.yaml");
    Spec::load(&path).unwrap_or_else(|error| panic!("rule: the spec loads; {error}"))
}

/// Holds `tag` to the spec: a typo in an operation or error code fails the test.
fn assert_tag(tag: &Tag) {
    assert_eq!(
        tag.class,
        Class::Semantic,
        "rule: the corpus holds semantic tests; {}",
        tag.test
    );
    let problems = validate_all(&[*tag], &spec());
    assert!(
        problems.is_empty(),
        "rule: a tag is sound; problems={problems:?}"
    );
}

/// A fresh in-process reference model server under `limits`.
fn spawn_model(limits: Limits) -> Node {
    let config = Config::new(1, "keyval-model", limits, HistoryLimits::DEFAULT)
        .unwrap_or_else(|error| panic!("rule: the model config is valid; {error}"));
    let server = Server::bind("127.0.0.1:0", config)
        .unwrap_or_else(|error| panic!("rule: the model server binds; {error}"));
    let address = server
        .local_addr()
        .unwrap_or_else(|error| panic!("rule: the bound address is known; {error}"));
    // The listener thread ends with the test process.
    thread::spawn(move || server.serve());
    let base = BaseUrl::parse(&format!("http://{address}"))
        .unwrap_or_else(|error| panic!("rule: the model URL parses; {error}"));
    Node {
        kind: Kind::Model,
        base,
        fault: fixtures::active(),
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_else(|| "a panic without text".to_owned())
}

fn run_on(tag: &Tag, node: &Node, check: &impl Fn(&Node)) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| check(node))) {
        panic!(
            "[{} on {} {}] {}",
            tag.test,
            node.kind.name(),
            node.base,
            panic_text(payload.as_ref())
        );
    }
}

/// Runs `check` on a fresh in-process model server under the default limits and on every
/// real target the environment names that serves all of `tag`'s operations (§10.8). Returns
/// how many targets ran; the model always does, so the count is at least 1.
pub fn each_target(tag: &Tag, check: impl Fn(&Node)) -> usize {
    assert_tag(tag);
    let model = spawn_model(Limits::DEFAULT);
    run_on(tag, &model, &check);
    let targets = Targets::from_lookup(|name| std::env::var(name).ok())
        .unwrap_or_else(|error| panic!("rule: the target environment is valid; {error}"));
    let mut ran = 1;
    let _healthy = cluster_shared();
    for (kind, base) in targets.selected(tag.class, tag.operations) {
        let node = Node {
            kind,
            base: base.clone(),
            fault: fixtures::active(),
        };
        run_on(tag, &node, &check);
        ran += 1;
    }
    ran
}

impl Node {
    /// A node of a real target at `base`.
    pub fn at(kind: Kind, base: BaseUrl) -> Self {
        Self {
            kind,
            base,
            fault: fixtures::active(),
        }
    }

    /// The node's base URL.
    pub const fn base(&self) -> &BaseUrl {
        &self.base
    }
}

/// Runs `check` on a fresh model server under `limits` only: a limit is server
/// configuration, which a real node does not let a test change.
pub fn with_model(tag: &Tag, limits: Limits, check: impl Fn(&Node)) {
    assert_tag(tag);
    run_on(tag, &spawn_model(limits), &check);
}
