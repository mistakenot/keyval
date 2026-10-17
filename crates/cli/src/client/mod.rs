//! The client operations of the CLI (PLAN.md §8.7, O-4, C-6; W33.2): `put`, `get`,
//! `delete`, `cas`, `scan`, `status` and `cluster` against a keyval cluster.
//!
//! - **One identity.** A write's request ID is generated once (32 lowercase hex digits) and
//!   its exact command is persisted in the [`PendingStore`] *before* the first send (O-4).
//!   Every later send of that write — after a redirect, a refusal, an `unknown_outcome` or
//!   a lost response — carries the same ID and the same bytes. The record is deleted only
//!   after a terminal result.
//! - **One policy.** What to do after each answer is [`types::Retry`], the policy the
//!   simulated clients use (§10.3): follow `not_leader` at once, back off on `busy`,
//!   `unavailable` and `recovering`, resend or ask the status lookup after an ambiguous
//!   outcome, within the bounds of §7.11 (10 sends, 5 distinct nodes, 15 s). This module
//!   only performs the policy's decisions: send, sleep, ask.
//! - **Honest endings.** When a bound runs out the write is [`Outcome::Unresolved`] with its
//!   request ID, never failed (§8.7), and its pending record stays for `resume`.
//!
//! Time, entropy and the network are traits ([`Clock`], [`RandomSource`], [`Transport`]) so
//! the tests drive the real client against scripted answers and a virtual clock.

mod decode;
pub mod http;

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use types::{
    Config, Exhausted, ExpectedVersion, Heard, Key, Limit, Mutation, Next, NodeId, OpKind, Refusal,
    RequestId, RequestState, Response, Retry, RetryBounds, ScanRange, Value,
};

pub use decode::{DecodeError, Decoded, Expect, Rejection, decode};
use http::{HttpRequest, HttpResponse, Transport, TransportError, percent_encode};

use crate::pending::{Method, PendingError, PendingStore, Record};

/// How long a connect may take.
pub const CONNECT_TIMEOUT_MS: u64 = 2_000;
/// How long to wait for a response after sending: past the server's 5 s write deadline
/// (§7.11), so its `504` can arrive.
pub const RESPONSE_TIMEOUT_MS: u64 = 6_000;
/// The least time an attempt is given, even close to the overall deadline.
const MIN_ATTEMPT_MS: u64 = 250;
/// The most protocol notes one operation keeps (CD-9).
const MAX_NOTES: usize = 32;

/// Time for the client: monotonic milliseconds, and waiting.
pub trait Clock {
    /// Milliseconds since an arbitrary fixed start; never goes backwards.
    fn now_ms(&self) -> u64;
    /// Waits until `now_ms()` is at least `at_ms`.
    fn sleep_until_ms(&self, at_ms: u64);
}

/// The system clock.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// A clock that starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_until_ms(&self, at_ms: u64) {
        let now = self.now_ms();
        if at_ms > now {
            std::thread::sleep(Duration::from_millis(at_ms - now));
        }
    }
}

/// Where request IDs and the retry policy's choices come from.
pub trait RandomSource {
    /// Fills `bytes` with random bytes.
    ///
    /// # Errors
    ///
    /// A description of why no entropy was available.
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), String>;
}

/// The operating system's entropy: `/dev/urandom`.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), String> {
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(bytes))
            .map_err(|err| format!("/dev/urandom: {err}"))
    }
}

