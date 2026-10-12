//! The K-10 witness over HTTP (PLAN.md §10.5, §10.8): clients record what they invoked and
//! what they were told, drive every write to a terminal answer with bounded status lookups,
//! and the history is judged by the same K-10 the simulator uses. A history the lookups cannot
//! settle is incomplete and fails; inconclusive is never a pass (P10).

#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use checker::{Kind, Report, k10};
use model::Limits;
use serde_json::{Value, json};

use super::support::{each_target, fresh_id, fresh_prefix, index_of, say, with_model};
use crate::http::Response;
use crate::poll::{Index, Policy, PollError};
use crate::suite::support::Node;
use crate::tag::{Class, Tag};

/// Bound on records one recorder keeps (CD-9); past it the run is marked overflowed, which
/// K-10 judges incomplete.
const MAX_RECORDS: usize = 50_000;
/// Status lookups made per write before it is called unresolved.
const LOOKUPS: u32 = 20;
/// Pause between lookups of one write.
const LOOKUP_PAUSE: Duration = Duration::from_millis(50);

/// A write a client sends.
#[derive(Debug, Clone)]
enum Op {
    Put {
        key: String,
        value: String,
    },
    Delete {
        key: String,
    },
    Cas {
        key: String,
        expected: Value,
        value: String,
    },
}

impl Op {
    const fn name(&self) -> &'static str {
        match self {
            Self::Put { .. } => "put",
            Self::Delete { .. } => "delete",
            Self::Cas { .. } => "cas",
        }
    }

    fn key(&self) -> &str {
        match self {
            Self::Put { key, .. } | Self::Delete { key } | Self::Cas { key, .. } => key,
        }
    }

    /// The operation as the observation schema spells it.
    fn json(&self) -> Value {
        match self {
            Self::Put { key, value } => json!({"op": "put", "key": key, "value": value}),
            Self::Delete { key } => json!({"op": "delete", "key": key}),
            Self::Cas {
                key,
                expected,
                value,
            } => json!({"op": "cas", "key": key, "expected": expected, "value": value}),
        }
    }

    fn send(&self, node: &Node, id: &str) -> Response {
        match self {
            Self::Put { key, value } => node.put(key, value, id),
            Self::Delete { key } => node.delete(key, id),
            Self::Cas {
                key,
                expected,
                value,
            } => node.cas(key, expected, value, id),
        }
    }
}

fn blank_outcome(code: &str) -> Value {
    json!({"code": code, "request_id": null, "index": null, "version": null, "existed": null,
        "replayed": null, "served_by": null, "attempt_state": null, "current_version": null,
        "original_index": null, "limit": null, "leader_hint": null, "lifecycle": null})
}

/// A success body (or a known status result) as an observation outcome.
fn outcome_ok(body: &Value) -> Value {
    let mut out = blank_outcome("ok");
    for field in [
        "request_id",
        "index",
        "version",
        "existed",
        "replayed",
        "served_by",
    ] {
        if let Some(value) = body.get(field) {
            out[field] = value.clone();
        }
    }
    out
}

/// An error object as an observation outcome; only terminal write results appear in a
/// witness history, so any other code is a harness fault.
fn outcome_error(error: &Value) -> Value {
    let code = error["code"].as_str().unwrap_or_default();
    assert!(
        [
            "version_mismatch",
            "request_id_conflict",
            "capacity_exceeded"
        ]
        .contains(&code),
        "rule: witness traffic draws only terminal write results; error={error}"
    );
    let mut out = blank_outcome(code);
    for field in [
        "request_id",
        "index",
        "replayed",
        "current_version",
        "original_index",
        "limit",
    ] {
        if let Some(value) = error["details"].get(field) {
            out[field] = value.clone();
        }
    }
    out
}

fn outcome_of(response: &Response) -> Value {
    let body = response
        .json()
        .unwrap_or_else(|error| panic!("rule: a body is JSON; {error}; {}", say(response)));
    if response.status == 200 {
        outcome_ok(&body)
    } else {
        outcome_error(&body["error"])
    }
}

