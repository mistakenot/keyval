//! Reads an HTTP answer into the domain's [`Response`] (CD-3), so the CLI feeds the same
//! [`types::Retry`] policy the simulated clients use (§8.7).
//!
//! The server is untrusted input here: every field is parsed into its domain type, and a
//! body that does not fit (a missing field, a key out of order in a scan page) is a
//! [`DecodeError`], never a panic. The caller treats an undecodable answer as silence: it
//! says nothing about the attempt (A-5).

use std::fmt;

use serde_json::Value as Json;
use types::{
    Decision, EntryIndex, ExpectedVersion, HistoryLimit, Key, KeyValueLimits, LiveStateLimit,
    LogIndex, NodeId, NotReadyLifecycle, Refusal, RequestState, Response, ScanItem, ScanPage,
    ScanRange, ServedBy, TerminalResult, Value, Version,
};

use super::http::HttpResponse;

/// What kind of operation an answer is for: it decides how a `200` body reads.
#[derive(Clone, Copy, Debug)]
pub enum Expect<'a> {
    /// A PUT, DELETE or compare-and-swap.
    Write,
    /// A GET.
    Get,
    /// A SCAN of this range.
    Scan(&'a ScanRange),
    /// A status lookup.
    Status,
}

/// A request the server refused for what it is, not for the cluster's state: nothing was
/// submitted and a retry of the same bytes cannot help (`400`, `413`, `405`, `415`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    /// The HTTP status.
    pub status: u16,
    /// The error code, as the server spelled it.
    pub code: String,
    /// The server's message.
    pub message: String,
}

/// A decoded answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// An answer the retry policy understands.
    Answer(Response),
    /// A refusal of the request itself.
    Rejected(Rejection),
}

/// Why an answer could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unreadable answer: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

fn bad<T>(why: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(why.into()))
}

/// Reads `response` as the answer to an operation of kind `expect`.
///
/// `nodes` are the nodes the client may send to: a `307` naming any other node is
/// unreadable, since the client has no address for it. `from` is the node that answered.
///
/// # Errors
///
/// [`DecodeError`] if the body is not the spec's shape for this status.
pub fn decode(
    response: &HttpResponse,
    expect: Expect<'_>,
    nodes: &[NodeId],
    from: NodeId,
) -> Result<Decoded, DecodeError> {
    let body: Json = serde_json::from_slice(&response.body).map_err(|err| {
        DecodeError(format!(
            "status {}: body is not JSON: {err}",
            response.status
        ))
    })?;
    if response.status == 200 {
        return ok(&body, expect).map(Decoded::Answer);
    }
    let error = body
        .get("error")
        .ok_or_else(|| DecodeError(format!("status {}: no error object", response.status)))?;
    let code = text(error, "code")?;
    let message = error
        .get("message")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let details = error.get("details").unwrap_or(&Json::Null);
    let refused = |refusal| Ok(Decoded::Answer(Response::Refused(refusal)));
    match (response.status, code) {
        (404, "not_found") => Ok(Decoded::Answer(Response::NotFound {
            served_by: served_by(details)?,
        })),
        (409, "version_mismatch") => {
            let current = match details.get("current_version") {
                Some(Json::Null) => None,
                Some(_) => Some(version(details, "current_version")?),
                None => return bad("version_mismatch without current_version"),
            };
            written(details, Decision::VersionMismatch { current }, from)
        }
        (409, "request_id_conflict") => Ok(Decoded::Answer(Response::Conflict {
            original_index: entry_index(details, "original_index")?,
        })),
        (507, "capacity_exceeded") => {
            let limit = match text(details, "limit")? {
                "max_live_keys" => LiveStateLimit::Keys,
                "max_live_bytes" => LiveStateLimit::Bytes,
                other => return bad(format!("capacity_exceeded limit {other:?}")),
            };
            written(details, Decision::CapacityExceeded { limit }, from)
        }
        (507, "history_full") => {
            let limit = match text(details, "limit")? {
                "max_log_entries" => HistoryLimit::LogEntries,
                "max_history_bytes" => HistoryLimit::HistoryBytes,
                other => return bad(format!("history_full limit {other:?}")),
            };
            refused(Refusal::HistoryFull { limit })
        }
        (307, "not_leader") => {
            let leader = NodeId::new(number(details, "leader_hint")?)
                .map_err(|err| DecodeError(format!("leader_hint: {err}")))?;
            if nodes.contains(&leader) {
                refused(Refusal::NotLeader { leader })
            } else {
                bad(format!(
                    "leader_hint {leader} is not a node of this cluster"
                ))
            }
        }
        (429, "busy") => refused(Refusal::Busy),
        (503, "unavailable") => refused(Refusal::Unavailable),
        (503, "recovering") => {
            let lifecycle = match text(details, "lifecycle")? {
                "starting" => NotReadyLifecycle::Starting,
                "recovering" => NotReadyLifecycle::Recovering,
                "failed" => NotReadyLifecycle::Failed,
                other => return bad(format!("recovering lifecycle {other:?}")),
            };
            refused(Refusal::Recovering { lifecycle })
        }
        (504, "unknown_outcome") => refused(Refusal::UnknownOutcome),
        (400 | 405 | 413 | 415, _) => Ok(Decoded::Rejected(Rejection {
            status: response.status,
            code: code.to_owned(),
            message,
        })),
        (status, code) => bad(format!("status {status} with code {code:?} is not in §8.4")),
    }
}

