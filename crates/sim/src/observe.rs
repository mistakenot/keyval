//! Observation recording (PLAN.md §10.2 "Outputs", §10.3; W36.2): one run's client-side
//! observations, as a `keyval.observation/v1` document (`schemas/observation/`).
//!
//! The document is built from what the simulated paths and the simulated clients saw, never
//! from what a node claims about itself (T-12):
//! - `invoke`: every attempt, from [`Cluster::observed`]: the client, the node incarnation it
//!   reached, the request ID of a write, the operation, and whether it is a misuse attempt
//!   (§10.3, from [`Clients::sends`]);
//! - `response`: the response the incarnation generated, in the API's terms, with its
//!   generation order;
//! - `delivery`: each copy of a response arriving at its client, or lost;
//! - `resolution`: once per logical write, after every attempt's records: executed at an
//!   index, refused, or unresolved, with the attempts whose answers decide it. Misuse
//!   attempts are never logical writes and have no resolution.
//!
//! The node side comes from [`Cluster::recorded`] (W36.3), the cluster's own mirrors of its
//! nodes:
//! - `recovered`: an incarnation's recovered durable state and whole log;
//! - `append` and `truncate`: how a running node's log changed, step by step;
//! - `node_state`: a node's process, lifecycle, role, term, vote, leader, commit, applied
//!   index and state digest whenever one changed. On a step's tie of `seq`, node-side
//!   records come first, since a change is recorded at the point it happened.
//!
//! `phase` records come from the marks the caller supplies. The rest of the node side
//! (`sync`, `accept`, `committed`, `release`, `attempt`, `fault` and the others) belongs to
//! later recorders (W36.4, W36.5, W40, W43), so until then the judge reports the run
//! incomplete (P10). A node-side queue that overflowed is listed in `overflowed`.
//!
//! Recording draws no randomness and changes nothing: the same run gives a byte-identical
//! document (P4). A shape the schema cannot express, or a stream that is out of order, is an
//! [`ObserveError`], never a silently repaired document (P10).

use core::fmt;
use std::collections::BTreeMap;

use serde_json::{Value, json};
use types::{
    AttemptId, ClientOp, Config, Decision, ExpectedVersion, MAX_SAFE_INTEGER, Mutation, RequestId,
    RequestState, Response, ServedBy, TerminalResult,
};
use types::{Entry, EntryPayload, Lifecycle, NodeId, Role};

use types::{Refusal, ScanItem};

use crate::clients::{Clients, End, HealMode, Outcome, SentAttempt};
use crate::cluster::{Cluster, Observed, Recorded};
use crate::recorder::{NodeSnapshot, Process};
use crate::scheduler::{NodeIncarnation, VirtualTime};

/// The identifier of the document's schema.
pub const SCHEMA_ID: &str = "keyval.observation/v1";

/// A point in the run: the scheduler's dispatched-event count and the virtual time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// The dispatched-event count.
    pub seq: u64,
    /// The virtual time.
    pub at: VirtualTime,
}

impl Mark {
    /// The cluster's present mark.
    #[must_use]
    pub fn of(cluster: &Cluster) -> Self {
        Self {
            seq: cluster.events().get(),
            at: cluster.now(),
        }
    }
}

/// When each run phase began (§10.2), in order.
///
/// A phase begins before the first record whose `seq` exceeds its mark's: a mark taken after
/// N events follows every record of those N events, whose `seq` is at most N.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phases {
    /// Setup.
    pub setup: Mark,
    /// Baseline.
    pub baseline: Mark,
    /// Chaos.
    pub chaos: Mark,
    /// Heal.
    pub heal: Mark,
    /// Check.
    pub check: Mark,
}

/// What the manifest records about the run beyond its configuration (§10.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The master seed, below 2^53 so the document can carry it (§8.1).
    pub seed: u64,
    /// The scenario version.
    pub scenario_version: u64,
    /// The profile's name.
    pub profile: String,
    /// The build ID hashing toolchain, target, lockfile and the rest (T-3).
    pub build_id: String,
    /// The PRNG implementation's name.
    pub prng: String,
    /// The run's event budget.
    pub max_events: u64,
    /// The chaos phase's length in ticks.
    pub chaos_ticks: u64,
    /// The heal's budget in ticks.
    pub heal_budget_ticks: u64,
    /// How the heal completes.
    pub heal: HealMode,
    /// The drain's quiet period in ticks.
    pub quiet_ticks: u64,
    /// When each phase began.
    pub phases: Phases,
}