#[derive(Default)]
struct Inner {
    records: Vec<Value>,
    attempts: u64,
    overflowed: bool,
    /// The highest index any write answered, which a status lookup waits for (C-7).
    highest: u64,
    /// Logical writes, in first-invocation order.
    ids: BTreeMap<String, Value>,
}

impl Inner {
    fn push(&mut self, mut record: Value) {
        if self.records.len() >= MAX_RECORDS {
            self.overflowed = true;
            return;
        }
        let seq = self.records.len() as u64;
        record["seq"] = json!(seq);
        record["time"] = json!(seq);
        self.records.push(record);
    }
}

/// What clients observed, in the order it happened.
#[derive(Default)]
struct Recorder {
    inner: Mutex<Inner>,
}

impl Recorder {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records an invocation and returns its attempt number. An invocation under a request ID
    /// already used for another operation is misuse (F-6), judged here from what the client
    /// sent, never claimed by the caller.
    fn invoke(&self, client: u64, operation: &Value, id: Option<&str>) -> u64 {
        let mut inner = self.lock();
        inner.attempts += 1;
        let attempt = inner.attempts;
        let mut misuse = false;
        if let Some(id) = id {
            match inner.ids.get(id) {
                Some(first) => misuse = first != operation,
                None if !inner.overflowed => {
                    inner.ids.insert(id.to_owned(), operation.clone());
                }
                None => {}
            }
        }
        inner.push(
            json!({"kind": "invoke", "client": client, "attempt": attempt,
            "target": {"node": 1, "incarnation": 0}, "request_id": id, "misuse": misuse,
            "operation": operation}),
        );
        drop(inner);
        attempt
    }

    /// Records an answer the client received.
    fn answer(&self, attempt: u64, op: &str, outcome: &Value) {
        let mut inner = self.lock();
        if outcome["code"] == "ok" {
            inner.highest = inner.highest.max(outcome["index"].as_u64().unwrap_or(0));
        }
        inner.push(json!({"kind": "response", "attempt": attempt,
            "from": {"node": 1, "incarnation": 0}, "generation": attempt, "op": op,
            "outcome": outcome}));
        inner.push(json!({"kind": "delivery", "attempt": attempt, "delivered": true}));
        drop(inner);
    }

    /// Sends a write and records the exchange; returns the outcome the client saw.
    fn write(&self, client: u64, node: &Node, op: &Op, id: &str) -> Value {
        let attempt = self.invoke(client, &op.json(), Some(id));
        let outcome = outcome_of(&op.send(node, id));
        self.answer(attempt, op.name(), &outcome);
        outcome
    }

    /// One status lookup of `id`; the index of its known result, if the write is known.
    fn lookup(&self, client: u64, node: &Node, id: &str) -> Option<(u64, u64)> {
        let attempt = self.invoke(client, &json!({"op": "status", "request_id": id}), None);
        let wanted = Index(self.lock().highest);
        let response = node.status_after(id, wanted);
        let body = response
            .json()
            .unwrap_or_else(|error| panic!("rule: a body is JSON; {error}; {}", say(&response)));
        assert_eq!(
            response.status,
            200,
            "rule: a status lookup answers 200; {}",
            say(&response)
        );
        let (state, result) = match body["state"].as_str() {
            Some("known") => (
                "known",
                body["result"]
                    .get("error")
                    .map_or_else(|| outcome_ok(&body["result"]), outcome_error),
            ),
            Some("unknown") => ("unknown", Value::Null),
            other => panic!(
                "rule: a status is known or unknown; state={other:?} {}",
                say(&response)
            ),
        };
        let mut outcome = blank_outcome("ok");
        outcome["state"] = json!(state);
        outcome["result"] = result.clone();
        self.answer(attempt, "status", &outcome);
        result["index"].as_u64().map(|index| (attempt, index))
    }