/// A `200` body.
fn ok(body: &Json, expect: Expect<'_>) -> Result<Response, DecodeError> {
    match expect {
        Expect::Write => {
            let decision = if body.get("existed").is_some() {
                Decision::Deleted {
                    existed: boolean(body, "existed")?,
                }
            } else {
                Decision::Set
            };
            let index = log_index(body, "index")?;
            if decision == Decision::Set && version(body, "version")?.index() != index {
                return bad("a PUT's version differs from its index (DM-3)");
            }
            Ok(Response::Written {
                result: terminal(index, decision)?,
                replayed: boolean(body, "replayed")?,
                served_by: served_by(body)?,
            })
        }
        Expect::Get => Ok(Response::Found {
            value: value(body, "value")?,
            version: version(body, "version")?,
            served_by: served_by(body)?,
        }),
        Expect::Scan(range) => scan_page(body, range).map(Response::Page),
        Expect::Status => status(body),
    }
}

fn status(body: &Json) -> Result<Response, DecodeError> {
    let served_by = served_by(body)?;
    let state = match text(body, "state")? {
        "unknown" => RequestState::Unknown,
        "known" => {
            let result = body
                .get("result")
                .ok_or_else(|| DecodeError("known without result".to_owned()))?;
            RequestState::Known(embedded_result(result)?)
        }
        other => return bad(format!("status state {other:?}")),
    };
    Ok(Response::Status { state, served_by })
}

/// The terminal result a status lookup embeds: the body a retry of the write would get.
fn embedded_result(result: &Json) -> Result<TerminalResult, DecodeError> {
    if let Some(error) = result.get("error") {
        let details = error.get("details").unwrap_or(&Json::Null);
        let index = log_index(details, "index")?;
        return match text(error, "code")? {
            "version_mismatch" => {
                let current = match details.get("current_version") {
                    Some(Json::Null) => None,
                    _ => Some(version(details, "current_version")?),
                };
                terminal(index, Decision::VersionMismatch { current })
            }
            "capacity_exceeded" => {
                let limit = match text(details, "limit")? {
                    "max_live_keys" => LiveStateLimit::Keys,
                    "max_live_bytes" => LiveStateLimit::Bytes,
                    other => return bad(format!("capacity_exceeded limit {other:?}")),
                };
                terminal(index, Decision::CapacityExceeded { limit })
            }
            other => bad(format!("an embedded result with code {other:?}")),
        };
    }
    match ok(result, Expect::Write)? {
        Response::Written { result, .. } => Ok(result),
        other => bad(format!("an embedded result decoded as {other:?}")),
    }
}

/// A `409 version_mismatch` or `507 capacity_exceeded`: a committed result. The error body
/// carries no `served_by`; the answering node `from` served it, and the executing index is a
/// lower bound on what it has applied, which is all the policy (and the display) needs.
fn written(details: &Json, decision: Decision, from: NodeId) -> Result<Decoded, DecodeError> {
    let index = log_index(details, "index")?;
    Ok(Decoded::Answer(Response::Written {
        result: terminal(index, decision)?,
        replayed: boolean(details, "replayed")?,
        served_by: ServedBy {
            node: from,
            applied: index,
        },
    }))
}