/// Why an operation could not run at all (nothing was sent).
#[derive(Debug)]
pub enum ClientError {
    /// The pending store refused: nothing was sent, so no request is outstanding.
    Pending(PendingError),
    /// No entropy for a request ID.
    Entropy(String),
    /// The client has no node to send to.
    NoNodes,
    /// A defect in the client itself.
    Internal(&'static str),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(err) => write!(f, "{err}; nothing was sent"),
            Self::Entropy(why) => write!(f, "cannot generate a request ID: {why}"),
            Self::NoNodes => write!(f, "no node to send to"),
            Self::Internal(what) => write!(f, "internal error in the client: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<PendingError> for ClientError {
    fn from(err: PendingError) -> Self {
        Self::Pending(err)
    }
}

/// The nodes and bounds a client works within.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The nodes, in configured order, with their client addresses.
    pub nodes: Vec<(NodeId, SocketAddr)>,
    /// The retry bounds (§7.11).
    pub bounds: RetryBounds,
    /// The most distinct nodes one operation may use (`CliRetryNodes`); the status and
    /// cluster lookups ask at most this many.
    pub max_nodes: usize,
    /// The overall deadline in milliseconds (§7.11); each attempt's wait is capped by what
    /// is left of it.
    pub deadline_ms: u64,
}

impl Settings {
    /// The nodes of `config` and its CLI bounds.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        Self {
            nodes: config
                .nodes()
                .map(|node| (node.id(), node.client_address()))
                .collect(),
            bounds: RetryBounds::of(config),
            max_nodes: usize::try_from(config.limit(Limit::CliRetryNodes)).unwrap_or(usize::MAX),
            deadline_ms: config.limit(Limit::CliDeadlineMs),
        }
    }

    fn ids(&self) -> Vec<NodeId> {
        self.nodes.iter().map(|(id, _)| *id).collect()
    }

    /// The first `max_nodes` nodes, in configured order.
    fn lookup_nodes(&self) -> Vec<(NodeId, SocketAddr)> {
        self.nodes.iter().take(self.max_nodes).copied().collect()
    }

    fn address(&self, node: NodeId) -> Option<SocketAddr> {
        self.nodes
            .iter()
            .find_map(|(id, address)| (*id == node).then_some(*address))
    }
}

/// A terminal answer the cluster gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// What the policy understood it to be.
    pub response: Response,
    /// The HTTP status of the answer that ended the operation.
    pub http_status: u16,
    /// That answer's body, as the server sent it.
    pub body: Json,
    /// The node that gave it.
    pub node: NodeId,
    /// Whether the status lookup, not a resend, resolved a write.
    pub via_status_lookup: bool,
}

/// A write or read that ran out of bounds without a terminal answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unresolved {
    /// The write's request ID, for `resume`; `None` for a read.
    pub request_id: Option<RequestId>,
    /// Which bound ran out.
    pub exhausted: Exhausted,
    /// The sends made, status lookups included.
    pub sends: u64,
    /// The distinct nodes sent to.
    pub nodes_tried: Vec<NodeId>,
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A terminal answer: for a write, its result (a success, `version_mismatch`,
    /// `capacity_exceeded` or `request_id_conflict`); for a read, its answer.
    Answered(Answer),
    /// The server refused the request itself (`400`, `413`, …): nothing executed, and the
    /// same bytes would be refused again.
    Rejected(Rejection),
    /// `history_full`, with no send that may have been submitted: the write did not
    /// execute and retrying cannot help until the limit is raised. Its pending record is
    /// kept.
    Refused {
        /// The write's request ID.
        request_id: RequestId,
    },
    /// A bound ran out. Never "failed": a write may have executed.
    Unresolved(Unresolved),
}

/// An operation's outcome and what happened on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// How it ended.
    pub outcome: Outcome,
    /// Things worth telling the person: unreadable answers, unreachable nodes. Bounded.
    pub notes: Vec<String>,
    /// The pending record could not be deleted after a terminal result; the write is done
    /// and `resume` will replay it harmlessly (C-6).
    pub leftover: Option<String>,
}

/// The nodes' answers to a status lookup (`keyval status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    /// The request.
    pub request_id: RequestId,
    /// A node that has applied it, with its answer, or `None`: unknown on every node asked.
    /// Unknown is not failure (F-9, P8).
    pub known: Option<Answer>,
    /// The nodes that answered `unknown`.
    pub unknown_on: Vec<NodeId>,
    /// Notes about nodes that could not be asked.
    pub notes: Vec<String>,
}

/// One node's cluster status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterReport {
    /// The node that answered.
    pub node: NodeId,
    /// Its `GET /v1/cluster` body.
    pub body: Json,
    /// Notes about nodes tried first.
    pub notes: Vec<String>,
}

/// The client.
pub struct Client<T, C, R> {
    settings: Settings,
    transport: T,
    clock: C,
    random: R,
    store: Option<PendingStore>,
}

impl<T: Transport, C: Clock, R: RandomSource> Client<T, C, R> {
    /// A client over `settings`. Writes persist to `store` first when there is one.
    pub const fn new(
        settings: Settings,
        transport: T,
        clock: C,
        random: R,
        store: Option<PendingStore>,
    ) -> Self {
        Self {
            settings,
            transport,
            clock,
            random,
            store,
        }
    }