    /// Drives every logical write to a terminal answer with bounded status lookups and records
    /// how each settled. A write no lookup finds is `unresolved`.
    fn settle(&self, client: u64, node: &Node) {
        let ids: Vec<String> = self.lock().ids.keys().cloned().collect();
        for id in ids {
            let mut found = None;
            for round in 0..LOOKUPS {
                found = self.lookup(client, node, &id);
                if found.is_some() {
                    break;
                }
                if round + 1 < LOOKUPS {
                    thread::sleep(LOOKUP_PAUSE);
                }
            }
            let record = match found {
                Some((attempt, index)) => json!({"kind": "resolution", "request_id": id,
                    "resolved": "executed", "index": index, "evidence": [attempt]}),
                None => json!({"kind": "resolution", "request_id": id,
                    "resolved": "unresolved", "index": null, "evidence": []}),
            };
            self.lock().push(record);
        }
    }

    fn writes(&self) -> usize {
        self.lock().ids.len()
    }

    /// The `keyval.observation/v1` document of what was recorded.
    fn document(&self) -> Value {
        let inner = self.lock();
        let overflowed: Vec<&str> = if inner.overflowed {
            vec!["records"]
        } else {
            Vec::new()
        };
        json!({"schema": "keyval.observation/v1", "complete": !inner.overflowed,
            "overflowed": overflowed,
            "event_budget": {"used": inner.records.len(), "exhausted": false},
            "scenario": {"status": "valid", "violated": null},
            "manifest": {"limits": {"max_key_bytes": 256, "max_value_bytes": 65536,
                "max_live_keys": 10_000, "max_live_bytes": 67_108_864_u64,
                "max_log_entries": 1_000_000, "max_history_bytes": 3_221_225_472_u64}},
            "voters": [1], "records": inner.records})
    }

    fn judge(&self) -> Report {
        judge(&self.document())
    }
}

fn judge(doc: &Value) -> Report {
    k10(doc).unwrap_or_else(|error| panic!("rule: the history is well formed; {error:?}"))
}

