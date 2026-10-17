//! The client operations against a scripted network, a virtual clock and a scripted source
//! of randomness (W33.2): the real client, retry policy and pending store, with only the
//! network, time and entropy faked.

#![cfg(test)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::net::SocketAddr;
use std::rc::Rc;

use serde_json::{Value as Json, json};
use types::{
    Exhausted, ExpectedVersion, FIRST_BACKOFF_MS, Key, KeyValueLimits, NodeId, PageSize, RequestId,
    RequestState, Response, RetryBounds, ScanRange, Value, Version,
};

use super::http::{HttpRequest, HttpResponse, Transport, TransportError, percent_encode};
use super::*;

/// A draw that makes the policy resend a write rather than ask the status lookup, and
/// retry the node it last used.
const RESEND: u64 = 1 << 24;
/// A draw that makes the policy ask the status lookup after an ambiguous outcome.
const ASK: u64 = 0;

fn addr(node: u64) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], u16::try_from(7000 + node).unwrap()))
}

fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

fn settings(attempts: u64, nodes: u64, deadline_ms: u64) -> Settings {
    Settings {
        nodes: (1..=5).map(|id| (node(id), addr(id))).collect(),
        bounds: RetryBounds::new(attempts, nodes, deadline_ms),
        max_nodes: usize::try_from(nodes).unwrap(),
        deadline_ms,
    }
}

fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

/// The scripted network: answers in order, and logs every send.
struct Net {
    script: RefCell<VecDeque<Result<HttpResponse, TransportError>>>,
    log: RefCell<Vec<(SocketAddr, HttpRequest)>>,
    /// The pending store, checked at every write send: the record must already be durable.
    store: Option<PendingStore>,
}

impl Net {
    fn new(store: Option<PendingStore>) -> Rc<Self> {
        Rc::new(Self {
            script: RefCell::new(VecDeque::new()),
            log: RefCell::new(Vec::new()),
            store,
        })
    }

    fn then(&self, answer: Result<HttpResponse, TransportError>) -> &Self {
        self.script.borrow_mut().push_back(answer);
        self
    }

    fn reply(&self, status: u16, body: &Json) -> &Self {
        self.then(Ok(HttpResponse {
            status,
            location: None,
            body: body.to_string().into_bytes(),
        }))
    }

    fn sends(&self) -> Vec<(SocketAddr, HttpRequest)> {
        self.log.borrow().clone()
    }
}

impl Transport for Rc<Net> {
    fn send(
        &self,
        node: SocketAddr,
        request: &HttpRequest,
        _connect: Duration,
        _response: Duration,
    ) -> Result<HttpResponse, TransportError> {
        if let Some((_, id)) = request
            .headers
            .iter()
            .find(|(name, _)| *name == "Idempotency-Key")
            && let Some(store) = &self.store
        {
            let id = RequestId::parse(id).unwrap();
            let record = store
                .load(id)
                .unwrap()
                .unwrap_or_else(|| panic!("rule: a write is persisted before it is sent (O-4); {id} was not, at send to {node}"));
            assert_eq!(
                (
                    record.method.as_str(),
                    &record.target,
                    record.body.as_bytes()
                ),
                (request.method, &request.target, request.body.as_slice()),
                "rule: the persisted command is exactly what is sent (O-4); id={id}"
            );
        }
        self.log.borrow_mut().push((node, request.clone()));
        self.script.borrow_mut().pop_front().unwrap_or_else(|| {
            panic!(
                "the script ran out at the send {} to {node}",
                self.log.borrow().len()
            )
        })
    }
}

/// A virtual clock; sleeping advances it, and every sleep is logged.
#[derive(Default)]
struct Virtual {
    now: Cell<u64>,
    sleeps: RefCell<Vec<u64>>,
}

impl Clock for Rc<Virtual> {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn sleep_until_ms(&self, at_ms: u64) {
        if at_ms > self.now.get() {
            self.sleeps.borrow_mut().push(at_ms - self.now.get());
            self.now.set(at_ms);
        }
    }
}

/// Request IDs counting up from 1; draws taken from a queue, then `fallback`.
struct Scripted {
    next_id: u128,
    draws: VecDeque<u64>,
    fallback: u64,
    /// Fail every draw once the queue of draws is empty.
    fail_when_empty: bool,
}

impl Scripted {
    fn new(draws: &[u64], fallback: u64) -> Self {
        Self {
            next_id: 0xA0,
            draws: draws.iter().copied().collect(),
            fallback,
            fail_when_empty: false,
        }
    }
}

impl RandomSource for Scripted {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), String> {
        if bytes.len() == 16 {
            self.next_id += 1;
            bytes.copy_from_slice(&self.next_id.to_be_bytes());
        } else {
            let Some(draw) = self
                .draws
                .pop_front()
                .or_else(|| (!self.fail_when_empty).then_some(self.fallback))
            else {
                return Err("entropy source failed".to_owned());
            };
            bytes.copy_from_slice(&draw.to_le_bytes());
        }
        Ok(())
    }
}

