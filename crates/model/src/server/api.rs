//! The API over the reference model (PLAN.md §8, api/openapi.yaml): every operation a
//! single node can show, with the contract's strict parsing. Single-node, always ready, the
//! leader of term 1: it never answers `not_leader`, `busy`, `unavailable`, `recovering` or
//! `unknown_outcome` (topology and lifecycle behaviour is out of scope, §10.14). Of DM-7's
//! history capacity it enforces `max_log_entries`; `max_history_bytes` waits for the model's
//! byte accounting (br-q37e).
//!
//! A write is admitted in the order §8.1 implies: the `Idempotency-Key` (a validation error
//! carries `request_id` only once it is well-formed), the key, the media type, the body's
//! size, strict JSON, the value's size. A request ID the history already holds is answered
//! from it with no new entry (F-7's short circuit). Any other write must pass history
//! admission (`507 history_full`, not submitted, DM-7), and then is the next log entry.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};

use super::Config;
use super::http::{Body, Request, Response};
use super::query::{self, QueryError};
use crate::{
    Applied, Command, DataError, Decision, Entry, Expected, Index, Key, Model, Op, RequestId,
    ScanRequest, TerminalResult, Value, Version,
};

/// The server's state: the model, its configuration and its own client URL.
#[derive(Debug)]
pub struct State {
    /// The reference model every request reads or writes.
    pub model: Model,
    /// The node's configuration.
    pub config: Config,
    /// This node's client URL, e.g. `http://127.0.0.1:7001` (the cluster status's peer).
    pub url: String,
}

/// A refused request: its status, code, message and `details`.
struct Refusal {
    status: u16,
    code: &'static str,
    message: String,
    details: Map<String, Json>,
}

impl Refusal {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "invalid_request",
            message: message.into(),
            details: Map::new(),
        }
    }

    /// The same refusal, naming the write's request ID (§8.1: once it is well-formed).
    fn for_request(mut self, request_id: &str) -> Self {
        self.details
            .insert("request_id".to_owned(), json!(request_id));
        self
    }

    fn into_response(self) -> Response {
        error(
            self.status,
            self.code,
            &self.message,
            &Json::Object(self.details),
        )
    }
}

/// The error envelope (§8.1): `{"error": {"code", "message", "details"}}`.
fn error(status: u16, code: &str, message: &str, details: &Json) -> Response {
    Response::new(
        status,
        json!({"error": {"code": code, "message": message, "details": details}}),
    )
}

/// Answers one request.
pub fn handle(state: &mut State, request: &Request) -> Response {
    let route = match request.path.as_str() {
        "/v1/kv" => Some(("GET, PUT, DELETE", ["GET", "PUT", "DELETE"].as_slice())),
        "/v1/cas" => Some(("POST", ["POST"].as_slice())),
        "/v1/scan" | "/v1/health/live" | "/v1/health/ready" | "/v1/cluster" => {
            Some(("GET", ["GET"].as_slice()))
        }
        path if status_id(path).is_some() => Some(("GET", ["GET"].as_slice())),
        _ => None,
    };
    let Some((allow, methods)) = route else {
        return error(404, "invalid_request", "no such endpoint", &json!({}));
    };
    if !methods.contains(&request.method.as_str()) {
        let mut response = error(
            405,
            "invalid_request",
            "method not allowed here",
            &json!({}),
        );
        response.allow = Some(allow);
        return response;
    }
    let answered = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/kv") => get(state, request),
        ("PUT" | "DELETE", "/v1/kv") | ("POST", "/v1/cas") => write(state, request),
        ("GET", "/v1/scan") => scan(state, request),
        // The probes and the cluster status answer whatever query or body comes with them:
        // the spec lists only 200 (200 or 503 for readiness) for them.
        ("GET", "/v1/health/live" | "/v1/health/ready") => Ok(health(state)),
        ("GET", "/v1/cluster") => Ok(cluster(state)),
        (_, path) => status(state, request, status_id(path).unwrap_or_default()),
    };
    answered.unwrap_or_else(Refusal::into_response)
}