fn scan_page(body: &Json, range: &ScanRange) -> Result<ScanPage, DecodeError> {
    let raw = body
        .get("items")
        .and_then(Json::as_array)
        .ok_or_else(|| DecodeError("scan page without items".to_owned()))?;
    let mut items: Vec<ScanItem> = Vec::with_capacity(raw.len());
    for item in raw {
        let key = key(item, "key")?;
        if !range.contains(&key) {
            return bad(format!("scan key {:?} is outside the range", key.as_str()));
        }
        if items.last().is_some_and(|last| last.key >= key) {
            return bad(format!("scan key {:?} is out of order", key.as_str()));
        }
        items.push(ScanItem {
            key,
            value: value(item, "value")?,
            version: version(item, "version")?,
        });
    }
    if u64::try_from(items.len()).map_or(true, |count| count > range.limit().get()) {
        return bad(format!("{} scan items exceed the limit", items.len()));
    }
    let bytes: usize = items
        .iter()
        .map(|item| item.key.byte_len().saturating_add(item.value.byte_len()))
        .fold(0, usize::saturating_add);
    if u64::try_from(bytes).map_or(true, |bytes| bytes > types::MAX_SCAN_PAGE_BYTES) {
        return bad(format!("{bytes} scan page bytes exceed the bound"));
    }
    let has_more = boolean(body, "has_more")?;
    let continuation = match body.get("next_after") {
        Some(Json::Null) | None => None,
        Some(_) => Some(key(body, "next_after")?),
    };
    if has_more != continuation.is_some() {
        return bad("has_more disagrees with next_after");
    }
    if let Some(next) = &continuation
        && items.last().map(|last| &last.key) != Some(next)
    {
        return bad("next_after is not the last returned key");
    }
    Ok(ScanPage::new(range, items, continuation, served_by(body)?))
}

fn terminal(index: LogIndex, decision: Decision) -> Result<TerminalResult, DecodeError> {
    TerminalResult::new(index, decision)
        .ok_or_else(|| DecodeError("a result at log index 0".to_owned()))
}

fn served_by(from: &Json) -> Result<ServedBy, DecodeError> {
    let served = from
        .get("served_by")
        .ok_or_else(|| DecodeError("no served_by".to_owned()))?;
    Ok(ServedBy {
        node: NodeId::new(number(served, "node_id")?)
            .map_err(|err| DecodeError(format!("served_by.node_id: {err}")))?,
        applied: LogIndex::new(number(served, "applied_index")?)
            .map_err(|err| DecodeError(format!("served_by.applied_index: {err}")))?,
    })
}

fn field<'a>(from: &'a Json, name: &str) -> Result<&'a Json, DecodeError> {
    from.get(name)
        .ok_or_else(|| DecodeError(format!("no field {name:?}")))
}

fn text<'a>(from: &'a Json, name: &str) -> Result<&'a str, DecodeError> {
    field(from, name)?
        .as_str()
        .ok_or_else(|| DecodeError(format!("field {name:?} is not a string")))
}

fn number(from: &Json, name: &str) -> Result<u64, DecodeError> {
    field(from, name)?
        .as_u64()
        .ok_or_else(|| DecodeError(format!("field {name:?} is not a non-negative integer")))
}

fn boolean(from: &Json, name: &str) -> Result<bool, DecodeError> {
    field(from, name)?
        .as_bool()
        .ok_or_else(|| DecodeError(format!("field {name:?} is not a boolean")))
}

fn version(from: &Json, name: &str) -> Result<Version, DecodeError> {
    Version::new(number(from, name)?).map_err(|err| DecodeError(format!("{name}: {err}")))
}

fn log_index(from: &Json, name: &str) -> Result<LogIndex, DecodeError> {
    LogIndex::new(number(from, name)?).map_err(|err| DecodeError(format!("{name}: {err}")))
}

fn entry_index(from: &Json, name: &str) -> Result<EntryIndex, DecodeError> {
    EntryIndex::new(number(from, name)?).map_err(|err| DecodeError(format!("{name}: {err}")))
}

fn key(from: &Json, name: &str) -> Result<Key, DecodeError> {
    Key::parse(text(from, name)?.to_owned(), KeyValueLimits::MAXIMA)
        .map_err(|err| DecodeError(format!("{name}: {err}")))
}

fn value(from: &Json, name: &str) -> Result<Value, DecodeError> {
    Value::parse(text(from, name)?.to_owned(), KeyValueLimits::MAXIMA)
        .map_err(|err| DecodeError(format!("{name}: {err}")))
}

/// The JSON body of a compare-and-swap's expectation.
#[must_use]
pub fn expectation_json(expected: ExpectedVersion) -> Json {
    match expected {
        ExpectedVersion::Absent => serde_json::json!({"kind": "absent"}),
        ExpectedVersion::Version(version) => {
            serde_json::json!({"kind": "version", "version": version.get()})
        }
    }
}