type TestClient = Client<Rc<Net>, Rc<Virtual>, Scripted>;

struct Rig {
    net: Rc<Net>,
    clock: Rc<Virtual>,
    store: PendingStore,
    client: TestClient,
}

/// A client over five nodes. The first draw picks the first node: node 1.
fn rig(name: &str, settings: Settings, later_draws: &[u64], fallback: u64) -> Rig {
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .parent()
        .unwrap()
        .join("keyval-scratch")
        .join(format!("client-{}-{name}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let store = PendingStore::open(&dir).unwrap();
    let net = Net::new(Some(store.clone()));
    let clock = Rc::new(Virtual::default());
    let mut draws = vec![0];
    draws.extend_from_slice(later_draws);
    let client = Client::new(
        settings,
        Rc::clone(&net),
        Rc::clone(&clock),
        Scripted::new(&draws, fallback),
        Some(store.clone()),
    );
    Rig {
        net,
        clock,
        store,
        client,
    }
}

fn served(node: u64, applied: u64) -> Json {
    json!({"node_id": node, "applied_index": applied})
}

fn put_ok(id: &str, index: u64, replayed: bool) -> Json {
    json!({"request_id": id, "index": index, "version": index, "replayed": replayed,
        "served_by": served(1, index)})
}

fn error(code: &str, message: &str, details: &Json) -> Json {
    json!({"error": {"code": code, "message": message, "details": details}})
}

fn id_text(id: RequestId) -> String {
    id.to_string()
}

fn answered(report: &Report) -> &Answer {
    match &report.outcome {
        Outcome::Answered(answer) => answer,
        other => panic!(
            "expected a terminal answer, got {other:?}; notes={:?}",
            report.notes
        ),
    }
}

// check: CLIENT-OPS
#[test]
fn request_ids_are_32_lowercase_hex_and_new_for_each_operation() {
    let mut r = rig("ids", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .reply(200, &put_ok("00000000000000000000000000000000", 5, false));
    r.net
        .reply(200, &put_ok("00000000000000000000000000000000", 6, false));
    let (_, first) = r.client.put(&key("a"), &value("1")).unwrap();
    let (_, second) = r.client.put(&key("a"), &value("1")).unwrap();
    for id in [first, second] {
        let text = id_text(id);
        assert_eq!(text.len(), 32, "rule: an ID is 32 characters; id={text}");
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "rule: an ID is lowercase hex; id={text}"
        );
    }
    assert_ne!(first, second, "rule: a new operation takes a new ID");
}

#[test]
fn a_put_is_persisted_before_it_is_sent_and_deleted_after_its_terminal_result() {
    let mut r = rig("persist", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(200, &put_ok("x", 9, false));
    let (report, id) = r.client.put(&key("greeting"), &value("hello")).unwrap();
    // Net::send asserted at the send that the record was already durable and identical.
    let sends = r.net.sends();
    assert_eq!(sends.len(), 1, "one send; sends={sends:?}");
    let request = &sends[0].1;
    assert_eq!(
        (
            request.method,
            request.target.as_str(),
            request.body.as_slice()
        ),
        (
            "PUT",
            "/v1/kv?key=greeting",
            br#"{"value":"hello"}"#.as_slice()
        ),
        "rule: the PUT command (F-1)"
    );
    assert!(
        request.headers.contains(&("Idempotency-Key", id_text(id))),
        "rule: the ID is the Idempotency-Key (F-6); headers={:?}",
        request.headers
    );
    assert_eq!(answered(&report).http_status, 200);
    assert_eq!(
        r.store.list().unwrap(),
        vec![],
        "rule: deleted after a terminal result (O-4)"
    );
    assert_eq!(report.leftover, None);
}

#[test]
fn delete_and_cas_send_their_commands() {
    let mut r = rig("commands", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        200,
        &json!({"request_id": "x", "index": 4, "existed": true, "replayed": false,
            "served_by": served(1, 4)}),
    );
    r.net.reply(
        409,
        &error(
            "version_mismatch",
            "expected 41, current 57",
            &json!({"current_version": 57, "request_id": "x", "index": 58, "replayed": false}),
        ),
    );
    r.client.delete(&key("k")).unwrap();
    let (mismatch, _) = r
        .client
        .cas(
            &key("counter"),
            ExpectedVersion::Version(Version::new(41).unwrap()),
            &value("12"),
        )
        .unwrap();
    let sends = r.net.sends();
    assert_eq!(
        (
            sends[0].1.method,
            sends[0].1.target.as_str(),
            sends[0].1.body.len()
        ),
        ("DELETE", "/v1/kv?key=k", 0),
        "rule: DELETE takes no body"
    );
    let body: Json = serde_json::from_slice(&sends[1].1.body).unwrap();
    assert_eq!(
        (sends[1].1.method, sends[1].1.target.as_str(), body),
        (
            "POST",
            "/v1/cas?key=counter",
            json!({"expected": {"kind": "version", "version": 41}, "value": "12"})
        ),
        "rule: the CAS command (F-5)"
    );
    assert_eq!(
        sends.len(),
        2,
        "version_mismatch is terminal: not retried; sends={sends:?}"
    );
    assert_eq!(answered(&mismatch).http_status, 409);
    assert!(
        r.store.list().unwrap().is_empty(),
        "rule: a consumed ID's record is deleted"
    );
}

#[test]
fn a_307_is_followed_to_the_leader_with_the_same_id_and_bytes() {
    let mut r = rig("redirect", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        307,
        &error(
            "not_leader",
            "node 4 leads",
            &json!({"leader_hint": 4, "request_id": "x", "attempt_state": "not_submitted"}),
        ),
    );
    r.net.reply(200, &put_ok("x", 12, false));
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(
        sends.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        vec![addr(1), addr(4)],
        "rule: after not_leader the next send goes to the named leader (§8.4)"
    );
    assert_eq!(
        sends[0].1, sends[1].1,
        "rule: same ID, same bytes on every send (C-6)"
    );
    assert!(
        sends[1]
            .1
            .headers
            .contains(&("Idempotency-Key", id_text(id)))
    );
    assert_eq!(answered(&report).node, node(4));
    assert!(
        r.clock.sleeps.borrow().is_empty(),
        "rule: a redirect is followed at once; sleeps={:?}",
        r.clock.sleeps
    );
}

#[test]
fn busy_backs_off_and_retries_the_same_node_with_the_same_id() {
    let mut r = rig("busy", settings(10, 5, 15_000), &[], RESEND);
    let busy = error(
        "busy",
        "slow down",
        &json!({"request_id": "x", "attempt_state": "not_submitted"}),
    );
    r.net.reply(429, &busy);
    r.net.reply(429, &busy);
    r.net.reply(200, &put_ok("x", 3, false));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 3, "sends={sends:?}");
    assert!(
        sends.iter().all(|(a, r)| *a == addr(1) && r == &sends[0].1),
        "rule: busy retries the same node with the same request; sends={sends:?}"
    );
    let sleeps = r.clock.sleeps.borrow().clone();
    assert_eq!(
        sleeps.len(),
        2,
        "one backoff per refusal; sleeps={sleeps:?}"
    );
    assert!(
        (FIRST_BACKOFF_MS / 2..=FIRST_BACKOFF_MS).contains(&sleeps[0])
            && (FIRST_BACKOFF_MS..=2 * FIRST_BACKOFF_MS).contains(&sleeps[1]),
        "rule: backoff waits half to all of {FIRST_BACKOFF_MS} ms, then of double that; sleeps={sleeps:?}"
    );
    assert_eq!(answered(&report).http_status, 200);
}

#[test]
fn unavailable_and_recovering_back_off_and_try_another_node() {
    // Draw 1 << 16 steers `other()` to a different node than the last.
    let mut r = rig("other", settings(10, 5, 15_000), &[RESEND, RESEND], RESEND);
    r.net.reply(
        503,
        &error(
            "unavailable",
            "no leader",
            &json!({"request_id": "x", "attempt_state": "not_submitted", "nodes": []}),
        ),
    );
    r.net.reply(
        503,
        &error("recovering", "not ready", &json!({"lifecycle": "recovering", "request_id": "x", "attempt_state": "not_submitted"})),
    );
    r.net.reply(200, &put_ok("x", 3, false));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 3);
    assert_ne!(
        sends[1].0, sends[0].0,
        "rule: unavailable moves to another node; sends={sends:?}"
    );
    assert_ne!(
        sends[2].0, sends[1].0,
        "rule: recovering moves to another node; sends={sends:?}"
    );
    assert_eq!(r.clock.sleeps.borrow().len(), 2, "rule: both back off");
    assert_eq!(answered(&report).http_status, 200);
}

#[test]
fn unknown_outcome_resends_the_same_id_and_command() {
    let mut r = rig("unknown", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 2, "sends={sends:?}");
    assert_eq!(
        sends[0].1, sends[1].1,
        "rule: the resend is the same ID and bytes (C-6)"
    );
    assert!(
        sends[1]
            .1
            .headers
            .contains(&("Idempotency-Key", id_text(id)))
    );
    let Response::Written { replayed, .. } = &answered(&report).response else {
        panic!("expected a written result, got {report:?}");
    };
    assert!(*replayed, "the retry was a replay of the first execution");
    assert_eq!(
        r.store.list().unwrap(),
        vec![],
        "rule: a resolved write's record is deleted"
    );
}

#[test]
fn a_lost_response_is_resent_with_the_same_id_never_a_new_one() {
    let mut r = rig("lost", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .then(Err(TransportError::Unknown("connection reset".to_owned())));
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 2, "sends={sends:?}");
    assert_eq!(
        sends[0].1, sends[1].1,
        "rule: same ID and bytes after silence (A-5)"
    );
    assert!(
        sends[1]
            .1
            .headers
            .contains(&("Idempotency-Key", id_text(id)))
    );
    assert_eq!(answered(&report).http_status, 200);
    assert!(
        report.notes.iter().any(|n| n.contains("connection reset")),
        "the person is told what happened; notes={:?}",
        report.notes
    );
}

#[test]
fn the_status_lookup_resolves_an_ambiguous_write_when_the_policy_asks_it() {
    let mut r = rig("ask", settings(10, 5, 15_000), &[], ASK);
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    r.net.reply(
        200,
        &json!({"state": "known", "result": put_ok("x", 8, true), "served_by": served(1, 9)}),
    );
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(
        sends[1].1.target,
        format!("/v1/requests/{id}"),
        "rule: the second send is the status lookup; sends={sends:?}"
    );
    let answer = answered(&report);
    assert!(answer.via_status_lookup, "answer={answer:?}");
    assert!(
        matches!(
            &answer.response,
            Response::Status {
                state: RequestState::Known(_),
                ..
            }
        ),
        "answer={answer:?}"
    );
    assert!(
        r.store.list().unwrap().is_empty(),
        "rule: a known result is terminal"
    );
}

#[test]
fn a_status_of_unknown_is_never_a_failure_so_the_write_is_resent() {
    let mut r = rig(
        "unknown-status",
        settings(10, 5, 15_000),
        &[ASK, RESEND],
        RESEND,
    );
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    r.net
        .reply(200, &json!({"state": "unknown", "served_by": served(1, 2)}));
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 3, "sends={sends:?}");
    assert_eq!(
        sends[2].1, sends[0].1,
        "rule: unknown means resend the same command (P8)"
    );
    assert!(
        sends[2]
            .1
            .headers
            .contains(&("Idempotency-Key", id_text(id)))
    );
    assert_eq!(answered(&report).http_status, 200);
}

