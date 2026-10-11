//! The client API adapter (PLAN.md §8, `api/openapi.yaml`; W18.3).
//!
//! It only translates (CD-1):
//! - [`route`] parses each HTTP request into the core's domain types at the boundary
//!   (CD-3): the strict query, the `Idempotency-Key`, the strict JSON body, and the key and
//!   value limits measured after decoding;
//! - [`render`] turns each core answer into §8.4's status, code and `details`, shaped as the
//!   spec's schemas;
//! - [`live`], [`ready`] and [`cluster`] render the node's published snapshot (O-1).
//!
//! Every endpoint of `api/openapi.yaml` is served here: PUT, GET and DELETE on `/v1/kv`,
//! compare-and-swap, SCAN and the status lookup (W32.1).

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use types::{
    ClientOp, Config, Decision, ExpectedVersion, Key, KeyError, Mutation, NodeId,
    NotReadyLifecycle, PageSize, Refusal, RequestId, RequestState, Response as CoreResponse,
    ScanPage, ScanRange, ServedBy, TerminalResult, Value, ValueError, Version,
};

use crate::http::{Body, Request, Response};
use crate::logs::{lifecycle_name, role_name};
use crate::query::{self, QueryError};
use crate::runtime::Snapshot;

/// What a request asks of the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routed {
    /// A client attempt for the core.
    Attempt(Attempt),
    /// `GET /v1/health/live`.
    Live,
    /// `GET /v1/health/ready`.
    Ready,
    /// `GET /v1/cluster`.
    Cluster,
}

/// A client attempt: the operation for the core, and what rendering its answer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The operation.
    pub op: ClientOp,
    /// What kind of attempt it is.
    pub kind: Kind,
}

/// The kinds of attempts this adapter serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `PUT /v1/kv?key=K` (F-1, F-6).
    Put {
        /// The request ID.
        request_id: RequestId,
        /// The key, for a redirect's `Location`.
        key: Key,
    },
    /// `GET /v1/kv?key=K` (F-2).
    Get {
        /// The key, echoed in the result.
        key: Key,
    },
    /// `DELETE /v1/kv?key=K` (F-3, F-6).
    Delete {
        /// The request ID.
        request_id: RequestId,
        /// The key, for a redirect's `Location`.
        key: Key,
    },
    /// `POST /v1/cas?key=K` (F-5, F-6).
    Cas {
        /// The request ID.
        request_id: RequestId,
        /// The key, for a redirect's `Location`.
        key: Key,
    },
    /// `GET /v1/scan` (F-4).
    Scan,
    /// `GET /v1/requests/{request_id}` (F-9).
    Status {
        /// The request looked up.
        request_id: RequestId,
    },
}

impl Kind {
    /// The request ID of a write; `None` for a read, which has none to report.
    const fn write_id(&self) -> Option<RequestId> {
        match self {
            Self::Put { request_id, .. }
            | Self::Delete { request_id, .. }
            | Self::Cas { request_id, .. } => Some(*request_id),
            Self::Get { .. } | Self::Scan | Self::Status { .. } => None,
        }
    }
}

/// Everything rendering needs to know about the cluster.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    /// The configuration.
    pub config: &'a Config,
}

/// The methods `/v1/kv` serves.
const KV_METHODS: &str = "DELETE, GET, PUT";

/// Parses `request` into what it asks of the node, or the error to answer at once.
///
/// # Errors
///
/// The error response: `400 invalid_request`, `413 too_large`, `404` for an unknown or
/// not-yet-served endpoint, `405` for a method the endpoint does not serve.
pub fn route(request: &Request, config: &Config) -> Result<Routed, Response> {
    match (request.method.as_str(), request.path.as_str()) {
        ("PUT", "/v1/kv") => put(request, config).map(Routed::Attempt),
        ("GET", "/v1/kv") => get(request, config).map(Routed::Attempt),
        ("DELETE", "/v1/kv") => delete(request, config).map(Routed::Attempt),
        ("POST", "/v1/cas") => cas(request, config).map(Routed::Attempt),
        ("GET", "/v1/scan") => scan(request, config).map(Routed::Attempt),
        (_, "/v1/kv") => Err(not_allowed(
            KV_METHODS,
            &format!("{} is not a method of /v1/kv", request.method),
        )),
        ("GET", "/v1/health/live") => no_query(request).map(|()| Routed::Live),
        ("GET", "/v1/health/ready") => no_query(request).map(|()| Routed::Ready),
        ("GET", "/v1/cluster") => no_query(request).map(|()| Routed::Cluster),
        (_, "/v1/health/live" | "/v1/health/ready" | "/v1/cluster") => Err(not_allowed(
            "GET",
            &format!("{} is not a method of {}", request.method, request.path),
        )),
        (_, "/v1/cas") => Err(not_allowed(
            "POST",
            &format!("{} is not a method of /v1/cas", request.method),
        )),
        (_, "/v1/scan") => Err(not_allowed(
            "GET",
            &format!("{} is not a method of /v1/scan", request.method),
        )),
        ("GET", path) if path.starts_with("/v1/requests/") => {
            status(request, path.trim_start_matches("/v1/requests/")).map(Routed::Attempt)
        }
        (_, path) if path.starts_with("/v1/requests/") => Err(not_allowed(
            "GET",
            &format!("{} is not a method of {path}", request.method),
        )),
        (_, path) => Err(error(
            404,
            "invalid_request",
            &format!("no endpoint {path}"),
            Map::new(),
        )),
    }
}

/// The strict body of a PUT (§8.1): exactly one member, `value`, a string.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    value: String,
}

/// A PUT, in the order its errors are found: the request ID first, so that every later
/// error can carry it (§8.1).
// api: putKey
fn put(request: &Request, config: &Config) -> Result<Attempt, Response> {
    let request_id = write_id(request)?;
    let id = Some(request_id);
    let key = query_key(request, config).map_err(|why| invalid_write(id, &why))?;
    let body: PutBody = json_body(request, id, "PutRequest")?;
    let value = write_value(body.value, config, id)?;
    Ok(Attempt {
        op: ClientOp::Write {
            request_id,
            mutation: Mutation::Put {
                key: key.clone(),
                value,
            },
        },
        kind: Kind::Put { request_id, key },
    })
}

/// The strict body of a compare-and-swap (§8.1): `expected` and `value`, nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CasBody {
    expected: Expectation,
    value: String,
}

/// `expected`: exactly `{"kind":"absent"}` or `{"kind":"version","version":N}`. A struct,
/// not a tagged enum, so serde refuses unknown and duplicate members (§8.1); the pairing of
/// kind and version is checked by [`expectation`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expectation {
    kind: String,
    version: Option<u64>,
}

fn expectation(expected: &Expectation) -> Result<ExpectedVersion, String> {
    match (expected.kind.as_str(), expected.version) {
        ("absent", None) => Ok(ExpectedVersion::Absent),
        ("absent", Some(_)) => Err("expected.kind \"absent\" takes no other member".to_owned()),
        ("version", Some(raw)) => Version::new(raw)
            .map(ExpectedVersion::Version)
            .map_err(|why| format!("expected.version: {why}")),
        ("version", None) => {
            Err("expected.kind \"version\" takes exactly one more member, version".to_owned())
        }
        (other, _) => Err(format!(
            "expected.kind {other:?} is neither \"absent\" nor \"version\""
        )),
    }
}

/// A compare-and-swap, its errors found in the order a PUT's are.
// api: compareAndSwap
fn cas(request: &Request, config: &Config) -> Result<Attempt, Response> {
    let request_id = write_id(request)?;
    let id = Some(request_id);
    let key = query_key(request, config).map_err(|why| invalid_write(id, &why))?;
    let CasBody { expected, value } = json_body(request, id, "CasRequest")?;
    let expected = expectation(&expected).map_err(|why| invalid_write(id, &why))?;
    let value = write_value(value, config, id)?;
    Ok(Attempt {
        op: ClientOp::Write {
            request_id,
            mutation: Mutation::Cas {
                key: key.clone(),
                expected,
                value,
            },
        },
        kind: Kind::Cas { request_id, key },
    })
}

