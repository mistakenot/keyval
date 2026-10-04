//! The model server's API, request by request, without sockets (W30.1).
#![cfg(test)]

// check: MODEL-SERVER

use serde_json::{Value as Json, json};

use super::Config;
use super::api::{State, handle};
use super::http::{Body, Request};
use crate::{HistoryLimits, Limits, Model};

const RID: &str = "0123456789abcdef0123456789abcdef";
const RID2: &str = "fedcba9876543210fedcba9876543210";

fn state_with(limits: Limits) -> State {
    state_under(limits, HistoryLimits::DEFAULT)
}

fn state_under(limits: Limits, history: HistoryLimits) -> State {
    State {
        model: Model::new(limits),
        config: Config::new(1, "keyval-model", limits, history).unwrap(),
        url: "http://127.0.0.1:7001".to_owned(),
    }
}

fn state() -> State {
    state_with(Limits::DEFAULT)
}

fn request(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
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
            .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_owned()))
            .collect(),
        body: Body::Bytes(body.to_vec()),
        close: false,
    }
}

fn send(state: &mut State, request: &Request) -> (u16, Json) {
    let response = handle(state, request);
    (response.status, response.body)
}

fn put(state: &mut State, rid: &str, key: &str, value: &str) -> (u16, Json) {
    let body = json!({"value": value}).to_string();
    send(
        state,
        &request(
            "PUT",
            &format!("/v1/kv?key={key}"),
            &[
                ("Idempotency-Key", rid),
                ("Content-Type", "application/json"),
            ],
            body.as_bytes(),
        ),
    )
}

fn cas(state: &mut State, rid: &str, key: &str, body: &str) -> (u16, Json) {
    send(
        state,
        &request(
            "POST",
            &format!("/v1/cas?key={key}"),
            &[
                ("Idempotency-Key", rid),
                ("Content-Type", "application/json"),
            ],
            body.as_bytes(),
        ),
    )
}

fn get(state: &mut State, target: &str) -> (u16, Json) {
    send(state, &request("GET", target, &[], b""))
}

fn served(applied: u64) -> Json {
    json!({"node_id": 1, "applied_index": applied})
}

fn code(body: &Json) -> Option<&str> {
    body.pointer("/error/code").and_then(Json::as_str)
}

#[test]
fn a_put_then_a_get_round_trip() {
    let mut state = state();
    let written = put(&mut state, RID, "a%2Fb%3Fc%26d", "v");
    assert_eq!(
        written,
        (
            200,
            json!({"request_id": RID, "index": 1, "version": 1, "replayed": false, "served_by": served(1)})
        ),
        "rule: a first PUT executes at index 1 with version == index (§8.2, §8.3)"
    );
    assert_eq!(
        get(&mut state, "/v1/kv?key=a%2Fb%3Fc%26d"),
        (
            200,
            json!({"key": "a/b?c&d", "value": "v", "version": 1, "served_by": served(1)})
        ),
        "rule: a GET returns the key, value, version and served_by (F-2); keys may hold / ? &"
    );
    assert_eq!(
        get(&mut state, "/v1/kv?key=absent"),
        (
            404,
            json!({"error": {"code": "not_found", "message": "no such key", "details": {"served_by": served(1)}}})
        ),
        "rule: an absent key is 404 not_found with served_by (§8.4)"
    );
}

#[test]
fn a_retry_replays_and_a_reused_id_conflicts_without_a_new_entry() {
    let mut state = state();
    put(&mut state, RID, "k", "v");
    let replay = put(&mut state, RID, "k", "v");
    let conflict = put(&mut state, RID, "k", "other");
    assert_eq!(
        (replay, conflict, state.model.applied_index()),
        (
            (
                200,
                json!({"request_id": RID, "index": 1, "version": 1, "replayed": true, "served_by": served(1)})
            ),
            (
                409,
                json!({"error": {"code": "request_id_conflict",
                                   "message": "the request ID was already used for a different command",
                                   "details": {"request_id": RID, "original_index": 1}}})
            ),
            1
        ),
        "rule: a same-ID retry replays and a different command conflicts, from the history, \
         with no new log entry (F-6, F-7 short circuit)"
    );
}