/// Why a document could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObserveError {
    /// A response whose shape the schema has no outcome for (a refusal a read cannot get, or
    /// a response of another operation's kind).
    Unrepresentable {
        /// The attempt.
        attempt: u64,
        /// What was wrong.
        what: &'static str,
    },
    /// A response or delivery names an attempt that was never invoked.
    UnknownAttempt {
        /// The attempt.
        attempt: u64,
    },
    /// An integer the document cannot carry (it must be below 2^53, §8.1).
    TooLarge {
        /// What it was.
        what: &'static str,
        /// Its value.
        value: u64,
    },
    /// A record's `seq` or `time` is before its predecessor's.
    Order {
        /// The record's position.
        position: usize,
    },
}

impl fmt::Display for ObserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrepresentable { attempt, what } => {
                write!(f, "attempt {attempt}: {what} has no observation outcome")
            }
            Self::UnknownAttempt { attempt } => {
                write!(f, "attempt {attempt} is named before it was invoked")
            }
            Self::TooLarge { what, value } => {
                write!(f, "{what} {value} does not fit below 2^53")
            }
            Self::Order { position } => {
                write!(f, "record {position} is before its predecessor")
            }
        }
    }
}

impl std::error::Error for ObserveError {}

/// Checks that `value` fits the document's integers.
const fn safe(what: &'static str, value: u64) -> Result<u64, ObserveError> {
    if value <= MAX_SAFE_INTEGER {
        Ok(value)
    } else {
        Err(ObserveError::TooLarge { what, value })
    }
}

/// A node ID, or incarnation, as the document carries it.
fn on(at: NodeIncarnation) -> Value {
    json!({ "node": at.node.get(), "incarnation": at.incarnation.get() })
}

/// The serving node and applied prefix of an answer.
fn served(served_by: ServedBy) -> Value {
    json!({ "node_id": served_by.node.get(), "applied_index": served_by.applied.get() })
}

/// The operation name of `op`.
const fn op_name(op: &ClientOp) -> &'static str {
    match op {
        ClientOp::Write { mutation, .. } => match mutation {
            Mutation::Put { .. } => "put",
            Mutation::Delete { .. } => "delete",
            Mutation::Cas { .. } => "cas",
        },
        ClientOp::Get { .. } => "get",
        ClientOp::Scan { .. } => "scan",
        ClientOp::Status { .. } => "status",
    }
}

/// `op` in the API's terms (§8.2).
fn operation(op: &ClientOp) -> Value {
    let name = op_name(op);
    match op {
        ClientOp::Write { mutation, .. } => match mutation {
            Mutation::Put { key, value } => {
                json!({ "op": name, "key": key.as_str(), "value": value.as_str() })
            }
            Mutation::Delete { key } => json!({ "op": name, "key": key.as_str() }),
            Mutation::Cas {
                key,
                expected,
                value,
            } => {
                let expected = match expected {
                    ExpectedVersion::Absent => json!({ "kind": "absent" }),
                    ExpectedVersion::Version(version) => {
                        json!({ "kind": "version", "version": version.get() })
                    }
                };
                json!({
                    "op": name,
                    "key": key.as_str(),
                    "expected": expected,
                    "value": value.as_str(),
                })
            }
        },
        ClientOp::Get { key } => json!({ "op": name, "key": key.as_str() }),
        ClientOp::Scan { range } => json!({
            "op": name,
            "start": range.start().map(types::Key::as_str),
            "end": range.end().map(types::Key::as_str),
            "after": range.after().map(types::Key::as_str),
            "limit": range.limit().get(),
        }),
        ClientOp::Status { request_id } => {
            json!({ "op": name, "request_id": request_id.to_string() })
        }
    }
}

/// The request ID `op` carries, if it is a write.
const fn write_id(op: &ClientOp) -> Option<RequestId> {
    match op {
        ClientOp::Write { request_id, .. } => Some(*request_id),
        ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
    }
}

/// A write outcome with every field present and the ones a code does not carry null.
#[derive(Default)]
struct Write {
    code: &'static str,
    request_id: Option<String>,
    index: Option<u64>,
    version: Option<u64>,
    existed: Option<bool>,
    replayed: Option<bool>,
    served_by: Option<Value>,
    attempt_state: Option<&'static str>,
    current_version: Option<u64>,
    original_index: Option<u64>,
    limit: Option<&'static str>,
    leader_hint: Option<u64>,
    lifecycle: Option<&'static str>,
}