fn failures(report: &Report) -> String {
    report
        .violations
        .iter()
        .map(|violation| format!("{:?}: {}", violation.kind, violation.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

const CONCURRENT: Tag = Tag {
    test: "concurrent_clients_produce_a_linearizable_history",
    class: Class::Semantic,
    operations: &[
        "putKey",
        "deleteKey",
        "compareAndSwap",
        "getRequestStatus",
        "getKey",
    ],
    error_codes: &["version_mismatch"],
};

// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: getRequestStatus
// api-test: getKey
// api-test: version_mismatch
#[test]
pub(super) fn concurrent_clients_produce_a_linearizable_history() {
    each_target(&CONCURRENT, |node| {
        let prefix = fresh_prefix();
        let keys = [format!("{prefix}a"), format!("{prefix}b")];
        let recorder = Recorder::default();
        // (key, index, version after the write; None for a delete) of every success.
        let effects: Vec<(String, u64, Option<u64>)> = thread::scope(|scope| {
            let workers: Vec<_> = (1..=4_u64)
                .map(|client| {
                    let (recorder, keys) = (&recorder, &keys);
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        let mut seen_version = 1;
                        for step in 0..8_u64 {
                            let key =
                                keys[usize::try_from((client + step) % 2).unwrap_or(0)].clone();
                            let op = match (client + step) % 4 {
                                0 => Op::Put {
                                    key,
                                    value: format!("c{client}s{step}"),
                                },
                                1 => Op::Delete { key },
                                2 => Op::Cas {
                                    key,
                                    expected: json!({"kind": "absent"}),
                                    value: format!("c{client}s{step}"),
                                },
                                _ => Op::Cas {
                                    key,
                                    expected: json!({"kind": "version", "version": seen_version}),
                                    value: format!("c{client}s{step}"),
                                },
                            };
                            let id = fresh_id();
                            let first = recorder.write(client, node, &op, &id);
                            let outcome = if step % 3 == 0 {
                                // A client that did not see the answer retries the same ID.
                                recorder.write(client, node, &op, &id)
                            } else {
                                first
                            };
                            if outcome["code"] == "ok" {
                                let index = outcome["index"].as_u64().unwrap_or(0);
                                let version = outcome["version"].as_u64();
                                seen_version = version.unwrap_or(seen_version);
                                mine.push((op.key().to_owned(), index, version));
                            }
                        }
                        mine
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|_| panic!("rule: a client thread completes"))
                })
                .collect()
        });
        recorder.settle(0, node);
        let report = recorder.judge();
        assert!(
            report.passed(),
            "rule: K-10 accepts the concurrent history; {}",
            failures(&report)
        );
        assert_eq!(
            report.examined,
            recorder.writes() as u64,
            "rule: K-10 examined every logical write; examined={}",
            report.examined
        );
        assert!(!effects.is_empty(), "rule: some writes took effect");
        // The final state of each key is its highest-index success, read once applied.
        for key in &keys {
            let Some((_, index, version)) = effects
                .iter()
                .filter(|(k, _, _)| k == key)
                .max_by_key(|(_, index, _)| *index)
            else {
                continue;
            };
            let read = node.read_after(key, Index(*index));
            match version {
                Some(version) => {
                    let got = super::support::ok(&read, "GET of the final state");
                    assert_eq!(
                        got["version"], *version,
                        "rule: the last write wins; key={key:?} body={got}"
                    );
                }
                None => assert_eq!(
                    read.status,
                    404,
                    "rule: a final delete leaves the key absent; {}",
                    say(&read)
                ),
            }
        }
    });
}

const RETRIES: Tag = Tag {
    test: "retries_and_misuse_of_one_request_id_agree",
    class: Class::Semantic,
    operations: &["putKey", "getRequestStatus"],
    error_codes: &["request_id_conflict"],
};

// api-test: putKey
// api-test: getRequestStatus
// api-test: request_id_conflict
#[test]
fn retries_and_misuse_of_one_request_id_agree() {
    each_target(&RETRIES, |node| {
        let key = format!("{}k", fresh_prefix());
        let recorder = Recorder::default();
        let id = fresh_id();
        let put = Op::Put {
            key: key.clone(),
            value: "one".to_owned(),
        };
        let first = recorder.write(1, node, &put, &id);
        let second = recorder.write(2, node, &put, &id);
        assert_eq!(
            first["replayed"], false,
            "rule: the first execution is not a replay"
        );
        assert_eq!(second["replayed"], true, "rule: the retry is a replay");
        assert_eq!(first["index"], second["index"], "rule: both name one index");
        // Reusing the ID for another command is refused and is not an execution.
        let other = Op::Put {
            key,
            value: "two".to_owned(),
        };
        let misuse = recorder.write(3, node, &other, &id);
        assert_eq!(
            misuse["code"], "request_id_conflict",
            "rule: misuse conflicts; {misuse}"
        );
        recorder.settle(1, node);
        let report = recorder.judge();
        assert!(
            report.passed(),
            "rule: K-10 accepts retries and ignores misuse; {}",
            failures(&report)
        );
        assert_eq!(
            report.examined, 1,
            "rule: three attempts are one logical write"
        );
    });
}

const UNRESOLVED: Tag = Tag {
    test: "an_unresolved_write_fails_the_run_as_incomplete",
    class: Class::Semantic,
    operations: &["putKey", "getRequestStatus"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getRequestStatus
#[test]
fn an_unresolved_write_fails_the_run_as_incomplete() {
    each_target(&UNRESOLVED, |node| {
        let recorder = Recorder::default();
        let put = Op::Put {
            key: format!("{}k", fresh_prefix()),
            value: "v".to_owned(),
        };
        recorder.write(1, node, &put, &fresh_id());
        // A client invokes a write and never hears back; the node never saw it, so no lookup
        // can settle it either way.
        let lost = Op::Put {
            key: format!("{}lost", fresh_prefix()),
            value: "v".to_owned(),
        };
        recorder.invoke(2, &lost.json(), Some(&fresh_id()));
        recorder.settle(1, node);
        let report = recorder.judge();
        assert!(
            report.has(Kind::HistoryIncomplete) && !report.passed(),
            "rule: a write that cannot be resolved makes the run incomplete, a failure (P10); {}",
            failures(&report)
        );
    });
}

const TAMPER: Tag = Tag {
    test: "a_tampered_history_is_rejected",
    class: Class::Semantic,
    operations: &["putKey", "getRequestStatus"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getRequestStatus
#[test]
fn a_tampered_history_is_rejected() {
    with_model(&TAMPER, Limits::DEFAULT, |node| {
        let recorder = Recorder::default();
        for step in 0..3 {
            let put = Op::Put {
                key: format!("{}k{step}", fresh_prefix()),
                value: "v".to_owned(),
            };
            recorder.write(1, node, &put, &fresh_id());
        }
        recorder.settle(1, node);
        let doc = recorder.document();
        assert!(
            judge(&doc).passed(),
            "rule: the untampered history passes; {}",
            failures(&recorder.judge())
        );
        // The same answer with a version the log cannot have produced.
        let mut wrong_version = doc.clone();
        first_put_outcome(&mut wrong_version)["version"] = json!(999_999);
        assert!(
            !judge(&wrong_version).passed(),
            "rule: a version the model never produced fails K-10 (non-vacuity)"
        );
        // The same answer at an index its own resolution contradicts.
        let mut wrong_index = doc;
        let outcome = first_put_outcome(&mut wrong_index);
        outcome["index"] = json!(outcome["index"].as_u64().unwrap_or(0) + 1000);
        assert!(
            !judge(&wrong_index).passed(),
            "rule: an index the resolution contradicts fails K-10 (non-vacuity)"
        );
    });
}

/// The outcome of the first PUT response in `doc`.
fn first_put_outcome(doc: &mut Value) -> &mut Value {
    let record = doc["records"].as_array_mut().and_then(|records| {
        records
            .iter_mut()
            .find(|record| record["kind"] == "response" && record["op"] == "put")
    });
    let Some(record) = record else {
        panic!("rule: the history holds a PUT response");
    };
    &mut record["outcome"]
}

const TIMEOUT: Tag = Tag {
    test: "a_poll_that_never_reaches_its_index_times_out_as_a_failure",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getKey
#[test]
fn a_poll_that_never_reaches_its_index_times_out_as_a_failure() {
    with_model(&TIMEOUT, Limits::DEFAULT, |node| {
        let key = format!("{}k", fresh_prefix());
        let put = super::support::ok(&node.put(&key, "v", &fresh_id()), "PUT");
        let reached = node.try_read_after(
            &key,
            index_of(&put),
            Policy::new(Duration::from_secs(5), Duration::from_millis(10)),
        );
        assert!(
            reached.is_ok(),
            "rule: an index the node has applied is reached"
        );
        let beyond = Index(index_of(&put).0 + 1_000_000);
        let started = std::time::Instant::now();
        let outcome = node.try_read_after(
            &key,
            beyond,
            Policy::new(Duration::from_millis(300), Duration::from_millis(20)),
        );
        match outcome {
            Err(PollError::Timeout {
                wanted, attempts, ..
            }) => {
                assert_eq!(
                    wanted, beyond,
                    "rule: the timeout names the index waited for"
                );
                assert!(
                    attempts >= 2,
                    "rule: polling retried before giving up; attempts={attempts}"
                );
            }
            other => panic!(
                "rule: an unreachable index is a timeout failure, never a pass; got {other:?}"
            ),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "rule: polling is bounded by its policy; took {:?}",
            started.elapsed()
        );
    });
}