#[test]
fn exhausting_the_attempt_bound_is_unresolved_with_the_id_and_keeps_the_record() {
    let mut r = rig("attempts", settings(10, 5, 60_000), &[], RESEND);
    for _ in 0..10 {
        r.net.reply(
            429,
            &error(
                "busy",
                "slow down",
                &json!({"request_id": "x", "attempt_state": "not_submitted"}),
            ),
        );
    }
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(
        sends.len(),
        10,
        "rule: at most 10 sends (§7.11); sends={}",
        sends.len()
    );
    let Outcome::Unresolved(unresolved) = &report.outcome else {
        panic!("rule: exhausted bounds are unresolved, never failed (§8.7); got {report:?}");
    };
    assert_eq!(unresolved.request_id, Some(id));
    assert_eq!(unresolved.exhausted, Exhausted::Attempts);
    assert_eq!(unresolved.sends, 10);
    assert_eq!(
        r.store
            .list()
            .unwrap()
            .iter()
            .map(|rec| rec.request_id)
            .collect::<Vec<_>>(),
        vec![id],
        "rule: an unresolved write's record stays for resume (O-4)"
    );
}

#[test]
fn exhausting_the_node_bound_is_unresolved() {
    // Two nodes allowed; `unavailable` keeps moving between them and never to a third.
    let mut r = rig(
        "nodes",
        settings(10, 2, 60_000),
        &[0, 1 << 16, 1 << 16, 1 << 16, 1 << 16],
        RESEND,
    );
    for _ in 0..10 {
        r.net.reply(
            503,
            &error(
                "unavailable",
                "no leader",
                &json!({"request_id": "x", "attempt_state": "not_submitted", "nodes": []}),
            ),
        );
    }
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let nodes: std::collections::BTreeSet<SocketAddr> =
        r.net.sends().iter().map(|(a, _)| *a).collect();
    assert!(
        nodes.len() == 2,
        "rule: the sends move between, and stay within, the 2 nodes allowed (§7.11); nodes={nodes:?}"
    );
    assert_eq!(
        r.net.sends().len(),
        10,
        "the node bound confines sends, the attempt bound ends them"
    );
    let Outcome::Unresolved(unresolved) = &report.outcome else {
        panic!("expected unresolved, got {report:?}");
    };
    assert_eq!(unresolved.request_id, Some(id));
    assert!(
        unresolved.nodes_tried.len() == 2,
        "unresolved={unresolved:?}"
    );
    assert_eq!(
        unresolved.exhausted,
        Exhausted::Attempts,
        "unresolved={unresolved:?}"
    );
}