impl Write {
    fn json(self) -> Value {
        json!({
            "code": self.code,
            "request_id": self.request_id,
            "index": self.index,
            "version": self.version,
            "existed": self.existed,
            "replayed": self.replayed,
            "served_by": self.served_by,
            "attempt_state": self.attempt_state,
            "current_version": self.current_version,
            "original_index": self.original_index,
            "limit": self.limit,
            "leader_hint": self.leader_hint,
            "lifecycle": self.lifecycle,
        })
    }
}

/// A stored terminal result as the document carries it (§8.3): the body `request_id`'s retry
/// would get, `replayed` as given. `served_by` is carried by a success only.
fn terminal(
    request_id: &RequestId,
    result: TerminalResult,
    replayed: bool,
    served_by: ServedBy,
) -> Write {
    let base = Write {
        request_id: Some(request_id.to_string()),
        index: Some(result.index().get()),
        replayed: Some(replayed),
        ..Write::default()
    };
    match result.decision() {
        Decision::Set => Write {
            code: "ok",
            version: result.version().map(types::Version::get),
            served_by: Some(served(served_by)),
            ..base
        },
        Decision::Deleted { existed } => Write {
            code: "ok",
            existed: Some(existed),
            served_by: Some(served(served_by)),
            ..base
        },
        Decision::VersionMismatch { current } => Write {
            code: "version_mismatch",
            current_version: current.map(types::Version::get),
            ..base
        },
        Decision::CapacityExceeded { limit } => Write {
            code: "capacity_exceeded",
            limit: Some(limit.as_str()),
            ..base
        },
    }
}

/// A refusal as a write outcome.
fn refused_write(request_id: &RequestId, refusal: Refusal) -> Write {
    let base = Write {
        request_id: Some(request_id.to_string()),
        attempt_state: Some(if refusal.submitted() {
            "submitted"
        } else {
            "not_submitted"
        }),
        ..Write::default()
    };
    match refusal {
        Refusal::NotLeader { leader } => Write {
            code: "not_leader",
            leader_hint: Some(leader.get()),
            ..base
        },
        Refusal::Busy => Write {
            code: "busy",
            ..base
        },
        Refusal::Unavailable => Write {
            code: "unavailable",
            ..base
        },
        Refusal::Recovering { lifecycle } => Write {
            code: "recovering",
            lifecycle: Some(lifecycle.as_str()),
            ..base
        },
        Refusal::UnknownOutcome => Write {
            code: "unknown_outcome",
            ..base
        },
        Refusal::HistoryFull { limit } => Write {
            code: "history_full",
            limit: Some(limit.as_str()),
            ..base
        },
    }
}

/// A refusal a read or status lookup can get, as the code, the lifecycle and nothing else;
/// `None` for one only a write can get.
const fn read_refusal(refusal: Refusal) -> Option<(&'static str, Option<&'static str>)> {
    match refusal {
        Refusal::Busy => Some(("busy", None)),
        Refusal::Unavailable => Some(("unavailable", None)),
        Refusal::Recovering { lifecycle } => Some(("recovering", Some(lifecycle.as_str()))),
        Refusal::NotLeader { .. } | Refusal::UnknownOutcome | Refusal::HistoryFull { .. } => None,
    }
}