#[test]
fn delete_reports_whether_the_key_existed() {
    let mut state = state();
    put(&mut state, RID, "k", "v");
    let delete = |state: &mut State, rid: &str| {
        send(
            state,
            &request("DELETE", "/v1/kv?key=k", &[("Idempotency-Key", rid)], b""),
        )
    };
    let first = delete(&mut state, RID2);
    let again = delete(&mut state, "11111111111111111111111111111111");
    assert_eq!(
        (first, again, get(&mut state, "/v1/kv?key=k").0),
        (
            (
                200,
                json!({"request_id": RID2, "index": 2, "existed": true, "replayed": false, "served_by": served(2)})
            ),
            (
                200,
                json!({"request_id": "11111111111111111111111111111111", "index": 3, "existed": false,
                         "replayed": false, "served_by": served(3)})
            ),
            404
        ),
        "rule: DELETE reports existed and no version (F-3, §8.3)"
    );
}

#[test]
fn compare_and_swap_matches_mismatches_and_replays_its_mismatch() {
    let mut state = state();
    let created = cas(
        &mut state,
        RID,
        "k",
        r#"{"expected":{"kind":"absent"},"value":"1"}"#,
    );
    let mismatch = cas(
        &mut state,
        RID2,
        "k",
        r#"{"expected":{"kind":"version","version":7},"value":"2"}"#,
    );
    let replayed = cas(
        &mut state,
        RID2,
        "k",
        r#"{"expected":{"kind":"version","version":7},"value":"2"}"#,
    );
    let matched = cas(
        &mut state,
        "22222222222222222222222222222222",
        "k",
        r#"{"expected":{"kind":"version","version":1},"value":"3"}"#,
    );
    let mismatch_details = |replayed: bool| {
        json!({"error": {"code": "version_mismatch", "message": "the compare-and-swap expectation did not match",
                         "details": {"current_version": 1, "request_id": RID2, "index": 2, "replayed": replayed}}})
    };
    assert_eq!(
        (
            created,
            mismatch,
            replayed,
            matched.0,
            matched.1["version"].clone()
        ),
        (
            (
                200,
                json!({"request_id": RID, "index": 1, "version": 1, "replayed": false, "served_by": served(1)})
            ),
            (409, mismatch_details(false)),
            (409, mismatch_details(true)),
            200,
            json!(3)
        ),
        "rule: CAS on an absent key creates it, a wrong version is a terminal 409 \
         version_mismatch that replays as such, a matching one executes (F-5, §8.4)"
    );
}

#[test]
fn a_quota_refusal_is_a_terminal_507_that_replays() {
    let mut state = state_with(Limits::new(256, 65_536, 1, 1_000).unwrap());
    put(&mut state, RID, "a", "v");
    let refused = put(&mut state, RID2, "b", "v");
    let again = put(&mut state, RID2, "b", "v");
    let details = |replayed: bool| {
        json!({"error": {"code": "capacity_exceeded", "message": "the live-state quota refused the write",
                         "details": {"request_id": RID2, "limit": "max_live_keys", "index": 2, "replayed": replayed}}})
    };
    assert_eq!(
        (refused, again),
        ((507, details(false)), (507, details(true))),
        "rule: a DM-6 refusal is a committed 507 capacity_exceeded naming its limit, replayed \
         unchanged (§8.4, DM-6)"
    );
}

#[test]
fn history_full_refuses_every_new_write_at_max_log_entries_and_submits_nothing() {
    const RID3: &str = "33333333333333333333333333333333";
    let mut state = state_under(Limits::DEFAULT, HistoryLimits::new(2).unwrap());
    let below = (
        put(&mut state, RID, "a", "1").0,
        put(&mut state, RID2, "b", "2").0,
    );
    let refused = json!({"error": {"code": "history_full",
        "message": "the history capacity refused admission; the write was not submitted",
        "details": {"request_id": RID3, "limit": "max_log_entries", "attempt_state": "not_submitted"}}});
    let delete = request("DELETE", "/v1/kv?key=a", &[("Idempotency-Key", RID3)], b"");
    assert_eq!(
        (
            below,
            put(&mut state, RID3, "c", "3"),
            cas(
                &mut state,
                RID3,
                "a",
                r#"{"expected":{"kind":"version","version":1},"value":"x"}"#
            ),
            send(&mut state, &delete),
        ),
        (
            (200, 200),
            (507, refused.clone()),
            (507, refused.clone()),
            (507, refused)
        ),
        "rule: below max_log_entries a write is admitted; at it, PUT, CAS and DELETE get 507 \
         history_full, not submitted, naming the limit (DM-7, §8.4)"
    );
    assert_eq!(
        (
            state.model.applied_index(),
            get(&mut state, &format!("/v1/requests/{RID3}")),
            get(&mut state, "/v1/kv?key=a").0
        ),
        (
            2,
            (200, json!({"state": "unknown", "served_by": served(2)})),
            200
        ),
        "rule: a refusal before proposal appends no entry and leaves no history (DM-7, F-9)"
    );
}