#[test]
fn exhausting_the_deadline_is_unresolved() {
    let mut r = rig("deadline", settings(10, 5, 400), &[], RESEND);
    for _ in 0..10 {
        r.net.reply(
            504,
            &error(
                "unknown_outcome",
                "not observed",
                &json!({"request_id": "x", "attempt_state": "submitted",
                "guidance": "retry_same_request_or_query_status", "nodes": []}),
            ),
        );
    }
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let Outcome::Unresolved(unresolved) = &report.outcome else {
        panic!("expected unresolved, got {report:?}");
    };
    assert_eq!(unresolved.request_id, Some(id));
    assert_eq!(
        unresolved.exhausted,
        Exhausted::Deadline,
        "unresolved={unresolved:?}"
    );
    assert!(
        r.clock.now.get() < 400,
        "rule: no send falls at or past the deadline; now={}",
        r.clock.now.get()
    );
    assert!(!r.store.list().unwrap().is_empty(), "the record stays");
    let sends = r.net.sends().len();
    assert!(
        (2..10).contains(&sends),
        "rule: the deadline, not the attempt bound, ended it after some sends; sends={sends}"
    );
}

#[test]
fn history_full_is_refused_only_when_no_send_may_have_been_submitted() {
    let full = error(
        "history_full",
        "full",
        &json!({"request_id": "x", "limit": "max_log_entries", "attempt_state": "not_submitted"}),
    );
    // A refused connect sent nothing, so history_full afterwards is a refusal.
    let mut r = rig(
        "full-clean",
        settings(10, 5, 15_000),
        &[RESEND, RESEND],
        RESEND,
    );
    r.net.then(Err(TransportError::NotSent(
        "connection refused".to_owned(),
    )));
    r.net.reply(507, &full);
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Refused { request_id: id },
        "rule: history_full with nothing submitted did not execute; report={report:?}"
    );
    assert_eq!(
        r.store.list().unwrap().len(),
        1,
        "the record is kept for resume"
    );

    // After an unknown_outcome a send may have been submitted: not a refusal.
    let mut r = rig(
        "full-maybe",
        settings(10, 5, 15_000),
        &[RESEND, RESEND],
        RESEND,
    );
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    r.net.reply(507, &full);
    r.net.reply(
        200,
        &json!({"state": "known", "result": put_ok("x", 8, true), "served_by": served(1, 9)}),
    );
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Answered(_)),
        "rule: after a possibly submitted send only the status lookup can resolve it (§10.2); report={report:?}"
    );
}

