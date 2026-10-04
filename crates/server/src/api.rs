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
//! DELETE, compare-and-swap, SCAN and the status lookup are not served yet. Their work
//! items (W27, W28, F-9) wire them; until then they are refused here, never forwarded to a
//! core that cannot apply them.

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use types::{
    ClientOp, Config, Decision, Key, KeyError, Mutation, NodeId, NotReadyLifecycle, Refusal,
    RequestId, Response as CoreResponse, ServedBy, TerminalResult, Value, ValueError, Version,
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
}

/// Everything rendering needs to know about the cluster.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    /// The configuration.
    pub config: &'a Config,
}

/// The methods `/v1/kv` serves today.
const KV_METHODS: &str = "GET, PUT";

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
        ("DELETE", "/v1/kv") => Err(not_allowed(
            KV_METHODS,
            "DELETE is not served yet: it arrives with W27",
        )),
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
        (_, "/v1/cas") => Err(not_served("compare-and-swap arrives with W27")),
        (_, "/v1/scan") => Err(not_served("SCAN arrives with W28")),
        (_, path) if path.starts_with("/v1/requests/") => {
            Err(not_served("the status lookup is not served yet (F-9)"))
        }
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
    let request_id = match request.field("idempotency-key")[..] {
        [raw] => RequestId::parse(raw)
            .map_err(|why| invalid_write(None, &format!("Idempotency-Key {raw:?}: {why}")))?,
        [] => {
            return Err(invalid_write(
                None,
                "a write needs an Idempotency-Key header",
            ));
        }
        _ => return Err(invalid_write(None, "Idempotency-Key is repeated")),
    };
    let id = Some(request_id);
    let key = query_key(request, config).map_err(|why| invalid_write(id, &why))?;
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
            "a PUT's body needs Content-Type: application/json",
        ));
    }
    let body: PutBody = serde_json::from_slice(bytes)
        .map_err(|why| invalid_write(id, &format!("the body is not a valid PutRequest: {why}")))?;
    let value = Value::parse(body.value, config.key_value_limits()).map_err(|why| match why {
        ValueError::TooLong { .. } => too_large(id, &format!("the value is too large: {why}")),
    })?;
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
        (
            Kind::Put { request_id, .. },
            CoreResponse::Written {
                result,
                replayed,
                served_by,
            },
        ) => written(*request_id, result, replayed, served_by),
        (Kind::Put { request_id, .. }, CoreResponse::Conflict { original_index }) => error(
            409,
            "request_id_conflict",
            "the request ID was already used for a different command; nothing changed",
            details([
                ("request_id", json!(request_id.to_string())),
                ("original_index", json!(original_index.get())),
            ]),
        ),
        (Kind::Put { request_id, key }, CoreResponse::Refused(refusal)) => {
            write_refusal(*request_id, key, refusal, view)
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
        (Kind::Get { .. }, CoreResponse::Refused(refusal)) => read_refusal(refusal, view),
        (
            kind,
            response @ (CoreResponse::Written { .. }
            | CoreResponse::Conflict { .. }
            | CoreResponse::Found { .. }
            | CoreResponse::NotFound { .. }
            | CoreResponse::Page(_)
            | CoreResponse::Status { .. }),
        ) => {
            types::invariant!(
                false,
                "API-ANSWER-KIND",
                "the core answered a {kind:?} with {response:?}"
            );
            unavailable_read(view)
        }
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
fn write_refusal(request_id: RequestId, key: &Key, refusal: Refusal, view: View<'_>) -> Response {
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
                    "http://{}/v1/kv?key={}",
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
    match &attempt.kind {
        Kind::Put { request_id, .. } => busy_write(*request_id),
        Kind::Get { .. } => error(
            429,
            "busy",
            "a local admission bound was hit; back off and retry",
            Map::new(),
        ),
    }
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
    match &attempt.kind {
        Kind::Put { request_id, .. } => unknown_outcome(*request_id, view),
        Kind::Get { .. } => unavailable_read(view),
    }
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

fn not_served(message: &str) -> Response {
    error(404, "invalid_request", message, Map::new())
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
    fn endpoints_not_served_yet_are_refused_never_forwarded() {
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
                shape(routed("DELETE", "/v1/kv?key=a")),
                shape(routed("POST", "/v1/kv?key=a")),
                shape(routed("POST", "/v1/health/live")),
                shape(routed("POST", "/v1/cas?key=a")),
                shape(routed("GET", "/v1/scan")),
                shape(routed("GET", &format!("/v1/requests/{ID}"))),
                shape(routed("GET", "/v2/kv")),
                shape(routed("GET", "/v1/cluster?x=1")),
            ],
            [
                (405, Some("GET, PUT"), json!("invalid_request")),
                (405, Some("GET, PUT"), json!("invalid_request")),
                (405, Some("GET"), json!("invalid_request")),
                (404, None, json!("invalid_request")),
                (404, None, json!("invalid_request")),
                (404, None, json!("invalid_request")),
                (404, None, json!("invalid_request")),
                (400, None, json!("invalid_request")),
            ],
            "rule: DELETE, compare-and-swap, SCAN and the status lookup are refused until their \
             work items serve them (W27, W28, F-9), never handed to a core that cannot apply \
             them; other methods are 405 with Allow, unknown paths 404 (§8.4)"
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
}