#[test]
fn at_the_history_ceiling_known_ids_reads_and_validation_answer_first() {
    let mut state = state_under(Limits::DEFAULT, HistoryLimits::new(1).unwrap());
    let first = put(&mut state, RID, "a", "1");
    let replay = put(&mut state, RID, "a", "1");
    let conflict = put(&mut state, RID, "a", "2");
    let invalid = send(
        &mut state,
        &json_put("/v1/kv?key=", RID2, br#"{"value":"v"}"#),
    );
    assert_eq!(
        (
            first.0,
            replay.1["replayed"].clone(),
            replay.0,
            conflict.0,
            code(&conflict.1),
            invalid.0,
            code(&invalid.1)
        ),
        (
            200,
            json!(true),
            200,
            409,
            Some("request_id_conflict"),
            400,
            Some("invalid_request")
        ),
        "rule: at the ceiling a known request ID is still answered from the history and an \
         invalid write is still 400: both precede history admission (F-7, DM-7)"
    );
    assert_eq!(
        (
            get(&mut state, "/v1/kv?key=a").0,
            get(&mut state, "/v1/scan").0,
            get(&mut state, &format!("/v1/requests/{RID}")).1["state"].clone(),
            state.model.applied_index()
        ),
        (200, 200, json!("known"), 1),
        "rule: reads, SCAN and status lookups keep working at the ceiling, and nothing was \
         appended (DM-7)"
    );
}

#[test]
fn status_lookups_answer_known_and_unknown_requests() {
    let mut state = state();
    put(&mut state, RID, "k", "v");
    cas(
        &mut state,
        RID2,
        "k",
        r#"{"expected":{"kind":"absent"},"value":"x"}"#,
    );
    let known = get(&mut state, &format!("/v1/requests/{RID}"));
    let mismatch = get(&mut state, &format!("/v1/requests/{RID2}"));
    let unknown = get(&mut state, "/v1/requests/33333333333333333333333333333333");
    assert_eq!(
        (
            known,
            mismatch.0,
            code(&mismatch.1["result"]),
            mismatch.1["result"]["error"]["details"]["replayed"].clone(),
            unknown
        ),
        (
            (
                200,
                json!({"state": "known", "served_by": served(2),
                         "result": {"request_id": RID, "index": 1, "version": 1, "replayed": true, "served_by": served(2)}})
            ),
            200,
            Some("version_mismatch"),
            json!(true),
            (200, json!({"state": "unknown", "served_by": served(2)}))
        ),
        "rule: a status lookup returns the stored terminal result as a replay, or unknown, as \
         of served_by (F-9, §8.3)"
    );
    for bad in [
        "/v1/requests/XYZ",
        "/v1/requests/0123456789ABCDEF0123456789ABCDEF",
        "/v1/requests/abc",
    ] {
        let (status, body) = get(&mut state, bad);
        assert_eq!(
            (status, code(&body), body["error"]["details"].clone()),
            (400, Some("invalid_request"), json!({})),
            "rule: a malformed request ID is 400 invalid_request with empty details; path={bad}"
        );
    }
}

#[test]
fn scan_pages_and_refuses_bad_parameters() {
    let mut state = state();
    for (rid, key) in [
        (RID, "a"),
        (RID2, "b"),
        ("44444444444444444444444444444444", "c"),
    ] {
        put(&mut state, rid, key, "v");
    }
    let first = get(&mut state, "/v1/scan?start=a&limit=2");
    let next = get(&mut state, "/v1/scan?start=a&limit=2&after=b");
    assert_eq!(
        (first, next),
        (
            (
                200,
                json!({"items": [{"key": "a", "value": "v", "version": 1}, {"key": "b", "value": "v", "version": 2}],
                         "has_more": true, "next_after": "b", "served_by": served(3)})
            ),
            (
                200,
                json!({"items": [{"key": "c", "value": "v", "version": 3}], "has_more": false,
                         "next_after": null, "served_by": served(3)})
            )
        ),
        "rule: SCAN pages in key order with has_more and next_after (F-4, §8.5)"
    );
    for bad in [
        "limit=0",
        "limit=129",
        "limit=abc",
        "limit=-1",
        "limit=%2B1",
        "start=b&end=a",
        "start=a&start=b",
        "other=1",
        "start=a+b",
    ] {
        let (status, body) = get(&mut state, &format!("/v1/scan?{bad}"));
        assert_eq!(
            (status, code(&body), body["error"]["details"].clone()),
            (400, Some("invalid_request"), json!({})),
            "rule: bad range or pagination parameters are 400 invalid_request (§8.4); query={bad}"
        );
    }
}

/// Each request is refused with `status`, `code` and `details`, appending nothing.
fn assert_refused(cases: Vec<(Request, u16, &str, Json)>) {
    let mut state = state();
    for (request, status, expected_code, details) in cases {
        let (observed, body) = send(&mut state, &request);
        assert_eq!(
            (observed, code(&body), body["error"]["details"].clone()),
            (status, Some(expected_code), details),
            "rule: a refused write names its request_id once the Idempotency-Key is well-formed \
             (§8.1); request={request:?} body={body}"
        );
    }
    assert_eq!(
        state.model.applied_index(),
        0,
        "rule: a refused write appends nothing"
    );
}

fn json_put(target: &str, rid: &'static str, body: &[u8]) -> Request {
    request(
        "PUT",
        target,
        &[
            ("Idempotency-Key", rid),
            ("Content-Type", "application/json"),
        ],
        body,
    )
}

#[test]
fn a_bad_request_id_or_key_refuses_a_write() {
    let value = br#"{"value":"v"}"#;
    assert_refused(vec![
        (
            request(
                "PUT",
                "/v1/kv?key=k",
                &[("Content-Type", "application/json")],
                value,
            ),
            400,
            "invalid_request",
            json!({}),
        ),
        (
            json_put("/v1/kv?key=k", "ABC", value),
            400,
            "invalid_request",
            json!({}),
        ),
        (
            request(
                "PUT",
                "/v1/kv?key=k",
                &[
                    ("Idempotency-Key", RID),
                    ("Idempotency-Key", RID),
                    ("Content-Type", "application/json"),
                ],
                value,
            ),
            400,
            "invalid_request",
            json!({}),
        ),
        (
            json_put("/v1/kv", RID, value),
            400,
            "invalid_request",
            json!({"request_id": RID}),
        ),
        (
            json_put("/v1/kv?key=a+b", RID, value),
            400,
            "invalid_request",
            json!({"request_id": RID}),
        ),
        (
            json_put("/v1/kv?key=", RID, value),
            400,
            "invalid_request",
            json!({"request_id": RID}),
        ),
        (
            request("DELETE", "/v1/kv?key=k", &[("Idempotency-Key", RID)], b"x"),
            400,
            "invalid_request",
            json!({"request_id": RID}),
        ),
    ]);
}

#[test]
fn a_bad_media_type_or_body_refuses_a_write() {
    let with_rid = json!({"request_id": RID});
    assert_refused(vec![
        (
            request(
                "PUT",
                "/v1/kv?key=k",
                &[("Idempotency-Key", RID)],
                br#"{"value":"v"}"#,
            ),
            415,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            request(
                "PUT",
                "/v1/kv?key=k",
                &[("Idempotency-Key", RID), ("Content-Type", "text/plain")],
                br#"{"value":"v"}"#,
            ),
            415,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"{"value":"v","extra":1}"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"{"value":"v","value":"w"}"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, b"{\"value\":\"\xff\"}"),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"{"value":"\ud800"}"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"{"value":"v"} x"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"{"value":1}"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, br#"["v"]"#),
            400,
            "invalid_request",
            with_rid.clone(),
        ),
        (
            json_put("/v1/kv?key=k", RID, b" \"v\""),
            400,
            "invalid_request",
            with_rid,
        ),
    ]);
}