#[test]
fn a_refused_request_is_terminal_and_not_retried() {
    let mut r = rig("reject", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .reply(400, &error("invalid_request", "bad key", &json!({})));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert_eq!(
        r.net.sends().len(),
        1,
        "rule: the same bytes would be refused again"
    );
    let Outcome::Rejected(rejection) = &report.outcome else {
        panic!("expected a rejection, got {report:?}");
    };
    assert_eq!(
        (rejection.status, rejection.code.as_str()),
        (400, "invalid_request")
    );
    assert!(
        r.store.list().unwrap().is_empty(),
        "it can never have executed"
    );
}

#[test]
fn request_id_conflict_is_terminal() {
    let mut r = rig("conflict", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        409,
        &error(
            "request_id_conflict",
            "used",
            &json!({"request_id": "x", "original_index": 7}),
        ),
    );
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert_eq!(r.net.sends().len(), 1);
    assert!(
        matches!(answered(&report).response, Response::Conflict { .. }),
        "report={report:?}"
    );
}

#[test]
fn an_unreadable_answer_is_silence_and_the_write_is_resent_under_its_id() {
    let mut r = rig("garbage", settings(10, 5, 15_000), &[], RESEND);
    r.net.then(Ok(HttpResponse {
        status: 200,
        location: None,
        body: b"<html>".to_vec(),
    }));
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].1, sends[1].1, "rule: same ID and bytes");
    assert!(
        report.notes.iter().any(|n| n.contains("not JSON")),
        "notes={:?}",
        report.notes
    );
}

#[test]
fn a_307_naming_an_unknown_node_is_unreadable_not_followed() {
    let mut r = rig("badleader", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        307,
        &error(
            "not_leader",
            "x",
            &json!({"leader_hint": 9, "request_id": "x", "attempt_state": "not_submitted"}),
        ),
    );
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("not a node of this cluster")),
        "notes={:?}",
        report.notes
    );
}

#[test]
fn get_reads_found_and_not_found() {
    let mut r = rig("get", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        200,
        &json!({"key": "a b", "value": "v", "version": 5, "served_by": served(1, 6)}),
    );
    r.net.reply(
        404,
        &error("not_found", "absent", &json!({"served_by": served(1, 6)})),
    );
    let found = r.client.get(&key("a b")).unwrap();
    let missing = r.client.get(&key("zzz")).unwrap();
    assert!(
        matches!(answered(&found).response, Response::Found { .. }),
        "found={found:?}"
    );
    assert!(
        matches!(answered(&missing).response, Response::NotFound { .. }),
        "missing={missing:?}"
    );
    let sends = r.net.sends();
    assert_eq!(
        sends[0].1.target, "/v1/kv?key=a%20b",
        "rule: keys are percent-encoded (§8.1)"
    );
    assert!(
        sends[0].1.headers.is_empty(),
        "a read carries no request ID"
    );
    assert!(
        r.store.list().unwrap().is_empty(),
        "a read persists nothing"
    );
}