/// The request ID segment of `/v1/requests/{request_id}`.
fn status_id(path: &str) -> Option<&str> {
    path.strip_prefix("/v1/requests/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

fn served_by(state: &State) -> Json {
    json!({"node_id": state.config.node_id, "applied_index": state.model.applied_index()})
}

fn parameters(request: &Request, allowed: &[&str]) -> Result<BTreeMap<String, String>, Refusal> {
    query::parse(request.query.as_deref(), allowed)
        .map_err(|error: QueryError| Refusal::invalid(error.to_string()))
}

/// A key from the query, held to DM-1 and the configured key limit (D-80).
fn key(state: &State, text: &str) -> Result<Key, Refusal> {
    let key = Key::new(text).map_err(|error| Refusal::invalid(error.to_string()))?;
    state
        .config
        .limits
        .check_key(&key)
        .map_err(|error| Refusal::invalid(error.to_string()))?;
    Ok(key)
}

/// A read takes no body.
fn no_body(request: &Request) -> Result<(), Refusal> {
    match &request.body {
        Body::Bytes(bytes) if bytes.is_empty() => Ok(()),
        Body::Bytes(_) | Body::TooLarge => Err(Refusal::invalid("this operation takes no body")),
    }
}

fn get(state: &State, request: &Request) -> Result<Response, Refusal> {
    no_body(request)?;
    let parameters = parameters(request, &["key"])?;
    let text = parameters
        .get("key")
        .ok_or_else(|| Refusal::invalid("the key parameter is required"))?;
    let key = key(state, text)?;
    let found = state
        .model
        .get(&key)
        .map_err(|error| Refusal::invalid(error.to_string()))?;
    Ok(match found {
        Some((value, version)) => Response::new(
            200,
            json!({"key": key.as_str(), "value": value.as_str(), "version": version.get(),
                   "served_by": served_by(state)}),
        ),
        None => error(
            404,
            "not_found",
            "no such key",
            &json!({"served_by": served_by(state)}),
        ),
    })
}

/// A PUT body (closed: unknown and duplicate fields are refused, §8.1).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    value: String,
}

/// A compare-and-swap body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CasBody {
    /// Only a JSON object: a derived struct would also take an array, positionally.
    #[serde(deserialize_with = "object")]
    expected: ExpectedBody,
    value: String,
}

/// Deserialises an [`ExpectedBody`] from a JSON object only, keeping the derived checks for
/// unknown and duplicate members (§8.1: `CasExpectation` is a closed object).
fn object<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<ExpectedBody, D::Error> {
    struct ObjectOnly;

    impl<'de> serde::de::Visitor<'de> for ObjectOnly {
        type Value = ExpectedBody;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an expectation object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<ExpectedBody, A::Error> {
            ExpectedBody::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectOnly)
}

/// A compare-and-swap expectation, checked after parsing so that every shape error is
/// `invalid_request`: `{"kind": "absent"}` or `{"kind": "version", "version": N}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedBody {
    kind: String,
    /// The `version` member if present, whatever its value: `null` is present too, so an
    /// absent expectation carrying `"version": null` is refused like any other member.
    #[serde(default, deserialize_with = "present")]
    version: Option<Json>,
}

/// Deserialises a member that is present, keeping `null` as `Some(Json::Null)`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Json>, D::Error> {
    Json::deserialize(deserializer).map(Some)
}

fn expected(body: &ExpectedBody) -> Result<Expected, String> {
    match (body.kind.as_str(), &body.version) {
        ("absent", None) => Ok(Expected::Absent),
        ("version", Some(version)) => version
            .as_u64()
            .and_then(|number| Version::new(number).ok())
            .map(Expected::Version)
            .ok_or_else(|| {
                format!("expected.version {version} is not an integer between 1 and 2^53 - 1")
            }),
        ("absent", Some(_)) => Err("an absent expectation takes no version".to_owned()),
        ("version", None) => Err("a version expectation needs its version".to_owned()),
        (kind, _) => Err(format!("unknown expectation kind {kind:?}")),
    }
}

/// The write's `Idempotency-Key` (F-6): present once, 32 lowercase hex digits.
fn request_id(request: &Request) -> Result<(RequestId, String), Refusal> {
    match request.field("idempotency-key")[..] {
        [text] => RequestId::parse(text)
            .map(|id| (id, text.to_owned()))
            .map_err(|_| Refusal::invalid("Idempotency-Key must be 32 lowercase hex characters")),
        [] => Err(Refusal::invalid("a write needs an Idempotency-Key header")),
        _ => Err(Refusal::invalid("Idempotency-Key is repeated")),
    }
}