/// The outcome of `response` to `op`, as the client sees it (§8.2, §8.4).
fn outcome(attempt: AttemptId, op: &ClientOp, response: &Response) -> Result<Value, ObserveError> {
    let wrong = |what| ObserveError::Unrepresentable {
        attempt: attempt.get(),
        what,
    };
    match (op, response) {
        (
            ClientOp::Write { request_id, .. },
            Response::Written {
                result,
                replayed,
                served_by,
            },
        ) => Ok(terminal(request_id, *result, *replayed, *served_by).json()),
        (ClientOp::Write { request_id, .. }, Response::Conflict { original_index }) => Ok(Write {
            code: "request_id_conflict",
            request_id: Some(request_id.to_string()),
            original_index: Some(original_index.get()),
            ..Write::default()
        }
        .json()),
        (ClientOp::Write { request_id, .. }, Response::Refused(refusal)) => {
            Ok(refused_write(request_id, *refusal).json())
        }
        (
            ClientOp::Write { .. },
            Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. },
        ) => Err(wrong("a write's response of another kind")),
        (
            ClientOp::Get { .. },
            Response::Found {
                value,
                version,
                served_by,
            },
        ) => Ok(json!({
            "code": "ok",
            "value": value.as_str(),
            "version": version.get(),
            "served_by": served(*served_by),
            "lifecycle": null,
        })),
        (ClientOp::Get { .. }, Response::NotFound { served_by }) => Ok(json!({
            "code": "not_found",
            "value": null,
            "version": null,
            "served_by": served(*served_by),
            "lifecycle": null,
        })),
        (ClientOp::Get { .. }, Response::Refused(refusal)) => {
            let (code, lifecycle) =
                read_refusal(*refusal).ok_or_else(|| wrong("a refusal a GET cannot get"))?;
            Ok(json!({
                "code": code,
                "value": null,
                "version": null,
                "served_by": null,
                "lifecycle": lifecycle,
            }))
        }
        (
            ClientOp::Get { .. },
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::Page(_)
            | Response::Status { .. },
        ) => Err(wrong("a GET's response of another kind")),
        (ClientOp::Scan { .. }, response) => scan_outcome(attempt, response),
        (ClientOp::Status { request_id }, response) => {
            status_outcome(attempt, request_id, response)
        }
    }
}

/// The outcome of `response` to a SCAN (§8.3).
fn scan_outcome(attempt: AttemptId, response: &Response) -> Result<Value, ObserveError> {
    let wrong = |what| ObserveError::Unrepresentable {
        attempt: attempt.get(),
        what,
    };
    match response {
        Response::Page(page) => {
            let items: Vec<Value> = page.items().iter().map(item).collect();
            Ok(json!({
                "code": "ok",
                "items": items,
                "has_more": page.continuation().is_some(),
                "next_after": page.continuation().map(types::Key::as_str),
                "served_by": served(page.served_by()),
                "lifecycle": null,
            }))
        }
        Response::Refused(refusal) => {
            let (code, lifecycle) =
                read_refusal(*refusal).ok_or_else(|| wrong("a refusal a SCAN cannot get"))?;
            Ok(json!({
                "code": code,
                "items": null,
                "has_more": null,
                "next_after": null,
                "served_by": null,
                "lifecycle": lifecycle,
            }))
        }
        Response::Written { .. }
        | Response::Conflict { .. }
        | Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Status { .. } => Err(wrong("a SCAN's response of another kind")),
    }
}

/// The outcome of `response` to a status lookup of `request_id` (§8.5).
fn status_outcome(
    attempt: AttemptId,
    request_id: &RequestId,
    response: &Response,
) -> Result<Value, ObserveError> {
    let wrong = |what| ObserveError::Unrepresentable {
        attempt: attempt.get(),
        what,
    };
    match response {
        Response::Status { state, served_by } => Ok(match state {
            RequestState::Known(result) => json!({
                "code": "ok",
                "state": "known",
                "result": terminal(request_id, *result, true, *served_by).json(),
                "served_by": served(*served_by),
                "lifecycle": null,
            }),
            RequestState::Unknown => json!({
                "code": "ok",
                "state": "unknown",
                "result": null,
                "served_by": served(*served_by),
                "lifecycle": null,
            }),
        }),
        Response::Refused(refusal) => {
            let (code, lifecycle) = read_refusal(*refusal)
                .ok_or_else(|| wrong("a refusal a status lookup cannot get"))?;
            Ok(json!({
                "code": code,
                "state": null,
                "result": null,
                "served_by": null,
                "lifecycle": lifecycle,
            }))
        }
        Response::Written { .. }
        | Response::Conflict { .. }
        | Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_) => Err(wrong("a status lookup's response of another kind")),
    }
}

/// One SCAN item.
fn item(item: &ScanItem) -> Value {
    json!({
        "key": item.key.as_str(),
        "value": item.value.as_str(),
        "version": item.version.get(),
    })
}