#[test]
fn a_read_busy_is_retried_and_exhaustion_is_reported_without_an_id() {
    let mut r = rig("read-busy", settings(3, 5, 60_000), &[], RESEND);
    for _ in 0..3 {
        r.net.reply(429, &error("busy", "slow", &json!({})));
    }
    let report = r.client.get(&key("k")).unwrap();
    assert_eq!(
        r.net.sends().len(),
        3,
        "rule: reads obey the attempt bound too"
    );
    let Outcome::Unresolved(unresolved) = &report.outcome else {
        panic!("expected unresolved, got {report:?}");
    };
    assert_eq!(unresolved.request_id, None, "a read has no request ID");
}

fn scan_range() -> ScanRange {
    ScanRange::new(
        Some(key("a")),
        Some(key("z")),
        None,
        PageSize::new(2).unwrap(),
    )
    .unwrap()
}

#[test]
fn scan_reads_a_page_and_sends_the_range() {
    let mut r = rig("scan", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        200,
        &json!({"items": [
            {"key": "b", "value": "1", "version": 3},
            {"key": "c", "value": "2", "version": 4}],
            "has_more": true, "next_after": "c", "served_by": served(1, 9)}),
    );
    let report = r.client.scan(&scan_range()).unwrap();
    let Response::Page(page) = &answered(&report).response else {
        panic!("expected a page, got {report:?}");
    };
    assert_eq!(page.items().len(), 2);
    assert_eq!(page.continuation().map(Key::as_str), Some("c"));
    assert_eq!(
        r.net.sends()[0].1.target,
        "/v1/scan?limit=2&start=a&end=z",
        "rule: the range is sent as query parameters (§8.5)"
    );
}

#[test]
fn a_scan_page_that_breaks_the_spec_is_unreadable_and_never_panics() {
    let bad_pages = [
        // out of order
        json!({"items": [{"key": "c", "value": "1", "version": 3}, {"key": "b", "value": "1", "version": 3}],
            "has_more": false, "next_after": null, "served_by": served(1, 9)}),
        // outside the range
        json!({"items": [{"key": "zz", "value": "1", "version": 3}],
            "has_more": false, "next_after": null, "served_by": served(1, 9)}),
        // too many items
        json!({"items": [{"key": "b", "value": "1", "version": 3}, {"key": "c", "value": "1", "version": 3},
            {"key": "d", "value": "1", "version": 3}], "has_more": false, "next_after": null, "served_by": served(1, 9)}),
        // continuation is not the last key
        json!({"items": [{"key": "b", "value": "1", "version": 3}],
            "has_more": true, "next_after": "x", "served_by": served(1, 9)}),
        // has_more disagrees with next_after
        json!({"items": [{"key": "b", "value": "1", "version": 3}],
            "has_more": true, "next_after": null, "served_by": served(1, 9)}),
    ];
    for (n, page) in bad_pages.iter().enumerate() {
        let response = HttpResponse {
            status: 200,
            location: None,
            body: page.to_string().into_bytes(),
        };
        let decoded = decode(&response, Expect::Scan(&scan_range()), &[node(1)], node(1));
        assert!(
            decoded.is_err(),
            "rule: a bad page is a DecodeError; case {n}: {page}; got {decoded:?}"
        );
    }
}