    /// `PUT key value` under a fresh request ID.
    ///
    /// # Errors
    ///
    /// [`ClientError`] if nothing could be sent.
    pub fn put(&mut self, key: &Key, value: &Value) -> Result<(Report, RequestId), ClientError> {
        let body = serde_json::json!({"value": value.as_str()}).to_string();
        self.write(Method::Put, "/v1/kv", key, body)
    }

    /// `DELETE key` under a fresh request ID.
    ///
    /// # Errors
    ///
    /// [`ClientError`] if nothing could be sent.
    pub fn delete(&mut self, key: &Key) -> Result<(Report, RequestId), ClientError> {
        self.write(Method::Delete, "/v1/kv", key, String::new())
    }

    /// Compare-and-swap under a fresh request ID.
    ///
    /// # Errors
    ///
    /// [`ClientError`] if nothing could be sent.
    pub fn cas(
        &mut self,
        key: &Key,
        expected: ExpectedVersion,
        value: &Value,
    ) -> Result<(Report, RequestId), ClientError> {
        let body = serde_json::json!({
            "expected": decode::expectation_json(expected),
            "value": value.as_str(),
        })
        .to_string();
        self.write(Method::Post, "/v1/cas", key, body)
    }

    /// The command of `mutation` under a fresh request ID.
    ///
    /// # Errors
    ///
    /// [`ClientError`] if nothing could be sent.
    pub fn mutate(&mut self, mutation: &Mutation) -> Result<(Report, RequestId), ClientError> {
        match mutation {
            Mutation::Put { key, value } => self.put(key, value),
            Mutation::Delete { key } => self.delete(key),
            Mutation::Cas {
                key,
                expected,
                value,
            } => self.cas(key, *expected, value),
        }
    }

    fn write(
        &mut self,
        method: Method,
        path: &str,
        key: &Key,
        body: String,
    ) -> Result<(Report, RequestId), ClientError> {
        let request_id = self.new_request_id()?;
        let record = Record {
            request_id,
            method,
            target: format!("{path}?key={}", percent_encode(key.as_str())),
            body,
        };
        // Durable before any send (O-4): if this fails, nothing was sent.
        if let Some(store) = &self.store {
            store.persist(&record)?;
        }
        let report = self.send_write(&record)?;
        Ok((report, request_id))
    }

    /// Sends the write `record` — already persisted, if the client has a store — until it
    /// ends, with the same ID and bytes on every send. `resume` calls this with a record
    /// read back from the store; it never makes a new ID.
    ///
    /// # Errors
    ///
    /// [`ClientError::NoNodes`] if the client has no node to send to.
    pub fn send_write(&mut self, record: &Record) -> Result<Report, ClientError> {
        let mut report = self.run(
            OpKind::Write,
            Some(record.request_id),
            Expect::Write,
            &write_request(record),
        )?;
        // The record is deleted only after a terminal result (§8.7). A rejection is one
        // too: it was refused before it could execute, and always will be.
        let terminal = match &report.outcome {
            Outcome::Answered(_) | Outcome::Rejected(_) => true,
            Outcome::Refused { .. } | Outcome::Unresolved(_) => false,
        };
        if terminal && let Some(store) = &self.store {
            report.leftover = store
                .remove(record.request_id)
                .err()
                .map(|err| err.to_string());
        }
        Ok(report)
    }

    /// `GET key`.
    ///
    /// # Errors
    ///
    /// [`ClientError::NoNodes`] if the client has no node to send to.
    pub fn get(&mut self, key: &Key) -> Result<Report, ClientError> {
        let target = format!("/v1/kv?key={}", percent_encode(key.as_str()));
        let request = HttpRequest {
            method: "GET",
            target,
            headers: Vec::new(),
            body: Vec::new(),
        };
        self.run(OpKind::Read, None, Expect::Get, &request)
    }