/// How the run resolved the logical write of `sent`, given its end: the resolution record's
/// `resolved`, `index` and `evidence`.
fn resolution(outcome: Option<&Outcome>) -> (&'static str, Option<u64>, Vec<u64>) {
    let Some(outcome) = outcome else {
        return ("unresolved", None, Vec::new());
    };
    let evidence: Vec<u64> = outcome
        .decided_by
        .iter()
        .map(|attempt| attempt.get())
        .collect();
    match &outcome.end {
        End::Done(Response::Written { result, .. }) => {
            ("executed", Some(result.index().get()), evidence)
        }
        End::Done(Response::Status {
            state: RequestState::Known(result),
            ..
        }) => ("executed", Some(result.index().get()), evidence),
        End::Refused => ("refused", None, evidence),
        // After a certified drain every recoverable node answered `unknown`: the lookups are
        // the evidence (§10.2).
        End::NotExecuted(lookups) => (
            "not_executed",
            None,
            lookups.iter().map(|attempt| attempt.get()).collect(),
        ),
        End::Done(
            Response::Status {
                state: RequestState::Unknown,
                ..
            }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Refused(_),
        )
        | End::Unresolved(_) => ("unresolved", None, Vec::new()),
    }
}

/// The five phase records.
fn phase_records(phases: &Phases) -> Result<Vec<(u64, Value)>, ObserveError> {
    [
        ("setup", phases.setup),
        ("baseline", phases.baseline),
        ("chaos", phases.chaos),
        ("heal", phases.heal),
        ("check", phases.check),
    ]
    .into_iter()
    .map(|(name, mark)| {
        let time = safe("time", mark.at.as_micros())?;
        Ok((
            mark.seq,
            json!({ "kind": "phase", "seq": safe("seq", mark.seq)?, "time": time, "phase": name }),
        ))
    })
    .collect()
}