#[test]
fn bodies_and_values_over_their_limits_are_too_large() {
    let mut state = state_with(Limits::new(4, 3, 10, 1_000).unwrap());
    let headers = [
        ("Idempotency-Key", RID),
        ("Content-Type", "application/json"),
    ];
    let mut huge = request("PUT", "/v1/kv?key=k", &headers, b"");
    huge.body = Body::TooLarge;
    let cases = [
        (huge, 413, "too_large"),
        (
            request("PUT", "/v1/kv?key=k", &headers, br#"{"value":"abcd"}"#),
            413,
            "too_large",
        ),
        (
            request("PUT", "/v1/kv?key=abcde", &headers, br#"{"value":"a"}"#),
            400,
            "invalid_request",
        ),
    ];
    for (request, status, expected) in cases {
        let (observed, body) = send(&mut state, &request);
        assert_eq!(
            (
                observed,
                code(&body),
                body["error"]["details"]["request_id"].clone()
            ),
            (status, Some(expected), json!(RID)),
            "rule: a body over 512 KiB or a value over its limit is 413 too_large, a key over its \
             limit 400 (DM-1, DM-2, D-80); request={request:?}"
        );
    }
    assert_eq!(
        put(&mut state, RID, "abcd", "abc").0,
        200,
        "rule: a key and value exactly at their configured limits are accepted (D-80)"
    );
}

#[test]
fn keys_are_held_to_256_bytes_counted_after_decoding() {
    let mut state = state();
    let key = |length: usize| "%C3%A9".repeat(length / 2) + &"x".repeat(length % 2);
    let ids = [
        "55555555555555555555555555555555",
        "66666666666666666666666666666666",
    ];
    assert_eq!(
        (
            put(&mut state, ids[0], &key(256), "v").0,
            put(&mut state, ids[1], &key(257), "v").0
        ),
        (200, 400),
        "rule: a 256-byte key is accepted and a 257-byte one refused, counting decoded UTF-8 \
         bytes, not characters (DM-1)"
    );
}

#[test]
fn compare_and_swap_expectations_are_parsed_strictly() {
    let mut state = state();
    for body in [
        r#"{"expected":{"kind":"absent","version":1},"value":"v"}"#,
        r#"{"expected":{"kind":"version"},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":0},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":9007199254740992},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":1.0},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":-1},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":1,"version":2},"value":"v"}"#,
        r#"{"expected":{"kind":"later"},"value":"v"}"#,
        r#"{"value":"v"}"#,
        r#"{"expected":{"kind":"absent","version":null},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":null},"value":"v"}"#,
        r#"{"expected":{"kind":"version","version":"1"},"value":"v"}"#,
        r#"[{"kind":"absent"},"v"]"#,
        r#"{"expected":["absent"],"value":"v"}"#,
        r#"{"expected":["version",1],"value":"v"}"#,
    ] {
        let (status, response) = cas(&mut state, RID, "k", body);
        assert_eq!(
            (status, code(&response)),
            (400, Some("invalid_request")),
            "rule: a malformed expectation, or a version at or above 2^53, is 400 \
             invalid_request (§8.1); body={body}"
        );
    }
}

#[test]
fn routing_health_and_cluster_status() {
    let mut state = state();
    put(&mut state, RID, "k", "v");
    let missing = get(&mut state, "/v1/nothing");
    let wrong = handle(&mut state, &request("POST", "/v1/kv?key=k", &[], b""));
    assert_eq!(
        (
            missing.0,
            code(&missing.1),
            wrong.status,
            code(&wrong.body),
            wrong.allow
        ),
        (
            404,
            Some("invalid_request"),
            405,
            Some("invalid_request"),
            Some("GET, PUT, DELETE")
        ),
        "rule: an unknown path and an unsupported method keep the error envelope (openapi)"
    );
    assert_eq!(
        (
            get(&mut state, "/v1/health/live"),
            get(&mut state, "/v1/health/ready"),
            get(&mut state, "/v1/health/live?x=1").0
        ),
        (
            (
                200,
                json!({"status": "ok", "node_id": 1, "lifecycle": "ready"})
            ),
            (
                200,
                json!({"status": "ok", "node_id": 1, "lifecycle": "ready"})
            ),
            200
        ),
        "rule: the model server is always ready (O-1)"
    );
    assert_eq!(
        get(&mut state, "/v1/cluster"),
        (
            200,
            json!({"node_id": 1, "cluster_id": "keyval-model", "role": "leader", "term": 1, "leader_id": 1,
                     "durable_last_index": 1, "commit_index": 1, "applied_index": 1,
                     "peers": [{"id": 1, "client_address": "http://127.0.0.1:7001", "match_index": 1}]})
        ),
        "rule: the single node reports itself as the leader of term 1, every index at applied (O-1)"
    );
}