/// A DELETE: no body.
// api: deleteKey
fn delete(request: &Request, config: &Config) -> Result<Attempt, Response> {
    let request_id = write_id(request)?;
    let id = Some(request_id);
    let key = query_key(request, config).map_err(|why| invalid_write(id, &why))?;
    match &request.body {
        Body::Bytes(bytes) if bytes.is_empty() => {}
        Body::Bytes(_) => return Err(invalid_write(id, "a DELETE takes no body")),
        Body::TooLarge => return Err(too_large(id, "the request body exceeds 512 KiB (§7.11)")),
    }
    Ok(Attempt {
        op: ClientOp::Write {
            request_id,
            mutation: Mutation::Delete { key: key.clone() },
        },
        kind: Kind::Delete { request_id, key },
    })
}

/// The one `Idempotency-Key` of a write (§8.1). Errors found before it carry no ID.
fn write_id(request: &Request) -> Result<RequestId, Response> {
    match request.field("idempotency-key")[..] {
        [raw] => RequestId::parse(raw)
            .map_err(|why| invalid_write(None, &format!("Idempotency-Key {raw:?}: {why}"))),
        [] => Err(invalid_write(
            None,
            "a write needs an Idempotency-Key header",
        )),
        _ => Err(invalid_write(None, "Idempotency-Key is repeated")),
    }
}