#[test]
fn status_asks_nodes_in_order_and_stops_at_the_first_that_knows() {
    let mut r = rig("status", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .reply(200, &json!({"state": "unknown", "served_by": served(1, 2)}));
    r.net
        .then(Err(TransportError::NotSent("refused".to_owned())));
    r.net.reply(
        200,
        &json!({"state": "known", "result": put_ok("x", 8, true), "served_by": served(3, 9)}),
    );
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let report = r.client.status(id).unwrap();
    let sends = r.net.sends();
    assert_eq!(
        sends.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        vec![addr(1), addr(2), addr(3)],
        "rule: nodes in configured order, stopping at the first that knows"
    );
    assert_eq!(report.unknown_on, vec![node(1)]);
    assert_eq!(
        report.known.as_ref().map(|a| a.node),
        Some(node(3)),
        "report={report:?}"
    );
    assert!(
        report.notes.iter().any(|n| n.contains("node 2")),
        "notes={:?}",
        report.notes
    );
}

#[test]
fn status_unknown_everywhere_is_reported_as_unknown_not_as_failure() {
    let mut r = rig("status-unknown", settings(10, 3, 15_000), &[], RESEND);
    for n in 1..=3 {
        r.net
            .reply(200, &json!({"state": "unknown", "served_by": served(n, 2)}));
    }
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let report = r.client.status(id).unwrap();
    assert_eq!(report.known, None);
    assert_eq!(report.unknown_on, vec![node(1), node(2), node(3)]);
    assert_eq!(
        r.net.sends().len(),
        3,
        "rule: at most the node bound of nodes is asked (§7.11)"
    );
}

#[test]
fn cluster_reads_from_the_first_node_that_answers() {
    let mut r = rig("cluster", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .then(Err(TransportError::NotSent("refused".to_owned())));
    r.net.reply(200, &json!({"node_id": 2, "role": "follower"}));
    let report = r.client.cluster().unwrap();
    assert_eq!(report.node, node(2));
    assert_eq!(report.body["role"], "follower");
    assert_eq!(r.net.sends()[0].1.target, "/v1/cluster");
    assert_eq!(
        report.notes.len(),
        1,
        "the unreachable node is noted; notes={:?}",
        report.notes
    );
}

#[test]
fn a_store_that_refuses_stops_the_write_before_anything_is_sent() {
    let r = rig("storefull", settings(10, 5, 15_000), &[], RESEND);
    // A store whose bounds admit no record refuses every persist.
    let tiny = r.store.clone().with_bounds(0, 1);
    let net = Net::new(None);
    let mut client = Client::new(
        settings(10, 5, 15_000),
        Rc::clone(&net),
        Rc::clone(&r.clock),
        Scripted::new(&[0], RESEND),
        Some(tiny),
    );
    let err = client.put(&key("k"), &value("v")).unwrap_err();
    assert!(matches!(err, ClientError::Pending(_)), "err={err}");
    assert!(
        net.sends().is_empty(),
        "rule: nothing is sent unless the record is durable (O-4)"
    );
}

#[test]
fn percent_encoding_follows_rfc_3986() {
    assert_eq!(percent_encode("a b+c"), "a%20b%2Bc");
    assert_eq!(percent_encode("Az09-._~"), "Az09-._~");
    assert_eq!(
        percent_encode("é"),
        "%C3%A9",
        "multibyte UTF-8 is encoded byte by byte"
    );
    assert_eq!(percent_encode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
}

#[test]
fn the_tcp_transport_exchanges_one_request_with_a_real_socket() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !String::from_utf8_lossy(&received).contains("\"v\"}") {
            let n = stream.read(&mut chunk).unwrap();
            received.extend_from_slice(&chunk[..n]);
        }
        let body = br#"{"ok":true}"#;
        write!(
            stream,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        String::from_utf8(received).unwrap()
    });
    let request = HttpRequest {
        method: "PUT",
        target: "/v1/kv?key=a%20b".to_owned(),
        headers: vec![(
            "Idempotency-Key",
            "0123456789abcdef0123456789abcdef".to_owned(),
        )],
        body: br#"{"value":"v"}"#.to_vec(),
    };
    let response = http::TcpTransport
        .send(
            address,
            &request,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
    let received = server.join().unwrap();
    assert_eq!(response.status, 307);
    assert_eq!(response.location.as_deref(), Some("http://127.0.0.1:1/x"));
    assert_eq!(response.body, br#"{"ok":true}"#);
    assert!(
        received.starts_with("PUT /v1/kv?key=a%20b HTTP/1.1\r\n"),
        "received={received}"
    );
    assert!(received.contains("Idempotency-Key: 0123456789abcdef0123456789abcdef\r\n"));
    assert!(
        received.contains("Content-Length: 13\r\n"),
        "received={received}"
    );
}

#[test]
fn a_refused_connection_is_not_sent_and_a_broken_one_is_unknown() {
    use std::io::Write;
    use std::net::TcpListener;

    let unused = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let request = HttpRequest {
        method: "GET",
        target: "/v1/cluster".to_owned(),
        headers: vec![],
        body: vec![],
    };
    let refused = http::TcpTransport.send(
        unused,
        &request,
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    assert!(
        matches!(refused, Err(TransportError::NotSent(_))),
        "rule: nothing sent; got {refused:?}"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n{\"half\"")
            .unwrap();
    });
    let broken = http::TcpTransport.send(
        address,
        &request,
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    server.join().unwrap();
    assert!(
        matches!(broken, Err(TransportError::Unknown(_))),
        "rule: may have been received; got {broken:?}"
    );
}

#[test]
fn capacity_exceeded_is_a_terminal_answer() {
    let mut r = rig("capacity", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        507,
        &error(
            "capacity_exceeded",
            "full",
            &json!({"request_id": "x", "limit": "max_live_keys", "index": 4, "replayed": false}),
        ),
    );
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert_eq!(
        r.net.sends().len(),
        1,
        "rule: an executed refusal is not resent"
    );
    assert_eq!(answered(&report).http_status, 507, "report={report:?}");
    assert_eq!(
        r.store.list().unwrap(),
        vec![],
        "rule: terminal, so the record is deleted"
    );
}

/// Serves `reply` bytes to one connection and returns what the transport made of it.
fn raw_answer(reply: Vec<u8>) -> Result<HttpResponse, TransportError> {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut sink = [0_u8; 1024];
        let _ = stream.read(&mut sink);
        let _ = stream.write_all(&reply);
    });
    let request = HttpRequest {
        method: "GET",
        target: "/v1/cluster".to_owned(),
        headers: vec![],
        body: vec![],
    };
    let answer = http::TcpTransport.send(
        address,
        &request,
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    server.join().unwrap();
    answer
}

#[test]
fn the_transport_refuses_what_it_cannot_bound_or_parse() {
    let oversize_body = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        http::MAX_BODY_BYTES + 1
    );
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "chunked",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        ),
        ("bad status line", b"BANANA\r\n\r\n".to_vec()),
        (
            "header without colon",
            b"HTTP/1.1 200 OK\r\nnonsense\r\n\r\n".to_vec(),
        ),
        (
            "non-numeric length",
            b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n".to_vec(),
        ),
        ("oversize length", oversize_body.into_bytes()),
        ("oversize head", vec![b'a'; http::MAX_HEAD_BYTES * 2]),
        ("closed inside head", b"HTTP/1.1 200".to_vec()),
    ];
    for (name, reply) in cases {
        let answer = raw_answer(reply);
        assert!(
            matches!(answer, Err(TransportError::Unknown(_))),
            "rule: {name} is unknown, never a response; got {answer:?}"
        );
    }
}