/// The record for one cluster observation. `sent` maps attempts to what the clients sent.
fn record(
    entry: &Observed,
    invoked: &BTreeMap<u64, &ClientOp>,
    sent: &BTreeMap<u64, &SentAttempt>,
) -> Result<(u64, u64, Value), ObserveError> {
    match entry {
        Observed::Invoked {
            seq,
            at,
            client,
            attempt,
            target,
            op,
        } => {
            let misuse = sent.get(&attempt.get()).is_some_and(|sent| sent.misuse);
            let time = safe("time", at.as_micros())?;
            Ok((
                *seq,
                time,
                json!({
                    "kind": "invoke",
                    "seq": safe("seq", *seq)?,
                    "time": time,
                    "client": safe("client", client.get().saturating_add(1))?,
                    "attempt": safe("attempt", attempt.get())?,
                    "target": on(*target),
                    "request_id": write_id(op).map(|id| id.to_string()),
                    "misuse": misuse,
                    "operation": operation(op),
                }),
            ))
        }
        Observed::Generated {
            seq,
            at,
            attempt,
            from,
            generation,
            response,
        } => {
            let op = invoked
                .get(&attempt.get())
                .ok_or_else(|| ObserveError::UnknownAttempt {
                    attempt: attempt.get(),
                })?;
            let time = safe("time", at.as_micros())?;
            Ok((
                *seq,
                time,
                json!({
                    "kind": "response",
                    "seq": safe("seq", *seq)?,
                    "time": time,
                    "attempt": attempt.get(),
                    "from": on(*from),
                    "generation": safe("generation", *generation)?,
                    "op": op_name(op),
                    "outcome": outcome(*attempt, op, response)?,
                }),
            ))
        }
        Observed::Delivered {
            seq,
            at,
            attempt,
            delivered,
        } => {
            if !invoked.contains_key(&attempt.get()) {
                return Err(ObserveError::UnknownAttempt {
                    attempt: attempt.get(),
                });
            }
            let time = safe("time", at.as_micros())?;
            Ok((
                *seq,
                time,
                json!({
                    "kind": "delivery",
                    "seq": safe("seq", *seq)?,
                    "time": time,
                    "attempt": attempt.get(),
                    "delivered": delivered,
                }),
            ))
        }
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

/// A log entry as the document carries it.
fn entry_json(entry: &Entry) -> Value {
    let payload = match &entry.payload {
        EntryPayload::Empty => String::new(),
        EntryPayload::Command(bytes) => hex(bytes),
    };
    json!({ "index": entry.index.get(), "term": entry.term.get(), "payload": payload })
}

/// A node ID or `null`.
fn node_or_null(node: Option<NodeId>) -> Value {
    node.map_or(Value::Null, |node| json!(node.get()))
}

/// The body of a `node_state` record.
fn node_state_json(state: &NodeSnapshot) -> Result<Value, ObserveError> {
    Ok(json!({
        "kind": "node_state",
        "process": match state.process {
            Process::Running => "running",
            Process::Paused => "paused",
            Process::Crashed => "crashed",
            Process::Lost => "lost",
        },
        "lifecycle": match state.lifecycle {
            Lifecycle::Starting => "starting",
            Lifecycle::Recovering => "recovering",
            Lifecycle::Ready => "ready",
            Lifecycle::Failed => "failed",
        },
        "role": match state.role {
            Role::Leader => "leader",
            Role::Candidate => "candidate",
            Role::PreCandidate => "pre_candidate",
            Role::Follower => "follower",
        },
        "term": safe("term", state.term.get())?,
        "vote": node_or_null(state.vote),
        "leader": node_or_null(state.leader),
        "commit": safe("commit", state.commit.get())?,
        "applied": safe("applied", state.applied.get())?,
        "state_digest": state.digest.to_string(),
    }))
}

/// The record for one node-side observation.
fn recorded_record(entry: &Recorded) -> Result<(u64, u64, Value), ObserveError> {
    let (seq, at, node, body) = match entry {
        Recorded::Recovered {
            seq,
            at,
            node,
            term,
            vote,
            log,
        } => {
            let last = log.last();
            (
                seq,
                at,
                node,
                json!({
                    "kind": "recovered",
                    "durable": {
                        "term": safe("term", term.get())?,
                        "vote": node_or_null(*vote),
                        "last_index": last.map_or(0, |entry| entry.index.get()),
                        "last_term": last.map_or(0, |entry| entry.term.get()),
                    },
                    "log": log.iter().map(entry_json).collect::<Vec<_>>(),
                }),
            )
        }
        Recorded::Appended {
            seq,
            at,
            node,
            entry,
        } => {
            let Value::Object(mut fields) = entry_json(entry) else {
                return Err(ObserveError::Unrepresentable {
                    attempt: 0,
                    what: "a log entry",
                });
            };
            fields.insert("kind".into(), json!("append"));
            (seq, at, node, Value::Object(fields))
        }
        Recorded::Truncated {
            seq,
            at,
            node,
            from,
        } => (
            seq,
            at,
            node,
            json!({ "kind": "truncate", "from_index": from.get() }),
        ),
        Recorded::NodeState {
            seq,
            at,
            node,
            state,
        } => (seq, at, node, node_state_json(state)?),
    };
    let time = safe("time", at.as_micros())?;
    let Value::Object(mut fields) = body else {
        return Err(ObserveError::Unrepresentable {
            attempt: 0,
            what: "a node-side record",
        });
    };
    fields.insert("seq".into(), json!(safe("seq", *seq)?));
    fields.insert("time".into(), json!(time));
    fields.insert("at".into(), on(*node));
    Ok((*seq, time, Value::Object(fields)))
}

/// The resolution records: one per logical write, in the order its first attempt was sent,
/// each at `end`. Misuse attempts are not logical writes.
fn resolutions(clients: &Clients, end: Mark) -> Result<Vec<Value>, ObserveError> {
    let mut seen = Vec::new();
    let mut records = Vec::new();
    let time = safe("time", end.at.as_micros())?;
    for sent in clients.sends().iter().filter(|sent| !sent.misuse) {
        let ClientOp::Write { request_id, .. } = &sent.request else {
            continue;
        };
        if seen.contains(request_id) {
            continue;
        }
        seen.push(*request_id);
        let ended = clients
            .outcomes()
            .iter()
            .find(|ended| ended.client == sent.client && ended.op == sent.op);
        let (resolved, index, evidence) = resolution(ended);
        records.push(json!({
            "kind": "resolution",
            "seq": safe("seq", end.seq)?,
            "time": time,
            "request_id": request_id.to_string(),
            "resolved": resolved,
            "index": index,
            "evidence": evidence,
        }));
    }
    Ok(records)
}

/// Appends `record`, first the phases that began before it.
fn push_with_phases(
    ordered: &mut Vec<(u64, u64, Value)>,
    phases: &mut core::iter::Peekable<std::vec::IntoIter<(u64, Value)>>,
    record: (u64, u64, Value),
) {
    while let Some((mark, phase)) = phases.next_if(|(mark, _)| *mark < record.0) {
        let at = phase["time"].as_u64().unwrap_or(0);
        ordered.push((mark, at, phase));
    }
    ordered.push(record);
}

/// The run's `keyval.observation/v1` document, from what `cluster` and `clients` saw.
///
/// It is complete exactly when the event budget was not exhausted and every client finished
/// its plan (P10). The records are in event order: the phases, then each attempt's invoke,
/// response and deliveries as they happened, then each logical write's resolution.
///
/// # Errors
///
/// [`ObserveError`] for a response the schema cannot express, an attempt named before its
/// invocation, an integer at or above 2^53, or records out of order.
pub fn document(
    cluster: &Cluster,
    clients: &Clients,
    config: &Config,
    meta: &Meta,
) -> Result<Value, ObserveError> {
    let invoked: BTreeMap<u64, &ClientOp> = cluster
        .observed()
        .filter_map(|entry| match entry {
            Observed::Invoked { attempt, op, .. } => Some((attempt.get(), op)),
            Observed::Generated { .. } | Observed::Delivered { .. } => None,
        })
        .collect();
    let sent: BTreeMap<u64, &SentAttempt> = clients
        .sends()
        .iter()
        .map(|sent| (sent.attempt.get(), sent))
        .collect();
    let mut phases = phase_records(&meta.phases)?.into_iter().peekable();
    let mut ordered: Vec<(u64, u64, Value)> = Vec::new();
    let mut nodes = cluster
        .recorded()
        .map(recorded_record)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .peekable();
    for entry in cluster.observed() {
        let (seq, time, value) = record(entry, &invoked, &sent)?;
        while let Some(node_side) = nodes.next_if(|node_side| node_side.0 <= seq) {
            push_with_phases(&mut ordered, &mut phases, node_side);
        }
        push_with_phases(&mut ordered, &mut phases, (seq, time, value));
    }
    for node_side in nodes {
        push_with_phases(&mut ordered, &mut phases, node_side);
    }
    for (mark, phase) in phases {
        let at = phase["time"].as_u64().unwrap_or(0);
        ordered.push((mark, at, phase));
    }
    let end = Mark::of(cluster);
    for value in resolutions(clients, end)? {
        ordered.push((end.seq, end.at.as_micros(), value));
    }
    for (position, pair) in ordered.windows(2).enumerate() {
        if pair[1].0 < pair[0].0 || pair[1].1 < pair[0].1 {
            return Err(ObserveError::Order {
                position: position + 1,
            });
        }
    }
    let used = safe("event count", end.seq)?;
    let exhausted = used >= meta.max_events;
    let overflowed: Vec<Value> = cluster
        .recorded_overflow()
        .map(|(seq, bound)| json!({ "seq": seq, "what": "node-side observations", "bound": bound }))
        .into_iter()
        .collect();
    Ok(json!({
        "schema": SCHEMA_ID,
        "manifest": manifest(config, meta)?,
        "voters": config.voters().map(types::NodeId::get).collect::<Vec<_>>(),
        "complete": !exhausted && clients.finished() && overflowed.is_empty(),
        "overflowed": overflowed,
        "event_budget": { "used": used, "exhausted": exhausted },
        "scenario": { "status": "valid", "violated": null },
        "records": ordered.into_iter().map(|(_, _, value)| value).collect::<Vec<_>>(),
    }))
}

/// The run manifest (§10.2).
fn manifest(config: &Config, meta: &Meta) -> Result<Value, ObserveError> {
    Ok(json!({
        "scenario_version": safe("scenario version", meta.scenario_version)?,
        "seed": safe("seed", meta.seed)?,
        "profile": meta.profile,
        "heal": meta.heal.name(),
        "build_id": meta.build_id,
        "config_fingerprint": config.semantic_fingerprint().to_string(),
        "prng": meta.prng,
        "max_events": safe("event budget", meta.max_events)?,
        "tick_us": config.tick_ms().saturating_mul(1_000),
        "chaos_ticks": safe("chaos ticks", meta.chaos_ticks)?,
        "heal_budget_ticks": safe("heal budget", meta.heal_budget_ticks)?,
        "quiet_ticks": safe("quiet ticks", meta.quiet_ticks)?,
        "limits": {
            "max_key_bytes": config.max_key_bytes().get(),
            "max_value_bytes": config.max_value_bytes().get(),
            "max_live_keys": config.max_live_keys().get(),
            "max_live_bytes": config.max_live_bytes().get(),
            "max_log_entries": config.max_log_entries().get(),
            "max_history_bytes": config.max_history_bytes().get(),
        },
        "faults": [],
    }))
}