/// The strict JSON body of a write: `application/json`, within the body bound, and exactly
/// the members of `T` (`what` names the schema in errors).
fn json_body<T: serde::de::DeserializeOwned>(
    request: &Request,
    id: Option<RequestId>,
    what: &str,
) -> Result<T, Response> {
    let bytes = match &request.body {
        Body::Bytes(bytes) => bytes,
        Body::TooLarge => {
            return Err(too_large(id, "the request body exceeds 512 KiB (§7.11)"));
        }
    };
    let json_type = request.field("content-type").first().is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json_type {
        return Err(invalid_write(
            id,
            "the body needs Content-Type: application/json",
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|why| invalid_write(id, &format!("the body is not a valid {what}: {why}")))
}

/// A write's value, within the DM-1 limit (`413` past it).
fn write_value(raw: String, config: &Config, id: Option<RequestId>) -> Result<Value, Response> {
    Value::parse(raw, config.key_value_limits()).map_err(|why| match why {
        ValueError::TooLong { .. } => too_large(id, &format!("the value is too large: {why}")),
    })
}

/// A SCAN: `start`, `end`, `after` and `limit`, each at most once, all optional (§8.5).
// api: scanKeys
fn scan(request: &Request, config: &Config) -> Result<Attempt, Response> {
    let mut parameters = query::parse(
        request.query.as_deref(),
        &["start", "end", "after", "limit"],
    )
    .map_err(|why: QueryError| invalid_read(&why.to_string()))?;
    let mut bound = |name: &str| -> Result<Option<Key>, Response> {
        parameters
            .remove(name)
            .map(|raw| {
                Key::parse(raw, config.key_value_limits()).map_err(|why| match why {
                    KeyError::Empty | KeyError::TooLong { .. } => {
                        invalid_read(&format!("bad {name}: {why}"))
                    }
                })
            })
            .transpose()
    };
    let start = bound("start")?;
    let end = bound("end")?;
    let after = bound("after")?;
    let limit = match parameters.remove("limit") {
        None => PageSize::DEFAULT,
        Some(raw) => {
            let digits = !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit());
            let number = digits.then(|| raw.parse::<u64>().ok()).flatten();
            number
                .ok_or_else(|| invalid_read(&format!("limit {raw:?} is not a decimal integer")))
                .and_then(|number| {
                    PageSize::new(number).map_err(|why| invalid_read(&format!("limit: {why}")))
                })?
        }
    };
    let range = ScanRange::new(start, end, after, limit)
        .map_err(|_| invalid_read("start sorts after end"))?;
    Ok(Attempt {
        op: ClientOp::Scan { range },
        kind: Kind::Scan,
    })
}

/// A status lookup of the request ID in the path.
// api: getRequestStatus
fn status(request: &Request, raw: &str) -> Result<Attempt, Response> {
    no_query(request)?;
    let request_id =
        RequestId::parse(raw).map_err(|why| invalid_read(&format!("request ID {raw:?}: {why}")))?;
    Ok(Attempt {
        op: ClientOp::Status { request_id },
        kind: Kind::Status { request_id },
    })
}

/// A GET.
// api: getKey
fn get(request: &Request, config: &Config) -> Result<Attempt, Response> {
    let key = query_key(request, config).map_err(|why| invalid_read(&why))?;
    Ok(Attempt {
        op: ClientOp::Get { key: key.clone() },
        kind: Kind::Get { key },
    })
}

/// The request's `key` parameter, decoded and checked against DM-1.
fn query_key(request: &Request, config: &Config) -> Result<Key, String> {
    let mut parameters = query::parse(request.query.as_deref(), &["key"])
        .map_err(|why: QueryError| why.to_string())?;
    let raw = parameters
        .remove("key")
        .ok_or_else(|| "the key query parameter is required".to_owned())?;
    Key::parse(raw, config.key_value_limits()).map_err(|why| match why {
        KeyError::Empty | KeyError::TooLong { .. } => format!("bad key: {why}"),
    })
}

/// Health and cluster endpoints take no query.
fn no_query(request: &Request) -> Result<(), Response> {
    query::parse(request.query.as_deref(), &[])
        .map(|_| ())
        .map_err(|why| invalid_read(&why.to_string()))
}

/// Renders the core's answer to `attempt` (§8.4).
#[must_use]
pub fn render(attempt: &Attempt, response: CoreResponse, view: View<'_>) -> Response {
    match (&attempt.kind, response) {
        (Kind::Put { request_id, key } | Kind::Delete { request_id, key }, response) => {
            render_write(&attempt.kind, *request_id, key, "/v1/kv", &response, view)
        }
        (Kind::Cas { request_id, key }, response) => {
            render_write(&attempt.kind, *request_id, key, "/v1/cas", &response, view)
        }
        (
            Kind::Get { key },
            CoreResponse::Found {
                value,
                version,
                served_by,
            },
        ) => Response::json(
            200,
            json!({
                "key": key.as_str(),
                "value": value.as_str(),
                "version": version.get(),
                "served_by": served(served_by),
            }),
        ),
        (Kind::Get { .. }, CoreResponse::NotFound { served_by }) => error(
            404,
            "not_found",
            "the key is absent in the applied prefix in served_by",
            details([("served_by", served(served_by))]),
        ),
        (Kind::Scan, CoreResponse::Page(page)) => scan_page(page),
        (Kind::Status { request_id }, CoreResponse::Status { state, served_by }) => {
            status_body(*request_id, state, served_by)
        }
        (Kind::Get { .. } | Kind::Scan | Kind::Status { .. }, CoreResponse::Refused(refusal)) => {
            read_refusal(refusal, view)
        }
        (
            kind @ (Kind::Get { .. } | Kind::Scan | Kind::Status { .. }),
            response @ (CoreResponse::Written { .. }
            | CoreResponse::Conflict { .. }
            | CoreResponse::Found { .. }
            | CoreResponse::NotFound { .. }
            | CoreResponse::Page(_)
            | CoreResponse::Status { .. }),
        ) => mismatch(kind, &response, view),
    }
}

/// The core's answer to a PUT, DELETE or compare-and-swap.
fn render_write(
    kind: &Kind,
    request_id: RequestId,
    key: &Key,
    path: &str,
    response: &CoreResponse,
    view: View<'_>,
) -> Response {
    match response {
        CoreResponse::Written {
            result,
            replayed,
            served_by,
        } => written(request_id, *result, *replayed, *served_by),
        CoreResponse::Conflict { original_index } => error(
            409,
            "request_id_conflict",
            "the request ID was already used for a different command; nothing changed",
            details([
                ("request_id", json!(request_id.to_string())),
                ("original_index", json!(original_index.get())),
            ]),
        ),
        CoreResponse::Refused(refusal) => write_refusal(request_id, key, path, *refusal, view),
        CoreResponse::Found { .. }
        | CoreResponse::NotFound { .. }
        | CoreResponse::Page(_)
        | CoreResponse::Status { .. } => mismatch(kind, response, view),
    }
}

/// A core answer that cannot answer this kind of attempt: a bug (CD-7).
fn mismatch(kind: &Kind, response: &CoreResponse, view: View<'_>) -> Response {
    types::invariant!(
        false,
        "API-ANSWER-KIND",
        "the core answered a {kind:?} with {response:?}"
    );
    unavailable_read(view)
}

/// A SCAN page (§8.5): `next_after` is the continuation, so `has_more` is its presence.
fn scan_page(page: ScanPage) -> Response {
    let (items, continuation, served_by) = page.into_parts();
    let items: Vec<Json> = items
        .iter()
        .map(|item| {
            json!({
                "key": item.key.as_str(),
                "value": item.value.as_str(),
                "version": item.version.get(),
            })
        })
        .collect();
    Response::json(
        200,
        json!({
            "items": items,
            "has_more": continuation.is_some(),
            "next_after": continuation.as_ref().map(Key::as_str),
            "served_by": served(served_by),
        }),
    )
}

/// A status lookup (F-9): `200` whatever the stored decision was; the embedded result is
/// the body a retry of the original would get now (`replayed: true`).
fn status_body(request_id: RequestId, state: RequestState, served_by: ServedBy) -> Response {
    match state {
        RequestState::Known(result) => Response::json(
            200,
            json!({
                "state": "known",
                "result": written(request_id, result, true, served_by).body,
                "served_by": served(served_by),
            }),
        ),
        RequestState::Unknown => Response::json(
            200,
            json!({"state": "unknown", "served_by": served(served_by)}),
        ),
    }
}

/// A write's terminal result (§8.3, §8.4): `200`, or its terminal error.
fn written(
    request_id: RequestId,
    result: TerminalResult,
    replayed: bool,
    served_by: ServedBy,
) -> Response {
    let index = result.index().get();
    match result.decision() {
        Decision::Set => Response::json(
            200,
            json!({
                "request_id": request_id.to_string(),
                "index": index,
                "version": result.version().map_or(index, Version::get),
                "replayed": replayed,
                "served_by": served(served_by),
            }),
        ),
        Decision::Deleted { existed } => Response::json(
            200,
            json!({
                "request_id": request_id.to_string(),
                "index": index,
                "existed": existed,
                "replayed": replayed,
                "served_by": served(served_by),
            }),
        ),
        Decision::VersionMismatch { current } => error(
            409,
            "version_mismatch",
            "the compare-and-swap's expectation did not match; it changed nothing",
            details([
                ("current_version", json!(current.map(Version::get))),
                ("request_id", json!(request_id.to_string())),
                ("index", json!(index)),
                ("replayed", json!(replayed)),
            ]),
        ),
        Decision::CapacityExceeded { limit } => error(
            507,
            "capacity_exceeded",
            "the live-state quota refused the write; it changed nothing",
            details([
                ("request_id", json!(request_id.to_string())),
                ("limit", json!(limit.as_str())),
                ("index", json!(index)),
                ("replayed", json!(replayed)),
            ]),
        ),
    }
}

/// A write's non-terminal refusal (§8.4), with `request_id` and `attempt_state`.
fn write_refusal(
    request_id: RequestId,
    key: &Key,
    path: &str,
    refusal: Refusal,
    view: View<'_>,
) -> Response {
    let id = json!(request_id.to_string());
    let not_submitted = json!("not_submitted");
    match refusal {
        Refusal::NotLeader { leader } => {
            let mut response = error(
                307,
                "not_leader",
                &format!("this node is not the leader; the leader is node {leader}"),
                details([
                    ("leader_hint", json!(leader.get())),
                    ("request_id", id),
                    ("attempt_state", not_submitted),
                ]),
            );
            response.location = view.config.node(leader).map(|node| {
                format!(
                    "http://{}{path}?key={}",
                    node.client_address(),
                    query::encode(key.as_str())
                )
            });
            response
        }
        Refusal::Busy => busy_write(request_id),
        Refusal::Unavailable => error(
            503,
            "unavailable",
            "no leader is known, or the node is shutting down; retry, possibly at another node",
            details([
                ("request_id", id),
                ("attempt_state", not_submitted),
                ("nodes", nodes(view)),
            ]),
        ),
        Refusal::Recovering { lifecycle } => error(
            503,
            "recovering",
            &format!("the node is not ready ({})", not_ready_name(lifecycle)),
            details([
                ("lifecycle", json!(not_ready_name(lifecycle))),
                ("request_id", id),
                ("attempt_state", not_submitted),
            ]),
        ),
        Refusal::UnknownOutcome => unknown_outcome(request_id, view),
        Refusal::HistoryFull { limit } => error(
            507,
            "history_full",
            "the history capacity refused admission before proposal",
            details([
                ("request_id", id),
                ("limit", json!(limit.as_str())),
                ("attempt_state", not_submitted),
            ]),
        ),
    }
}

/// A read's refusal (§8.4): no write fields.
fn read_refusal(refusal: Refusal, view: View<'_>) -> Response {
    match refusal {
        Refusal::Busy => error(
            429,
            "busy",
            "a local admission bound was hit; back off and retry",
            Map::new(),
        ),
        Refusal::Recovering { lifecycle } => recovering_read(lifecycle),
        Refusal::Unavailable
        | Refusal::NotLeader { .. }
        | Refusal::UnknownOutcome
        | Refusal::HistoryFull { .. } => unavailable_read(view),
    }
}

/// `429 busy` for a write the hand-off refused: it was not submitted (§7.11).
#[must_use]
pub fn busy(attempt: &Attempt) -> Response {
    attempt.kind.write_id().map_or_else(
        || {
            error(
                429,
                "busy",
                "a local admission bound was hit; back off and retry",
                Map::new(),
            )
        },
        busy_write,
    )
}

/// `429 busy` for a connection past the bound, before any request is read (CD-9).
#[must_use]
pub fn busy_connection() -> Response {
    error(
        429,
        "busy",
        "too many open connections; back off and retry",
        Map::new(),
    )
}

fn busy_write(request_id: RequestId) -> Response {
    error(
        429,
        "busy",
        "a local admission bound was hit; back off and retry the same request",
        details([
            ("request_id", json!(request_id.to_string())),
            ("attempt_state", json!("not_submitted")),
        ]),
    )
}

/// The answer when the core did not answer in time: a handed-over write's outcome is
/// unknown (A-5), a read is unavailable.
#[must_use]
pub fn timed_out(attempt: &Attempt, view: View<'_>) -> Response {
    attempt
        .kind
        .write_id()
        .map_or_else(|| unavailable_read(view), |id| unknown_outcome(id, view))
}

fn unknown_outcome(request_id: RequestId, view: View<'_>) -> Response {
    error(
        504,
        "unknown_outcome",
        "the write was submitted but its commit was not observed; retry the same request or \
         query its status",
        details([
            ("request_id", json!(request_id.to_string())),
            ("attempt_state", json!("submitted")),
            ("guidance", json!("retry_same_request_or_query_status")),
            ("nodes", nodes(view)),
        ]),
    )
}

fn unavailable_read(view: View<'_>) -> Response {
    error(
        503,
        "unavailable",
        "the node is shutting down",
        details([("nodes", nodes(view))]),
    )
}

fn recovering_read(lifecycle: NotReadyLifecycle) -> Response {
    error(
        503,
        "recovering",
        &format!("the node is not ready ({})", not_ready_name(lifecycle)),
        details([("lifecycle", json!(not_ready_name(lifecycle)))]),
    )
}

/// `GET /v1/health/live` (O-1): the process answers, in its lifecycle state.
// api: getLiveness
#[must_use]
pub fn live(snapshot: &Snapshot) -> Response {
    Response::json(
        200,
        json!({
            "status": "ok",
            "node_id": snapshot.node.get(),
            "lifecycle": lifecycle_name(snapshot.lifecycle),
        }),
    )
}

/// `GET /v1/health/ready` (O-1): 200 only when a complete committed read view is served.
// api: getReadiness
#[must_use]
pub fn ready(snapshot: &Snapshot) -> Response {
    snapshot.lifecycle.not_ready().map_or_else(
        || {
            Response::json(
                200,
                json!({
                    "status": "ok",
                    "node_id": snapshot.node.get(),
                    "lifecycle": "ready",
                }),
            )
        },
        recovering_read,
    )
}

/// `GET /v1/cluster` (O-1): this node's local view of the group.
// api: getClusterStatus
#[must_use]
pub fn cluster(snapshot: &Snapshot, view: View<'_>) -> Response {
    let mut voters: Vec<(NodeId, String)> = view
        .config
        .nodes()
        .map(|node| (node.id(), format!("http://{}", node.client_address())))
        .collect();
    voters.sort_by_key(|(id, _)| id.get());
    let peers: Vec<Json> = voters
        .into_iter()
        .map(|(id, client_address)| {
            let mut peer = Map::new();
            peer.insert("id".to_owned(), json!(id.get()));
            peer.insert("client_address".to_owned(), json!(client_address));
            if let Some((_, matched)) = snapshot.matched.iter().find(|(node, _)| *node == id) {
                peer.insert("match_index".to_owned(), json!(matched.get()));
            }
            Json::Object(peer)
        })
        .collect();
    Response::json(
        200,
        json!({
            "node_id": snapshot.node.get(),
            "cluster_id": view.config.cluster_id().to_string(),
            "role": role_name(snapshot.role),
            "term": snapshot.term.get(),
            "leader_id": snapshot.leader.map(NodeId::get),
            "durable_last_index": snapshot.durable_last.get(),
            "commit_index": snapshot.commit.get(),
            "applied_index": snapshot.applied.get(),
            "peers": peers,
        }),
    )
}

/// Every configured voter's client address, in ascending node-ID order (§8.4 `nodes`).
fn nodes(view: View<'_>) -> Json {
    let mut nodes: Vec<(u64, String)> = view
        .config
        .nodes()
        .map(|node| (node.id().get(), format!("http://{}", node.client_address())))
        .collect();
    nodes.sort_unstable();
    Json::Array(nodes.into_iter().map(|(_, url)| json!(url)).collect())
}

fn served(served_by: ServedBy) -> Json {
    json!({
        "node_id": served_by.node.get(),
        "applied_index": served_by.applied.get(),
    })
}

const fn not_ready_name(lifecycle: NotReadyLifecycle) -> &'static str {
    lifecycle_name(lifecycle.lifecycle())
}