#[test]
fn the_transport_reads_a_body_with_no_length_to_the_end_of_the_stream() {
    let answer = raw_answer(b"HTTP/1.1 200 OK\r\n\r\n{\"a\":1}".to_vec()).unwrap();
    assert_eq!(
        (answer.status, answer.body.as_slice()),
        (200, br#"{"a":1}"#.as_slice())
    );
}

#[test]
fn a_response_that_drips_is_cut_off_by_the_overall_timeout() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut sink = [0_u8; 1024];
        let _ = stream.read(&mut sink);
        for byte in b"HTTP/1.1 200 OK\r\nX-Slow: yes-and-more-and-more\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    });
    let request = HttpRequest {
        method: "GET",
        target: "/v1/cluster".to_owned(),
        headers: vec![],
        body: vec![],
    };
    let started = Instant::now();
    let answer = http::TcpTransport.send(
        address,
        &request,
        Duration::from_secs(2),
        Duration::from_millis(400),
    );
    let took = started.elapsed();
    drop(server);
    assert!(
        matches!(answer, Err(TransportError::Unknown(_))),
        "rule: a dripped response is unknown; got {answer:?}"
    );
    assert!(
        took < Duration::from_millis(1500),
        "rule: the response timeout bounds the whole read, not each read (§7.11); took={took:?}"
    );
}

#[test]
fn a_stray_answer_to_a_write_is_silence_not_a_result() {
    let mut r = rig("stray", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        404,
        &error("not_found", "absent", &json!({"served_by": served(1, 6)})),
    );
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, _) = r.client.put(&key("k"), &value("v")).unwrap();
    assert_eq!(
        r.net.sends().len(),
        2,
        "rule: a 404 is not a write's result, so it is resent"
    );
    assert!(
        matches!(answered(&report).response, Response::Written { .. }),
        "report={report:?}"
    );
}

#[test]
fn a_refused_status_lookup_does_not_end_the_write() {
    let mut r = rig(
        "lookup-refused",
        settings(10, 5, 15_000),
        &[ASK, RESEND],
        RESEND,
    );
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    r.net
        .reply(405, &error("method_not_allowed", "no", &json!({})));
    r.net.reply(200, &put_ok("x", 8, true));
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let sends = r.net.sends();
    assert_eq!(sends.len(), 3, "sends={sends:?}");
    assert_eq!(
        sends[1].1.target,
        format!("/v1/requests/{id}"),
        "sends={sends:?}"
    );
    assert_eq!(
        sends[2].1, sends[0].1,
        "rule: the write is resent under its ID after a refused lookup"
    );
    assert_eq!(answered(&report).http_status, 200, "report={report:?}");
}

#[test]
fn entropy_failing_after_a_send_leaves_the_write_unresolved_with_its_id() {
    let mut r = rig("entropy", settings(10, 5, 15_000), &[], RESEND);
    r.client.random.fail_when_empty = true;
    r.net.reply(
        504,
        &error(
            "unknown_outcome",
            "not observed",
            &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
        ),
    );
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    let Outcome::Unresolved(unresolved) = &report.outcome else {
        panic!("rule: a possibly executed write is unresolved, not an error; got {report:?}");
    };
    assert_eq!(unresolved.request_id, Some(id));
    assert_eq!(
        r.store.list().unwrap().len(),
        1,
        "rule: the record stays for resume"
    );
}