/// The JSON body of a PUT or compare-and-swap, after the media type and size checks.
fn json_body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let media_type = request.field("content-type");
    let is_json = matches!(media_type[..], [value] if value
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json")));
    if !is_json {
        return Err(Refusal {
            status: 415,
            ..Refusal::invalid("a request body must be Content-Type: application/json")
        });
    }
    let bytes = match &request.body {
        Body::Bytes(bytes) => bytes,
        Body::TooLarge => {
            return Err(Refusal {
                status: 413,
                code: "too_large",
                ..Refusal::invalid("the request body exceeds 512 KiB")
            });
        }
    };
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(Refusal::invalid("a request body is a JSON object"));
    }
    serde_json::from_slice(bytes)
        .map_err(|error| Refusal::invalid(format!("bad JSON body: {error}")))
}

/// A value from a body, held to DM-2 and the configured value limit (D-80): `413`.
fn value(state: &State, text: String) -> Result<Value, Refusal> {
    let too_large = |error: DataError| Refusal {
        status: 413,
        code: "too_large",
        ..Refusal::invalid(error.to_string())
    };
    let value = Value::new(text).map_err(too_large)?;
    state.config.limits.check_value(&value).map_err(too_large)?;
    Ok(value)
}

/// The operation a write request asks for.
fn operation(state: &State, request: &Request, key: Key) -> Result<Op, Refusal> {
    match request.method.as_str() {
        "PUT" => {
            let body: PutBody = json_body(request)?;
            Ok(Op::Put {
                key,
                value: value(state, body.value)?,
            })
        }
        "POST" => {
            let body: CasBody = json_body(request)?;
            let expected = expected(&body.expected).map_err(Refusal::invalid)?;
            Ok(Op::Cas {
                key,
                expected,
                value: value(state, body.value)?,
            })
        }
        _ => {
            no_body(request)?;
            Ok(Op::Delete { key })
        }
    }
}

fn write(state: &mut State, request: &Request) -> Result<Response, Refusal> {
    let (id, text) = request_id(request)?;
    let admitted = || -> Result<Command, Refusal> {
        let parameters = parameters(request, &["key"])?;
        let key_text = parameters
            .get("key")
            .ok_or_else(|| Refusal::invalid("the key parameter is required"))?;
        let key = key(state, key_text)?;
        Ok(Command {
            request_id: id,
            op: operation(state, request, key)?,
        })
    };
    let command = admitted().map_err(|refusal| refusal.for_request(&text))?;
    // F-7's order: a known request ID is answered from the history before history
    // admission is asked, and only an admitted command becomes the next entry.
    let applied = state
        .model
        .classify(&command)
        .map_or_else(|| admit(state).and_then(|()| append(state, command)), Ok)
        .map_err(|refusal| refusal.for_request(&text))?;
    Ok(written(state, &text, applied))
}

/// DM-7's history admission for a new write: `507 history_full`, not submitted, once the
/// log is at its ceiling (§8.4).
fn admit(state: &State) -> Result<(), Refusal> {
    state
        .model
        .admit(state.config.history())
        .map_err(|limit| Refusal {
            status: 507,
            code: "history_full",
            message: "the history capacity refused admission; the write was not submitted"
                .to_owned(),
            details: Map::from_iter([
                ("limit".to_owned(), json!(limit.as_str())),
                ("attempt_state".to_owned(), json!("not_submitted")),
            ]),
        })
}

/// Applies a first-seen command as the next log entry.
fn append(state: &mut State, command: Command) -> Result<Applied, Refusal> {
    let next = state.model.applied_index() + 1;
    let index = Index::new(next).map_err(|error| Refusal::invalid(error.to_string()))?;
    state
        .model
        .apply(index, &Entry::Command(command))
        .map_err(|error| Refusal::invalid(error.to_string()))
}