fn details<const N: usize>(fields: [(&str, Json); N]) -> Map<String, Json> {
    fields
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

/// The error envelope (§8.1): `{"error": {"code", "message", "details"}}`.
fn error(status: u16, code: &str, message: &str, details: Map<String, Json>) -> Response {
    let mut error = Map::new();
    error.insert("code".to_owned(), json!(code));
    error.insert("message".to_owned(), json!(message));
    error.insert("details".to_owned(), Json::Object(details));
    let mut body = Map::new();
    body.insert("error".to_owned(), Json::Object(error));
    Response::json(status, Json::Object(body))
}

/// `400 invalid_request` on a write, with the request ID once it is known (§8.1).
fn invalid_write(request_id: Option<RequestId>, message: &str) -> Response {
    let mut fields = Map::new();
    if let Some(request_id) = request_id {
        fields.insert("request_id".to_owned(), json!(request_id.to_string()));
    }
    error(400, "invalid_request", message, fields)
}

/// `400 invalid_request` on a read: no write fields.
fn invalid_read(message: &str) -> Response {
    error(400, "invalid_request", message, Map::new())
}

/// `413 too_large`, with the request ID once it is known.
fn too_large(request_id: Option<RequestId>, message: &str) -> Response {
    let mut fields = Map::new();
    if let Some(request_id) = request_id {
        fields.insert("request_id".to_owned(), json!(request_id.to_string()));
    }
    let mut response = error(413, "too_large", message, fields);
    response.close = true;
    response
}

fn not_allowed(allow: &'static str, message: &str) -> Response {
    let mut response = error(405, "invalid_request", message, Map::new());
    response.allow = Some(allow);
    response
}

/// `400 invalid_request` for a request that is not well-formed HTTP.
#[must_use]
pub fn malformed(message: &str) -> Response {
    let mut response = invalid_read(message);
    response.close = true;
    response
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value as Json, json};
    use types::{
        ClientOp, Config, Decision, EntryIndex, HistoryLimit, Key, Lifecycle, LiveStateLimit,
        LogIndex, NodeId, NotReadyLifecycle, Refusal, RequestId, Response as CoreResponse, Role,
        ServedBy, Term, TerminalResult, Value, Version,
    };

    use super::{Attempt, Kind, Routed, View, busy, cluster, live, ready, render, route};
    use crate::http::{Body, Request, Response};
    use crate::runtime::Snapshot;

    const ONE_VOTER: &str = include_str!("../../../config/fixtures/one-voter.toml");
    const THREE_VOTERS: &str = include_str!("../../../config/fixtures/three-voters.toml");
    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn config(text: &str) -> Config {
        Config::parse(text, Path::new("config/fixtures")).unwrap()
    }

    fn request(method: &str, target: &str, headers: &[(&str, &str)], body: Body) -> Request {
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
            None => (target.to_owned(), None),
        };
        Request {
            method: method.to_owned(),
            path,
            query,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body,
            close: false,
        }
    }

    fn put(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Routed, Response> {
        route(
            &request("PUT", target, headers, Body::Bytes(body.to_vec())),
            &config(ONE_VOTER),
        )
    }

    const GOOD: [(&str, &str); 2] = [
        ("idempotency-key", ID),
        ("content-type", "application/json"),
    ];

    /// The status, code and details of an error response.
    fn refusal(routed: Result<Routed, Response>) -> (u16, Json, Json) {
        let response = routed.unwrap_err();
        (
            response.status,
            response.body["error"]["code"].clone(),
            response.body["error"]["details"].clone(),
        )
    }

    fn with_id() -> Json {
        json!({"request_id": ID})
    }

    #[test]
    fn a_put_parses_into_the_core_s_domain_types() {
        let routed = put("/v1/kv?key=a%2Fb", &GOOD, br#"{"value":"v"}"#).unwrap();
        let limits = config(ONE_VOTER).key_value_limits();
        let key = Key::parse("a/b".to_owned(), limits).unwrap();
        let request_id = RequestId::parse(ID).unwrap();
        assert_eq!(
            routed,
            Routed::Attempt(Attempt {
                op: ClientOp::Write {
                    request_id,
                    mutation: types::Mutation::Put {
                        key: key.clone(),
                        value: Value::parse("v".to_owned(), limits).unwrap(),
                    },
                },
                kind: Kind::Put { request_id, key },
            }),
            "rule: a PUT is parsed into domain types at the boundary: the decoded key, the \
             value and the request ID (CD-3, §8.2)"
        );
    }

    #[test]
    fn a_put_s_header_query_and_body_are_parsed_strictly() {
        let too_long_key = format!("/v1/kv?key={}", "k".repeat(257));
        let too_long_value = json!({"value": "v".repeat(65_537)}).to_string();
        let cases: Vec<(&str, (u16, Json, Json))> = vec![
            (
                "no Idempotency-Key",
                refusal(put("/v1/kv?key=a", &GOOD[1..], br#"{"value":"v"}"#)),
            ),
            (
                "an upper-case Idempotency-Key",
                refusal(put(
                    "/v1/kv?key=a",
                    &[("idempotency-key", &ID.to_uppercase()), GOOD[1]],
                    br#"{"value":"v"}"#,
                )),
            ),
            ("no key", refusal(put("/v1/kv", &GOOD, br#"{"value":"v"}"#))),
            (
                "an unescaped +",
                refusal(put("/v1/kv?key=a+b", &GOOD, br#"{"value":"v"}"#)),
            ),
            (
                "a repeated key",
                refusal(put("/v1/kv?key=a&key=b", &GOOD, br#"{"value":"v"}"#)),
            ),
            (
                "an empty key",
                refusal(put("/v1/kv?key=", &GOOD, br#"{"value":"v"}"#)),
            ),
            (
                "a 257-byte key",
                refusal(put(&too_long_key, &GOOD, br#"{"value":"v"}"#)),
            ),
            (
                "no Content-Type",
                refusal(put("/v1/kv?key=a", &GOOD[..1], br#"{"value":"v"}"#)),
            ),
            (
                "an unknown field",
                refusal(put("/v1/kv?key=a", &GOOD, br#"{"value":"v","x":1}"#)),
            ),
            (
                "a duplicate member",
                refusal(put("/v1/kv?key=a", &GOOD, br#"{"value":"v","value":"w"}"#)),
            ),
            (
                "malformed UTF-8",
                refusal(put("/v1/kv?key=a", &GOOD, b"{\"value\":\"\xff\"}")),
            ),
            (
                "a number value",
                refusal(put("/v1/kv?key=a", &GOOD, br#"{"value":1e999}"#)),
            ),
            (
                "trailing data",
                refusal(put("/v1/kv?key=a", &GOOD, br#"{"value":"v"} {}"#)),
            ),
            ("no body", refusal(put("/v1/kv?key=a", &GOOD, b""))),
            (
                "a 64 KiB + 1 value",
                refusal(put("/v1/kv?key=a", &GOOD, too_long_value.as_bytes())),
            ),
            (
                "a body over 512 KiB",
                refusal(route(
                    &request("PUT", "/v1/kv?key=a", &GOOD, Body::TooLarge),
                    &config(ONE_VOTER),
                )),
            ),
        ];
        let invalid = |details: Json| (400, json!("invalid_request"), details);
        let expected = vec![
            invalid(json!({})),
            invalid(json!({})),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            invalid(with_id()),
            (413, json!("too_large"), with_id()),
            (413, json!("too_large"), with_id()),
        ];
        let observed: Vec<(u16, Json, Json)> = cases.iter().map(|(_, got)| got.clone()).collect();
        assert_eq!(
            observed,
            expected,
            "rule: strict parsing (§8.1): a missing or malformed Idempotency-Key, a missing, \
             repeated, '+'-carrying, empty or 257-byte key, a missing Content-Type, an unknown \
             field, a duplicate member, malformed UTF-8, a wrong type, trailing data and a \
             missing body are 400 invalid_request; a value over 64 KiB or a body over 512 KiB \
             is 413 too_large; request_id appears exactly when a well-formed Idempotency-Key \
             was sent; cases: {:?}",
            cases.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn methods_and_paths_are_answered_405_with_allow_or_404() {
        let config = config(ONE_VOTER);
        let routed = |method: &str, target: &str| {
            route(
                &request(method, target, &[], Body::Bytes(Vec::new())),
                &config,
            )
        };
        let shape = |routed: Result<Routed, Response>| {
            let response = routed.unwrap_err();
            (
                response.status,
                response.allow,
                response.body["error"]["code"].clone(),
            )
        };
        assert_eq!(
            [
                shape(routed("POST", "/v1/kv?key=a")),
                shape(routed("POST", "/v1/health/live")),
                shape(routed("GET", "/v1/cas?key=a")),
                shape(routed("PUT", "/v1/scan")),
                shape(routed("POST", &format!("/v1/requests/{ID}"))),
                shape(routed("GET", "/v2/kv")),
                shape(routed("GET", "/v1/cluster?x=1")),
            ],
            [
                (405, Some("DELETE, GET, PUT"), json!("invalid_request")),
                (405, Some("GET"), json!("invalid_request")),
                (405, Some("POST"), json!("invalid_request")),
                (405, Some("GET"), json!("invalid_request")),
                (405, Some("GET"), json!("invalid_request")),
                (404, None, json!("invalid_request")),
                (400, None, json!("invalid_request")),
            ],
            "rule: a method an endpoint does not serve is 405 with Allow naming the methods it \
             does, an unknown path 404, an unknown query parameter 400 (§8.4)"
        );
    }

    fn put_attempt() -> Attempt {
        let limits = config(ONE_VOTER).key_value_limits();
        let key = Key::parse("a b".to_owned(), limits).unwrap();
        let request_id = RequestId::parse(ID).unwrap();
        Attempt {
            op: ClientOp::Write {
                request_id,
                mutation: types::Mutation::Put {
                    key: key.clone(),
                    value: Value::parse("v".to_owned(), limits).unwrap(),
                },
            },
            kind: Kind::Put { request_id, key },
        }
    }

    fn get_attempt() -> Attempt {
        let key = Key::parse("k".to_owned(), config(ONE_VOTER).key_value_limits()).unwrap();
        Attempt {
            op: ClientOp::Get { key: key.clone() },
            kind: Kind::Get { key },
        }
    }

    fn served_by() -> ServedBy {
        ServedBy {
            node: NodeId::new(7).unwrap(),
            applied: LogIndex::new(9).unwrap(),
        }
    }

    fn result(decision: Decision) -> TerminalResult {
        TerminalResult::executed_at(EntryIndex::new(5).unwrap(), decision)
    }

    /// The status, `Location` and body (its message checked and removed) that a PUT's
    /// `response` renders as, on the three-voter fixture.
    fn put_rendered(response: CoreResponse) -> (u16, Option<String>, Json) {
        let three = config(THREE_VOTERS);
        let rendered = render(&put_attempt(), response, View { config: &three });
        let mut body = rendered.body;
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            assert!(
                error
                    .remove("message")
                    .is_some_and(|message| message.is_string()),
                "rule: every error has a message"
            );
        }
        (rendered.status, rendered.location, body)
    }

    fn envelope(code: &str, details: Json) -> Json {
        let mut error = serde_json::Map::new();
        error.insert("code".to_owned(), json!(code));
        error.insert("details".to_owned(), details);
        let mut body = serde_json::Map::new();
        body.insert("error".to_owned(), Json::Object(error));
        Json::Object(body)
    }

    fn written(decision: Decision, replayed: bool) -> CoreResponse {
        CoreResponse::Written {
            result: result(decision),
            replayed,
            served_by: served_by(),
        }
    }

    #[test]
    fn a_put_s_terminal_results_render_as_section_8_3_says() {
        assert_eq!(
            [
                written(Decision::Set, false),
                written(Decision::Set, true),
                written(
                    Decision::CapacityExceeded {
                        limit: LiveStateLimit::Keys,
                    },
                    false,
                ),
                CoreResponse::Conflict {
                    original_index: EntryIndex::new(3).unwrap(),
                },
            ]
            .map(put_rendered),
            [
                (
                    200,
                    None,
                    json!({"request_id": ID, "index": 5, "version": 5,
                    "replayed": false, "served_by": {"node_id": 7, "applied_index": 9}})
                ),
                (
                    200,
                    None,
                    json!({"request_id": ID, "index": 5, "version": 5,
                    "replayed": true, "served_by": {"node_id": 7, "applied_index": 9}})
                ),
                (
                    507,
                    None,
                    envelope(
                        "capacity_exceeded",
                        json!({"request_id": ID,
                    "limit": "max_live_keys", "index": 5, "replayed": false})
                    )
                ),
                (
                    409,
                    None,
                    envelope(
                        "request_id_conflict",
                        json!({"request_id": ID, "original_index": 3})
                    )
                ),
            ],
            "rule: a PUT's result is VersionedWriteResult with version == index, replayed on a \
             retry; a live-quota refusal is the terminal 507 capacity_exceeded and a reused ID \
             409 request_id_conflict (§8.3, §8.4)"
        );
    }

    #[test]
    fn a_put_s_refusals_render_as_section_8_4_says() {
        let nodes = json!([
            "http://127.0.0.1:7203",
            "http://[::1]:7202",
            "http://127.0.0.1:7201"
        ]);
        let leader = NodeId::new(7).unwrap();
        assert_eq!(
            [
                Refusal::NotLeader { leader },
                Refusal::Busy,
                Refusal::Unavailable,
                Refusal::Recovering {
                    lifecycle: NotReadyLifecycle::Recovering,
                },
                Refusal::UnknownOutcome,
                Refusal::HistoryFull {
                    limit: HistoryLimit::LogEntries,
                },
            ]
            .map(|refusal| put_rendered(CoreResponse::Refused(refusal))),
            [
                (
                    307,
                    Some("http://[::1]:7202/v1/kv?key=a%20b".to_owned()),
                    envelope(
                        "not_leader",
                        json!({"leader_hint": 7, "request_id": ID,
                        "attempt_state": "not_submitted"})
                    )
                ),
                (
                    429,
                    None,
                    envelope(
                        "busy",
                        json!({"request_id": ID,
                    "attempt_state": "not_submitted"})
                    )
                ),
                (
                    503,
                    None,
                    envelope(
                        "unavailable",
                        json!({"request_id": ID,
                    "attempt_state": "not_submitted", "nodes": nodes})
                    )
                ),
                (
                    503,
                    None,
                    envelope(
                        "recovering",
                        json!({"lifecycle": "recovering",
                    "request_id": ID, "attempt_state": "not_submitted"})
                    )
                ),
                (
                    504,
                    None,
                    envelope(
                        "unknown_outcome",
                        json!({"request_id": ID,
                    "attempt_state": "submitted",
                    "guidance": "retry_same_request_or_query_status", "nodes": nodes})
                    )
                ),
                (
                    507,
                    None,
                    envelope(
                        "history_full",
                        json!({"request_id": ID,
                    "limit": "max_log_entries", "attempt_state": "not_submitted"})
                    )
                ),
            ],
            "rule: each refusal of a write has §8.4's status and details; a 307 names the \
             leader's client URL for the same path and re-encoded query; nodes lists every \
             voter's client URL in ascending node-ID order (§8.1, §8.4)"
        );
    }

    #[test]
    fn every_core_answer_to_a_get_and_a_refused_hand_off_render_as_section_8_4_says() {
        let one = config(ONE_VOTER);
        let view = View { config: &one };
        let attempt = get_attempt();
        let limits = one.key_value_limits();
        let rendered = |response: CoreResponse| {
            let response = render(&attempt, response, view);
            let mut body = response.body;
            if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
                error.remove("message");
            }
            (response.status, body)
        };
        let handed_off = busy(&put_attempt());
        assert_eq!(
            (
                [
                    CoreResponse::Found {
                        value: Value::parse("v".to_owned(), limits).unwrap(),
                        version: Version::at(LogIndex::new(5).unwrap()).unwrap(),
                        served_by: served_by(),
                    },
                    CoreResponse::NotFound {
                        served_by: served_by()
                    },
                    CoreResponse::Refused(Refusal::Busy),
                    CoreResponse::Refused(Refusal::Unavailable),
                    CoreResponse::Refused(Refusal::Recovering {
                        lifecycle: NotReadyLifecycle::Failed,
                    }),
                ]
                .map(rendered),
                (
                    handed_off.status,
                    handed_off.body["error"]["details"].clone()
                ),
            ),
            (
                [
                    (
                        200,
                        json!({"key": "k", "value": "v", "version": 5,
                        "served_by": {"node_id": 7, "applied_index": 9}})
                    ),
                    (
                        404,
                        json!({"error": {"code": "not_found",
                        "details": {"served_by": {"node_id": 7, "applied_index": 9}}}})
                    ),
                    (429, json!({"error": {"code": "busy", "details": {}}})),
                    (
                        503,
                        json!({"error": {"code": "unavailable",
                        "details": {"nodes": ["http://127.0.0.1:7101"]}}})
                    ),
                    (
                        503,
                        json!({"error": {"code": "recovering",
                        "details": {"lifecycle": "failed"}}})
                    ),
                ],
                (
                    429,
                    json!({"request_id": ID, "attempt_state": "not_submitted"})
                ),
            ),
            "rule: a GET answers ReadResult or not_found with served_by, and its refusals carry \
             no write fields; a write the hand-off refused is busy, not submitted (§8.4, \
             §7.11)"
        );
    }

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        let node = NodeId::new(1).unwrap();
        Snapshot {
            node,
            lifecycle,
            role: Role::Leader,
            term: Term::new(3).unwrap(),
            leader: Some(node),
            commit: LogIndex::new(4).unwrap(),
            last: LogIndex::new(5).unwrap(),
            durable_last: LogIndex::new(5).unwrap(),
            matched: vec![(node, LogIndex::new(5).unwrap())],
            applied: LogIndex::new(4).unwrap(),
            ticks: 10,
            clients_refused: 0,
            peers_dropped: 0,
            log_records_dropped: 0,
        }
    }

    #[test]
    fn health_and_cluster_render_the_published_snapshot() {
        let one = config(ONE_VOTER);
        let view = View { config: &one };
        let body = |response: Response| (response.status, response.body);
        let mut not_ready = ready(&snapshot(Lifecycle::Recovering)).body;
        not_ready["error"]
            .as_object_mut()
            .unwrap()
            .remove("message");
        assert_eq!(
            (
                body(live(&snapshot(Lifecycle::Recovering))),
                body(ready(&snapshot(Lifecycle::Ready))),
                not_ready,
                body(cluster(&snapshot(Lifecycle::Ready), view)),
            ),
            (
                (
                    200,
                    json!({"status": "ok", "node_id": 1, "lifecycle": "recovering"})
                ),
                (
                    200,
                    json!({"status": "ok", "node_id": 1, "lifecycle": "ready"})
                ),
                json!({"error": {"code": "recovering", "details": {"lifecycle": "recovering"}}}),
                (
                    200,
                    json!({"node_id": 1, "cluster_id": "keyval-fixture-one-voter",
                    "role": "leader", "term": 3, "leader_id": 1, "durable_last_index": 5,
                    "commit_index": 4, "applied_index": 4,
                    "peers": [{"id": 1, "client_address": "http://127.0.0.1:7101",
                        "match_index": 5}]})
                ),
            ),
            "rule: live reports the lifecycle; ready is 200 only when ready, else 503 \
             recovering; the cluster view has every voter with its client URL and, as leader, \
             its match index (O-1, §8.2)"
        );
    }

    fn call(
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Routed, Response> {
        route(
            &request(method, target, headers, Body::Bytes(body.to_vec())),
            &config(ONE_VOTER),
        )
    }

    fn attempt_of(routed: Result<Routed, Response>) -> Attempt {
        match routed {
            Ok(Routed::Attempt(attempt)) => attempt,
            other => panic!("rule: expected a client attempt; got {other:?}"),
        }
    }

    fn rendered(attempt: &Attempt, response: CoreResponse) -> Response {
        let three = config(THREE_VOTERS);
        render(attempt, response, View { config: &three })
    }

    /// The body of `response` rendered for `attempt`, an error's message removed.
    fn shown(attempt: &Attempt, response: CoreResponse) -> Json {
        let mut body = rendered(attempt, response).body;
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            assert!(
                error
                    .remove("message")
                    .is_some_and(|message| message.is_string()),
                "rule: every error has a message"
            );
        }
        body
    }

    fn invalid(details: Json) -> (u16, Json, Json) {
        (400, json!("invalid_request"), details)
    }

    fn delete_attempt() -> Attempt {
        attempt_of(call("DELETE", "/v1/kv?key=a%20b", &GOOD[..1], b""))
    }

    fn cas_attempt() -> Attempt {
        attempt_of(call(
            "POST",
            "/v1/cas?key=a%20b",
            &GOOD,
            br#"{"expected":{"kind":"absent"},"value":"v"}"#,
        ))
    }

    fn scan_attempt() -> Attempt {
        attempt_of(call("GET", "/v1/scan", &[], b""))
    }

    fn status_attempt() -> Attempt {
        attempt_of(call("GET", &format!("/v1/requests/{ID}"), &[], b""))
    }

    #[test]
    fn a_delete_parses_into_a_delete_and_takes_no_body() {
        let limits = config(ONE_VOTER).key_value_limits();
        let key = Key::parse("a/b".to_owned(), limits).unwrap();
        let request_id = RequestId::parse(ID).unwrap();
        let parsed = attempt_of(call("DELETE", "/v1/kv?key=a%2Fb", &GOOD[..1], b""));
        assert_eq!(
            parsed,
            Attempt {
                op: ClientOp::Write {
                    request_id,
                    mutation: types::Mutation::Delete { key: key.clone() },
                },
                kind: Kind::Delete { request_id, key },
            },
            "rule: a DELETE is parsed into domain types at the boundary (CD-3, F-3)"
        );
        assert_eq!(
            [
                refusal(call("DELETE", "/v1/kv?key=a", &[], b"")),
                refusal(call(
                    "DELETE",
                    "/v1/kv?key=a",
                    &GOOD[..1],
                    br#"{"value":"v"}"#
                )),
                refusal(call("DELETE", "/v1/kv", &GOOD[..1], b"")),
                refusal(call("DELETE", "/v1/kv?key=", &GOOD[..1], b"")),
                refusal(call("DELETE", "/v1/kv?key=a&key=b", &GOOD[..1], b"")),
            ],
            [
                invalid(json!({})),
                invalid(with_id()),
                invalid(with_id()),
                invalid(with_id()),
                invalid(with_id()),
            ],
            "rule: a DELETE needs one Idempotency-Key, one non-empty key and no body; \
             request_id appears exactly when a well-formed Idempotency-Key was sent (§8.1)"
        );
    }

    #[test]
    fn a_cas_is_parsed_strictly() {
        let limits = config(ONE_VOTER).key_value_limits();
        let key = Key::parse("k".to_owned(), limits).unwrap();
        let request_id = RequestId::parse(ID).unwrap();
        let cas = |body: &str| call("POST", "/v1/cas?key=k", &GOOD, body.as_bytes());
        let parsed = |body: &str| attempt_of(cas(body)).op;
        let expect = |expected| ClientOp::Write {
            request_id,
            mutation: types::Mutation::Cas {
                key: key.clone(),
                expected,
                value: Value::parse("v".to_owned(), limits).unwrap(),
            },
        };
        assert_eq!(
            [
                parsed(r#"{"expected":{"kind":"absent"},"value":"v"}"#),
                parsed(r#"{"expected":{"kind":"version","version":41},"value":"v"}"#),
                parsed(r#"{"value":"v","expected":{"version":9007199254740991,"kind":"version"}}"#),
            ],
            [
                expect(types::ExpectedVersion::Absent),
                expect(types::ExpectedVersion::Version(Version::new(41).unwrap())),
                expect(types::ExpectedVersion::Version(
                    Version::new(9_007_199_254_740_991).unwrap()
                )),
            ],
            "rule: a CAS body is parsed into an ExpectedVersion; 2^53-1 is the largest version"
        );
    }

    #[test]
    fn a_malformed_cas_is_refused_whatever_the_rule_it_breaks() {
        let cas = |body: &str| call("POST", "/v1/cas?key=k", &GOOD, body.as_bytes());
        let bad = [
            (
                "version 2^53",
                r#"{"expected":{"kind":"version","version":9007199254740992},"value":"v"}"#,
            ),
            (
                "version 0",
                r#"{"expected":{"kind":"version","version":0},"value":"v"}"#,
            ),
            (
                "negative version",
                r#"{"expected":{"kind":"version","version":-1},"value":"v"}"#,
            ),
            (
                "fractional version",
                r#"{"expected":{"kind":"version","version":1.5},"value":"v"}"#,
            ),
            (
                "string version",
                r#"{"expected":{"kind":"version","version":"4"},"value":"v"}"#,
            ),
            (
                "version missing",
                r#"{"expected":{"kind":"version"},"value":"v"}"#,
            ),
            (
                "version on absent",
                r#"{"expected":{"kind":"absent","version":1},"value":"v"}"#,
            ),
            (
                "repeated kind",
                r#"{"expected":{"kind":"version","kind":"absent"},"value":"v"}"#,
            ),
            (
                "repeated kind upgrading absent to version",
                r#"{"expected":{"kind":"absent","kind":"version","version":3},"value":"v"}"#,
            ),
            (
                "repeated version",
                r#"{"expected":{"kind":"version","version":1,"version":2},"value":"v"}"#,
            ),
            (
                "repeated value",
                r#"{"expected":{"kind":"absent"},"value":"v","value":"w"}"#,
            ),
            ("unknown kind", r#"{"expected":{"kind":"any"},"value":"v"}"#),
            ("no kind", r#"{"expected":{},"value":"v"}"#),
            ("no expected", r#"{"value":"v"}"#),
            ("no value", r#"{"expected":{"kind":"absent"}}"#),
            (
                "unknown member",
                r#"{"expected":{"kind":"absent"},"value":"v","x":1}"#,
            ),
            (
                "non-string value",
                r#"{"expected":{"kind":"absent"},"value":1}"#,
            ),
            ("not an object", "[]"),
        ];
        let observed: Vec<_> = bad.iter().map(|(_, body)| refusal(cas(body))).collect();
        assert_eq!(
            observed,
            vec![invalid(with_id()); bad.len()],
            "rule: an expected version of 0 or at least 2^53, a malformed or unknown \
             expectation and any unknown or missing member are 400 invalid_request carrying \
             the request ID; cases: {:?}",
            bad.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        let big = json!({"expected": {"kind": "absent"}, "value": "v".repeat(65_537)}).to_string();
        assert_eq!(
            [
                refusal(cas(&big)),
                refusal(call("POST", "/v1/cas?key=k", &GOOD[..1], b"{}")),
                refusal(call("POST", "/v1/cas?key=k", &GOOD[1..], b"{}")),
                refusal(call("POST", "/v1/cas", &GOOD, b"{}")),
            ],
            [
                (413, json!("too_large"), with_id()),
                invalid(with_id()),
                invalid(json!({})),
                invalid(with_id()),
            ],
            "rule: a value over 64 KiB is 413; a missing Content-Type, Idempotency-Key or key \
             is 400 (§8.1)"
        );
    }

    #[test]
    fn a_scan_s_parameters_are_parsed_strictly() {
        let limits = config(ONE_VOTER).key_value_limits();
        let key = |text: &str| Key::parse(text.to_owned(), limits).unwrap();
        let scan = |query: &str| call("GET", &format!("/v1/scan{query}"), &[], b"");
        let range = |start: Option<&str>, end: Option<&str>, after: Option<&str>, limit: u64| {
            ClientOp::Scan {
                range: types::ScanRange::new(
                    start.map(key),
                    end.map(key),
                    after.map(key),
                    types::PageSize::new(limit).unwrap(),
                )
                .unwrap(),
            }
        };
        assert_eq!(
            [
                attempt_of(scan("")).op,
                attempt_of(scan("?start=a&end=b%2F&after=a%20b&limit=128")).op,
                attempt_of(scan("?start=m&end=m&limit=1")).op,
            ],
            [
                range(None, None, None, 32),
                range(Some("a"), Some("b/"), Some("a b"), 128),
                range(Some("m"), Some("m"), None, 1),
            ],
            "rule: every SCAN parameter is optional, limit defaults to 32, equal bounds are a \
             valid (empty) range (§8.5)"
        );
        let bad = [
            "?start=b&end=a",
            "?start=",
            "?end=",
            "?after=",
            "?limit=0",
            "?limit=129",
            "?limit=",
            "?limit=abc",
            "?limit=%2B5",
            "?limit=-1",
            "?limit=1.0",
            "?limit=99999999999999999999999",
            "?limit=1&limit=2",
            "?start=a+b",
            "?x=1",
            "?start",
        ];
        let observed: Vec<_> = bad.iter().map(|query| refusal(scan(query))).collect();
        assert_eq!(
            observed,
            vec![invalid(json!({})); bad.len()],
            "rule: start > end, an empty bound, a limit outside 1..=128 or not a plain \
             decimal integer, a repeated, unknown or malformed parameter are 400 \
             invalid_request; cases: {bad:?}"
        );
        let long = format!("?start={}", "k".repeat(257));
        assert_eq!(
            refusal(scan(&long)),
            invalid(json!({})),
            "rule: a bound over the key limit is invalid_request"
        );
    }

    #[test]
    fn a_status_lookup_takes_a_request_id_in_the_path_and_no_query() {
        let request_id = RequestId::parse(ID).unwrap();
        let lookup = |target: &str| call("GET", target, &[], b"");
        assert_eq!(
            attempt_of(lookup(&format!("/v1/requests/{ID}"))),
            Attempt {
                op: ClientOp::Status { request_id },
                kind: Kind::Status { request_id },
            },
            "rule: the lookup parses the path's request ID into the domain type (F-9)"
        );
        let bad = [
            "/v1/requests/".to_owned(),
            "/v1/requests/abc".to_owned(),
            format!("/v1/requests/{}", ID.to_uppercase()),
            format!("/v1/requests/{ID}/"),
            format!("/v1/requests/{ID}?x=1"),
            format!("/v1/requests/{ID}00"),
        ];
        let observed: Vec<_> = bad.iter().map(|target| refusal(lookup(target))).collect();
        assert_eq!(
            observed,
            vec![invalid(json!({})); bad.len()],
            "rule: a malformed request ID, a trailing slash or any query is 400 \
             invalid_request; cases: {bad:?}"
        );
    }

    #[test]
    fn a_delete_and_a_cas_render_their_results_and_refusals() {
        let mismatch = |current: Option<u64>| Decision::VersionMismatch {
            current: current.map(|raw| Version::new(raw).unwrap()),
        };
        let delete = delete_attempt();
        let cas = cas_attempt();
        let served = json!({"node_id": 7, "applied_index": 9});
        assert_eq!(
            [
                shown(&delete, written(Decision::Deleted { existed: true }, false)),
                shown(&delete, written(Decision::Deleted { existed: false }, true)),
                shown(&cas, written(Decision::Set, false)),
                shown(&cas, written(mismatch(Some(57)), false)),
                shown(&cas, written(mismatch(None), true)),
                shown(
                    &delete,
                    CoreResponse::Conflict {
                        original_index: EntryIndex::new(3).unwrap()
                    }
                ),
            ],
            [
                json!({"request_id": ID, "index": 5, "existed": true, "replayed": false,
                    "served_by": served}),
                json!({"request_id": ID, "index": 5, "existed": false, "replayed": true,
                    "served_by": served}),
                json!({"request_id": ID, "index": 5, "version": 5, "replayed": false,
                    "served_by": served}),
                envelope(
                    "version_mismatch",
                    json!({"current_version": 57, "request_id": ID, "index": 5,
                        "replayed": false})
                ),
                envelope(
                    "version_mismatch",
                    json!({"current_version": null, "request_id": ID, "index": 5,
                        "replayed": true})
                ),
                envelope(
                    "request_id_conflict",
                    json!({"request_id": ID, "original_index": 3})
                ),
            ],
            "rule: DELETE answers existed, a CAS answers version == index, a mismatch is the \
             terminal version_mismatch with current_version (null when absent), replayed and \
             the original index, a reused ID request_id_conflict with original_index (§8.3, \
             §8.4)"
        );
        let status = |attempt: &Attempt, response| rendered(attempt, response).status;
        assert_eq!(
            [
                status(&delete, written(Decision::Deleted { existed: true }, false)),
                status(&cas, written(mismatch(Some(1)), false)),
                status(
                    &cas,
                    CoreResponse::Conflict {
                        original_index: EntryIndex::new(3).unwrap()
                    }
                ),
                status(&delete, CoreResponse::Refused(Refusal::Busy)),
                status(&cas, CoreResponse::Refused(Refusal::UnknownOutcome)),
                status(
                    &delete,
                    CoreResponse::Refused(Refusal::HistoryFull {
                        limit: HistoryLimit::LogEntries
                    })
                ),
            ],
            [200, 409, 409, 429, 504, 507],
            "rule: statuses per §8.4 for the other write endpoints"
        );
    }

    #[test]
    fn a_redirect_names_the_endpoint_that_was_asked() {
        let leader = NodeId::new(7).unwrap();
        let location = |attempt: &Attempt| {
            let response = rendered(
                attempt,
                CoreResponse::Refused(Refusal::NotLeader { leader }),
            );
            (response.status, response.location)
        };
        let node = config(THREE_VOTERS).node(leader).unwrap().client_address();
        assert_eq!(
            [
                location(&delete_attempt()),
                location(&cas_attempt()),
                location(&put_attempt())
            ],
            [
                (307, Some(format!("http://{node}/v1/kv?key=a%20b"))),
                (307, Some(format!("http://{node}/v1/cas?key=a%20b"))),
                (307, Some(format!("http://{node}/v1/kv?key=a%20b"))),
            ],
            "rule: a 307 Location keeps the endpoint and the re-encoded key (§8.1), so a CAS \
             is redirected to /v1/cas and a DELETE to /v1/kv"
        );
    }

    #[test]
    fn writes_refused_by_the_hand_off_or_timed_out_report_their_attempt_state() {
        let three = config(THREE_VOTERS);
        let view = View { config: &three };
        let state = |response: Response| {
            (
                response.status,
                response.body["error"]["details"]["attempt_state"].clone(),
            )
        };
        assert_eq!(
            [
                state(busy(&delete_attempt())),
                state(busy(&cas_attempt())),
                state(super::timed_out(&delete_attempt(), view)),
                state(super::timed_out(&cas_attempt(), view)),
            ],
            [
                (429, json!("not_submitted")),
                (429, json!("not_submitted")),
                (504, json!("submitted")),
                (504, json!("submitted")),
            ],
            "rule: a hand-off refusal is not_submitted, a silent core submitted (A-5), for \
             every write endpoint"
        );
        let reads = [scan_attempt(), status_attempt()];
        let observed: Vec<_> = reads
            .iter()
            .map(|read| (busy(read).status, super::timed_out(read, view).status))
            .collect();
        assert_eq!(
            observed,
            vec![(429, 503); 2],
            "rule: SCAN and the status lookup are reads: busy is 429, a silent core 503"
        );
    }

    #[test]
    fn a_scan_page_renders_as_the_spec_says() {
        let limits = config(ONE_VOTER).key_value_limits();
        let item = |key: &str, value: &str, version: u64| types::ScanItem {
            key: Key::parse(key.to_owned(), limits).unwrap(),
            value: Value::parse(value.to_owned(), limits).unwrap(),
            version: Version::new(version).unwrap(),
        };
        let range =
            types::ScanRange::new(None, None, None, types::PageSize::new(2).unwrap()).unwrap();
        let page = |items: Vec<types::ScanItem>, more: bool| {
            let next = more.then(|| items.last().unwrap().key.clone());
            CoreResponse::Page(types::ScanPage::new(&range, items, next, served_by()))
        };
        let scan = scan_attempt();
        let served = json!({"node_id": 7, "applied_index": 9});
        assert_eq!(
            [
                shown(
                    &scan,
                    page(vec![item("a", "1", 2), item("b", "2", 3)], true)
                ),
                shown(&scan, page(vec![item("a", "1", 2)], false)),
                shown(&scan, page(Vec::new(), false)),
            ],
            [
                json!({"items": [{"key": "a", "value": "1", "version": 2},
                    {"key": "b", "value": "2", "version": 3}],
                    "has_more": true, "next_after": "b", "served_by": served}),
                json!({"items": [{"key": "a", "value": "1", "version": 2}],
                    "has_more": false, "next_after": null, "served_by": served}),
                json!({"items": [], "has_more": false, "next_after": null, "served_by": served}),
            ],
            "rule: a page lists key, value and version; has_more is true exactly when \
             next_after is the last returned key (§8.5)"
        );
        assert_eq!(rendered(&scan, page(Vec::new(), false)).status, 200);
    }

    #[test]
    fn a_status_lookup_renders_the_original_result_and_is_200_whatever_it_was() {
        let status = status_attempt();
        let served = json!({"node_id": 7, "applied_index": 9});
        let lookup = |state| CoreResponse::Status {
            state,
            served_by: served_by(),
        };
        let known = |decision| lookup(types::RequestState::Known(result(decision)));
        let decisions = [
            Decision::Set,
            Decision::Deleted { existed: false },
            Decision::VersionMismatch {
                current: Some(Version::new(4).unwrap()),
            },
            Decision::CapacityExceeded {
                limit: LiveStateLimit::Bytes,
            },
        ];
        let statuses: Vec<u16> = decisions
            .iter()
            .map(|decision| rendered(&status, known(*decision)).status)
            .collect();
        assert_eq!(
            statuses,
            vec![200; 4],
            "rule: the lookup is 200 whatever the stored decision was (§8.3)"
        );
        let mut observed: Vec<Json> = decisions
            .iter()
            .map(|decision| rendered(&status, known(*decision)).body)
            .collect();
        observed.push(rendered(&status, lookup(types::RequestState::Unknown)).body);
        let result_body = |decision| shown(&delete_attempt_for(decision), written(decision, true));
        assert_eq!(
            observed,
            vec![
                json!({"state": "known", "served_by": served, "result": result_body(decisions[0])}),
                json!({"state": "known", "served_by": served, "result": result_body(decisions[1])}),
                json!({"state": "known", "served_by": served, "result": json!({"error": {
                    "code": "version_mismatch",
                    "message": "the compare-and-swap's expectation did not match; it changed nothing",
                    "details": {"current_version": 4, "request_id": ID, "index": 5,
                        "replayed": true}}})}),
                json!({"state": "known", "served_by": served, "result": json!({"error": {
                    "code": "capacity_exceeded",
                    "message": "the live-state quota refused the write; it changed nothing",
                    "details": {"request_id": ID, "limit": "max_live_bytes", "index": 5,
                        "replayed": true}}})}),
                json!({"state": "unknown", "served_by": served}),
            ],
            "rule: a known request embeds the body a retry of it would get now (replayed true, \
             the original index and decision), an unknown one only served_by (F-9)"
        );
    }

    /// The write attempt whose result a `decision` is: a CAS for a mismatch, else a DELETE.
    fn delete_attempt_for(decision: Decision) -> Attempt {
        match decision {
            Decision::Deleted { .. } => delete_attempt(),
            Decision::Set
            | Decision::VersionMismatch { .. }
            | Decision::CapacityExceeded { .. } => cas_attempt(),
        }
    }

    #[test]
    fn a_read_refusal_is_the_same_for_every_read() {
        let three = config(THREE_VOTERS);
        let view = View { config: &three };
        let scan = scan_attempt();
        let status = status_attempt();
        let codes = |refusal| {
            [&scan, &status].map(|read| {
                let response = render(read, CoreResponse::Refused(refusal), view);
                (response.status, response.body["error"]["code"].clone())
            })
        };
        assert_eq!(
            [
                codes(Refusal::Busy),
                codes(Refusal::Unavailable),
                codes(Refusal::Recovering {
                    lifecycle: NotReadyLifecycle::Recovering
                }),
            ],
            [
                [(429, json!("busy")), (429, json!("busy"))],
                [(503, json!("unavailable")), (503, json!("unavailable"))],
                [(503, json!("recovering")), (503, json!("recovering"))],
            ],
            "rule: SCAN and the status lookup refuse as GET does (§8.4)"
        );
    }
}