    /// One page of a SCAN.
    ///
    /// # Errors
    ///
    /// [`ClientError::NoNodes`] if the client has no node to send to.
    pub fn scan(&mut self, range: &ScanRange) -> Result<Report, ClientError> {
        let mut target = format!("/v1/scan?limit={}", range.limit().get());
        for (name, bound) in [
            ("start", range.start()),
            ("end", range.end()),
            ("after", range.after()),
        ] {
            if let Some(bound) = bound {
                target.push('&');
                target.push_str(name);
                target.push('=');
                target.push_str(&percent_encode(bound.as_str()));
            }
        }
        let request = HttpRequest {
            method: "GET",
            target,
            headers: Vec::new(),
            body: Vec::new(),
        };
        self.run(OpKind::Read, None, Expect::Scan(range), &request)
    }

    /// Asks the nodes, in configured order and at most the node bound of them, whether they
    /// have applied `request_id`; stops at the first that has. A node that has not says
    /// `unknown`, which is not a failure (F-9, P8), and a node that cannot answer is noted
    /// and skipped.
    ///
    /// # Errors
    ///
    /// [`ClientError::NoNodes`] if the client has no node to send to.
    pub fn status(&mut self, request_id: RequestId) -> Result<StatusReport, ClientError> {
        if self.settings.nodes.is_empty() {
            return Err(ClientError::NoNodes);
        }
        let started = self.clock.now_ms();
        let ids = self.settings.ids();
        let request = HttpRequest {
            method: "GET",
            target: format!("/v1/requests/{request_id}"),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let mut report = StatusReport {
            request_id,
            known: None,
            unknown_on: Vec::new(),
            notes: Vec::new(),
        };
        for (node, address) in self.settings.lookup_nodes() {
            let remaining = self.remaining_ms(started);
            if remaining == 0 {
                note(
                    &mut report.notes,
                    "the overall deadline ended the status lookup".to_owned(),
                );
                break;
            }
            match self.exchange(address, &request, remaining) {
                Err(err) => note(&mut report.notes, format!("node {node}: {err}")),
                Ok(response) => match decode(&response, Expect::Status, &ids, node) {
                    Ok(Decoded::Answer(Response::Status { state, served_by })) => match state {
                        RequestState::Known(_) => {
                            report.known = Some(answer(
                                Response::Status { state, served_by },
                                &response,
                                node,
                                false,
                            ));
                            break;
                        }
                        RequestState::Unknown => report.unknown_on.push(node),
                    },
                    Ok(Decoded::Answer(Response::Refused(refusal))) => {
                        note(
                            &mut report.notes,
                            format!("node {node} refused: {}", refusal.error_code()),
                        );
                    }
                    Ok(Decoded::Answer(other)) => {
                        note(
                            &mut report.notes,
                            format!("node {node}: unexpected answer {other:?}"),
                        );
                    }
                    Ok(Decoded::Rejected(rejection)) => {
                        note(
                            &mut report.notes,
                            format!(
                                "node {node} rejected the lookup: {} {}: {}",
                                rejection.status, rejection.code, rejection.message
                            ),
                        );
                    }
                    Err(err) => note(&mut report.notes, format!("node {node}: {err}")),
                },
            }
        }
        Ok(report)
    }

    /// `GET /v1/cluster` from the first node that answers.
    ///
    /// # Errors
    ///
    /// [`ClientError::NoNodes`] if the client has no node to send to. When no node
    /// answers, the report's body is `null` and its notes say why.
    pub fn cluster(&mut self) -> Result<ClusterReport, ClientError> {
        let Some(&(first, _)) = self.settings.nodes.first() else {
            return Err(ClientError::NoNodes);
        };
        let started = self.clock.now_ms();
        let request = HttpRequest {
            method: "GET",
            target: "/v1/cluster".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let mut notes = Vec::new();
        for (node, address) in self.settings.lookup_nodes() {
            let remaining = self.remaining_ms(started);
            if remaining == 0 {
                note(
                    &mut notes,
                    "the overall deadline ended the lookup".to_owned(),
                );
                break;
            }
            match self.exchange(address, &request, remaining) {
                Err(err) => note(&mut notes, format!("node {node}: {err}")),
                Ok(response) if response.status == 200 => {
                    match serde_json::from_slice(&response.body) {
                        Ok(body) => return Ok(ClusterReport { node, body, notes }),
                        Err(err) => {
                            note(&mut notes, format!("node {node}: body is not JSON: {err}"));
                        }
                    }
                }
                Ok(response) => note(
                    &mut notes,
                    format!("node {node}: status {}", response.status),
                ),
            }
        }
        Ok(ClusterReport {
            node: first,
            body: Json::Null,
            notes,
        })
    }

    fn new_request_id(&mut self) -> Result<RequestId, ClientError> {
        let mut bytes = [0_u8; 16];
        self.random.fill(&mut bytes).map_err(ClientError::Entropy)?;
        Ok(RequestId::from_bytes(bytes))
    }

    fn draw(&mut self) -> Result<u64, ClientError> {
        let mut bytes = [0_u8; 8];
        self.random.fill(&mut bytes).map_err(ClientError::Entropy)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// What is left of the overall deadline that began at `started`.
    fn remaining_ms(&self, started: u64) -> u64 {
        self.settings
            .deadline_ms
            .saturating_sub(self.clock.now_ms().saturating_sub(started))
    }

    /// One exchange with the node at `address`, waiting at most what `remaining_ms` of the
    /// deadline allows (never less than a short minimum).
    fn exchange(
        &self,
        address: SocketAddr,
        request: &HttpRequest,
        remaining_ms: u64,
    ) -> Result<HttpResponse, TransportError> {
        let response_ms = RESPONSE_TIMEOUT_MS.min(remaining_ms.max(MIN_ATTEMPT_MS));
        let connect_ms = CONNECT_TIMEOUT_MS.min(response_ms);
        self.transport.send(
            address,
            request,
            Duration::from_millis(connect_ms),
            Duration::from_millis(response_ms),
        )
    }

    /// Runs one operation under the retry policy until it ends.
    fn run(
        &mut self,
        kind: OpKind,
        request_id: Option<RequestId>,
        expect: Expect<'_>,
        operation: &HttpRequest,
    ) -> Result<Report, ClientError> {
        let ids = self.settings.ids();
        let Some(&first) = ids.first() else {
            return Err(ClientError::NoNodes);
        };
        let started = self.clock.now_ms();
        // Start at a random node so that clients do not all begin at the same one.
        let pick = self.draw()? % u64::try_from(ids.len()).unwrap_or(1);
        let first = ids
            .get(usize::try_from(pick).unwrap_or(0))
            .copied()
            .unwrap_or(first);
        let (mut retry, mut next) = Retry::start(self.settings.bounds, kind, &ids, first, started);
        let mut notes = Vec::new();
        loop {
            let (node, at_ms, asking) = match next {
                Next::Send { node, at_ms } => (node, at_ms, false),
                Next::AskStatus { node, at_ms } => (node, at_ms, true),
                // `Done` is returned the moment it is heard, below.
                Next::Done => return Err(ClientError::Internal("Done before any answer")),
                Next::Refused => {
                    return Ok(done(
                        request_id.map_or_else(
                            || Outcome::Unresolved(unresolved(&retry, None, Exhausted::Attempts)),
                            |request_id| Outcome::Refused { request_id },
                        ),
                        notes,
                    ));
                }
                Next::Unresolved(exhausted) => {
                    return Ok(done(
                        Outcome::Unresolved(unresolved(&retry, request_id, exhausted)),
                        notes,
                    ));
                }
            };
            self.clock.sleep_until_ms(at_ms);
            let Some(address) = self.settings.address(node) else {
                return Err(ClientError::NoNodes);
            };
            let status_request;
            let (request, expecting) = if asking {
                status_request = request_id.map(|id| HttpRequest {
                    method: "GET",
                    target: format!("/v1/requests/{id}"),
                    headers: Vec::new(),
                    body: Vec::new(),
                });
                (status_request.as_ref().unwrap_or(operation), Expect::Status)
            } else {
                (operation, expect)
            };
            let remaining = self.remaining_ms(started);
            let heard = match hear(
                self.exchange(address, request, remaining),
                expecting,
                (&ids, node),
                asking,
                &mut notes,
            ) {
                Ok(heard) => heard,
                Err(rejection) => return Ok(done(Outcome::Rejected(rejection), notes)),
            };
            // Sends have happened, so a failure from here on is not a refusal to start: the
            // write may have executed, and it is reported as unresolved with its ID (§8.7).
            let Ok(draw) = self.draw() else {
                note(
                    &mut notes,
                    "no randomness to choose the next send".to_owned(),
                );
                return Ok(done(
                    Outcome::Unresolved(unresolved(&retry, request_id, Exhausted::Attempts)),
                    notes,
                ));
            };
            let now = self.clock.now_ms();
            next = match &heard {
                Some((decoded, _)) => retry.heard(Heard::Answer(decoded), now, draw),
                None => retry.heard(Heard::Silence, now, draw),
            };
            if matches!(next, Next::Done)
                && let Some((decoded, Some(response))) = heard
            {
                return Ok(done(
                    Outcome::Answered(answer(decoded, &response, node, asking)),
                    notes,
                ));
            }
        }
    }
}

/// What one exchange amounts to: an answer (with its HTTP response, if one came), silence
/// (`None`), or a rejection that ends the operation. A connect failure is a refusal, since
/// nothing reached the node (A-5); anything stranger than that is silence.
fn hear(
    result: Result<HttpResponse, TransportError>,
    expecting: Expect<'_>,
    (ids, node): (&[NodeId], NodeId),
    asking: bool,
    notes: &mut Vec<String>,
) -> Result<Option<(Response, Option<HttpResponse>)>, Rejection> {
    let response = match result {
        Err(TransportError::NotSent(why)) => {
            note(notes, format!("node {node}: {why}"));
            return Ok(Some((Response::Refused(Refusal::Unavailable), None)));
        }
        Err(TransportError::Unknown(why)) => {
            note(notes, format!("node {node}: {why}"));
            return Ok(None);
        }
        Ok(response) => response,
    };
    match decode(&response, expecting, ids, node) {
        Ok(Decoded::Answer(decoded)) if !fits(expecting, &decoded) => {
            note(
                notes,
                format!("node {node}: an answer that is not this operation's"),
            );
            Ok(None)
        }
        Ok(Decoded::Answer(decoded)) => Ok(Some((decoded, Some(response)))),
        // A refused status lookup says nothing about the write it asks after (§8.4): an
        // earlier send may have committed.
        Ok(Decoded::Rejected(rejection)) if asking => {
            note(
                notes,
                format!(
                    "node {node}: the status lookup was refused: {} {}",
                    rejection.status, rejection.code
                ),
            );
            Ok(None)
        }
        Ok(Decoded::Rejected(rejection)) => Err(rejection),
        Err(err) => {
            note(notes, format!("node {node}: {err}"));
            Ok(None)
        }
    }
}

/// Whether `response` can be the answer to what `expecting` asked. Refusals fit anything;
/// a terminal answer to a different operation is a stray, not a result.
const fn fits(expecting: Expect<'_>, response: &Response) -> bool {
    match (expecting, response) {
        (_, Response::Refused(_))
        | (Expect::Write, Response::Written { .. } | Response::Conflict { .. })
        | (Expect::Get, Response::Found { .. } | Response::NotFound { .. })
        | (Expect::Scan(_), Response::Page(_))
        | (Expect::Status, Response::Status { .. }) => true,
        (
            Expect::Write | Expect::Get | Expect::Scan(_) | Expect::Status,
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. },
        ) => false,
    }
}

/// The request of a write `record`.
fn write_request(record: &Record) -> HttpRequest {
    let mut headers = vec![("Idempotency-Key", record.request_id.to_string())];
    if !record.body.is_empty() {
        headers.push(("Content-Type", "application/json".to_owned()));
    }
    HttpRequest {
        method: record.method.as_str(),
        target: record.target.clone(),
        headers,
        body: record.body.clone().into_bytes(),
    }
}

fn answer(
    response: Response,
    http: &HttpResponse,
    node: NodeId,
    via_status_lookup: bool,
) -> Answer {
    Answer {
        response,
        http_status: http.status,
        body: serde_json::from_slice(&http.body).unwrap_or(Json::Null),
        node,
        via_status_lookup,
    }
}

fn unresolved(retry: &Retry, request_id: Option<RequestId>, exhausted: Exhausted) -> Unresolved {
    Unresolved {
        request_id,
        exhausted,
        sends: retry.sends(),
        nodes_tried: retry.tried().to_vec(),
    }
}

const fn done(outcome: Outcome, notes: Vec<String>) -> Report {
    Report {
        outcome,
        notes,
        leftover: None,
    }
}

fn note(notes: &mut Vec<String>, text: String) {
    if notes.len() < MAX_NOTES {
        notes.push(text);
    }
}

#[cfg(test)]
mod tests;