/// The response to a write's result (§8.2, §8.4).
fn written(state: &State, request_id: &str, applied: Applied) -> Response {
    match applied {
        Applied::Executed(result) => terminal(state, request_id, result, false),
        Applied::Replayed(result) => terminal(state, request_id, result, true),
        Applied::Conflict { original_index, .. } => error(
            409,
            "request_id_conflict",
            "the request ID was already used for a different command",
            &json!({"request_id": request_id, "original_index": original_index.get()}),
        ),
        // A command entry never applies as empty.
        Applied::Empty => error(
            400,
            "invalid_request",
            "the write did not execute",
            &json!({"request_id": request_id}),
        ),
    }
}

/// A terminal result as the API returns it: a success body, or the `version_mismatch` or
/// `capacity_exceeded` envelope (§8.1 shape 1).
fn terminal(state: &State, request_id: &str, result: TerminalResult, replayed: bool) -> Response {
    let index = result.index.get();
    match result.decision {
        Decision::Set { version } => Response::new(
            200,
            json!({"request_id": request_id, "index": index, "version": version.get(),
                   "replayed": replayed, "served_by": served_by(state)}),
        ),
        Decision::Deleted { existed } => Response::new(
            200,
            json!({"request_id": request_id, "index": index, "existed": existed,
                   "replayed": replayed, "served_by": served_by(state)}),
        ),
        Decision::VersionMismatch { current } => error(
            409,
            "version_mismatch",
            "the compare-and-swap expectation did not match",
            &json!({"current_version": current.map(Version::get), "request_id": request_id,
                   "index": index, "replayed": replayed}),
        ),
        Decision::CapacityExceeded { limit } => error(
            507,
            "capacity_exceeded",
            "the live-state quota refused the write",
            &json!({"request_id": request_id, "limit": limit.as_str(), "index": index,
                   "replayed": replayed}),
        ),
    }
}

fn scan(state: &State, request: &Request) -> Result<Response, Refusal> {
    no_body(request)?;
    let parameters = parameters(request, &["start", "end", "after", "limit"])?;
    let bound = |name: &str| {
        parameters
            .get(name)
            .map(|text| key(state, text))
            .transpose()
    };
    let limit = parameters
        .get("limit")
        .map(|text| {
            if text.is_empty() || !text.bytes().all(|digit| digit.is_ascii_digit()) {
                return Err(Refusal::invalid(format!(
                    "limit {text:?} is not an integer"
                )));
            }
            text.parse::<u64>()
                .map_err(|_| Refusal::invalid(format!("limit {text:?} is out of range")))
        })
        .transpose()?;
    let scan = ScanRequest::new(bound("start")?, bound("end")?, bound("after")?, limit)
        .map_err(|error| Refusal::invalid(error.to_string()))?;
    let page = state
        .model
        .scan(&scan)
        .map_err(|error| Refusal::invalid(error.to_string()))?;
    let items: Vec<Json> = page
        .items
        .iter()
        .map(|item| json!({"key": item.key.as_str(), "value": item.value.as_str(), "version": item.version.get()}))
        .collect();
    Ok(Response::new(
        200,
        json!({"items": items, "has_more": page.has_more,
               "next_after": page.next_after.as_ref().map(Key::as_str),
               "served_by": served_by(state)}),
    ))
}

fn status(state: &State, request: &Request, id_text: &str) -> Result<Response, Refusal> {
    no_body(request)?;
    parameters(request, &[])?;
    let id = RequestId::parse(id_text)
        .map_err(|_| Refusal::invalid("a request ID is 32 lowercase hex characters"))?;
    Ok(state.model.status(&id).map_or_else(
        || {
            Response::new(
                200,
                json!({"state": "unknown", "served_by": served_by(state)}),
            )
        },
        |result| {
            let stored = terminal(state, id_text, result, true).body;
            Response::new(
                200,
                json!({"state": "known", "result": stored, "served_by": served_by(state)}),
            )
        },
    ))
}

fn health(state: &State) -> Response {
    Response::new(
        200,
        json!({"status": "ok", "node_id": state.config.node_id, "lifecycle": "ready"}),
    )
}

fn cluster(state: &State) -> Response {
    let applied = state.model.applied_index();
    Response::new(
        200,
        json!({"node_id": state.config.node_id, "cluster_id": state.config.cluster_id,
               "role": "leader", "term": 1, "leader_id": state.config.node_id,
               "durable_last_index": applied, "commit_index": applied, "applied_index": applied,
               "peers": [{"id": state.config.node_id, "client_address": state.url,
                          "match_index": applied}]}),
    )
}
