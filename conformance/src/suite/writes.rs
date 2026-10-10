//! PUT and DELETE: result shapes, versions, idempotent replay and request-ID reuse
//! (F-2, F-6, F-7; §8.2, §8.3).

#![cfg(test)]

use serde_json::Value;

use super::support::{
    assert_keys, each_target, enc, error, fresh_id, fresh_prefix, index_of, ok, say,
};
use crate::tag::{Class, Tag};

const PUT_SHAPE: Tag = Tag {
    test: "put_result_shape_and_versions_follow_the_log",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getKey
#[test]
pub(super) fn put_result_shape_and_versions_follow_the_log() {
    each_target(&PUT_SHAPE, |node| {
        let key = format!("{}k", fresh_prefix());
        let id = fresh_id();
        let first = ok(&node.put(&key, "one", &id), "first PUT");
        assert_keys(
            &first,
            &["index", "replayed", "request_id", "served_by", "version"],
            "a PUT result is index, replayed, request_id, served_by and version",
        );
        assert_keys(
            &first["served_by"],
            &["applied_index", "node_id"],
            "served_by names the node and its applied index",
        );
        assert_eq!(
            first["request_id"], id,
            "rule: the request ID is echoed; body={first}"
        );
        assert_eq!(
            first["replayed"], false,
            "rule: a first execution is not a replay"
        );
        assert_eq!(
            first["version"], first["index"],
            "rule: version == execution index (DM-3)"
        );
        assert!(
            first["served_by"]["applied_index"].as_u64() >= first["index"].as_u64(),
            "rule: a write is applied where it is answered; body={first}"
        );
        let same = ok(&node.put(&key, "one", &fresh_id()), "PUT of the same value");
        assert!(
            index_of(&same) > index_of(&first) && same["version"] == same["index"],
            "rule: an unchanged value still takes a new index and version (DM-3); body={same}"
        );
        let second = ok(&node.put(&key, "two", &fresh_id()), "second PUT");
        assert!(
            index_of(&second) > index_of(&first),
            "rule: log indices increase; first={first} second={second}"
        );
        assert_eq!(
            second["version"], second["index"],
            "rule: version == index on overwrite"
        );
        let got = ok(
            &node.read_after(&key, index_of(&second)),
            "GET after the overwrite",
        );
        assert_keys(
            &got,
            &["key", "served_by", "value", "version"],
            "a GET result is key, served_by, value and version",
        );
        assert_eq!(
            got["value"], "two",
            "rule: the latest write wins; body={got}"
        );
        assert_eq!(
            got["version"], second["version"],
            "rule: GET reports the entry's version"
        );
    });
}

const REPLAY: Tag = Tag {
    test: "retried_put_replays_the_original_result",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getKey
#[test]
pub(super) fn retried_put_replays_the_original_result() {
    each_target(&REPLAY, |node| {
        let key = format!("{}k", fresh_prefix());
        let id = fresh_id();
        let first = ok(&node.put(&key, "one", &id), "first PUT");
        // Later writes to the key must not change what the retry is told.
        let later = ok(&node.put(&key, "two", &fresh_id()), "intervening PUT");
        for attempt in 0..3 {
            let retry = ok(&node.put(&key, "one", &id), "retry of the first PUT");
            assert_eq!(
                retry["replayed"], true,
                "rule: a retry is a replay; attempt={attempt}"
            );
            for field in ["index", "version", "request_id"] {
                assert_eq!(
                    retry[field], first[field],
                    "rule: a replay returns the original {field} (F-6); attempt={attempt}"
                );
            }
        }
        let got = ok(
            &node.read_after(&key, index_of(&later)),
            "GET after the retries",
        );
        assert_eq!(
            got["value"], "two",
            "rule: a replay never re-applies the write; body={got}"
        );
        assert_eq!(
            got["version"], later["version"],
            "rule: and takes no new version"
        );
    });
}

const CONFLICT: Tag = Tag {
    test: "request_id_reused_for_another_command_conflicts",
    class: Class::Semantic,
    operations: &["putKey", "deleteKey", "compareAndSwap"],
    error_codes: &["request_id_conflict"],
};

// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: request_id_conflict
#[test]
fn request_id_reused_for_another_command_conflicts() {
    each_target(&CONFLICT, |node| {
        let prefix = fresh_prefix();
        let (key, other) = (format!("{prefix}k"), format!("{prefix}other"));
        let id = fresh_id();
        let first = ok(&node.put(&key, "one", &id), "first PUT");
        let expected = |response: &crate::http::Response, why: &str| {
            let details = error(response, 409, "request_id_conflict", why);
            assert_keys(&details, &["original_index", "request_id"], why);
            assert_eq!(
                details["request_id"], id,
                "rule: the conflicting ID is named; {why}"
            );
            assert_eq!(
                details["original_index"], first["index"],
                "rule: the original execution is named; {why}"
            );
        };
        expected(
            &node.put(&key, "different value", &id),
            "same key, different value",
        );
        expected(&node.put(&other, "one", &id), "same value, different key");
        expected(&node.delete(&key, &id), "a delete under a PUT's ID");
        expected(
            &node.cas(&key, &serde_json::json!({"kind": "absent"}), "one", &id),
            "a CAS under a PUT's ID",
        );
        // The conflict neither consumed nor disturbed the original.
        let retry = ok(
            &node.put(&key, "one", &id),
            "the original command still replays",
        );
        assert_eq!(
            retry["replayed"], true,
            "rule: the original ID still replays; body={retry}"
        );
        assert_eq!(
            retry["index"], first["index"],
            "rule: with its original result"
        );
        error(
            &node.read_after(&other, index_of(&first)),
            404,
            "not_found",
            "the conflict wrote nothing",
        );
    });
}

const ID_SHAPE: Tag = Tag {
    test: "idempotency_key_must_be_32_lowercase_hex",
    class: Class::Semantic,
    operations: &["putKey", "deleteKey", "compareAndSwap"],
    error_codes: &["invalid_request"],
};

// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: invalid_request
#[test]
fn idempotency_key_must_be_32_lowercase_hex() {
    each_target(&ID_SHAPE, |node| {
        let key = format!("{}k", fresh_prefix());
        let put = format!("/v1/kv?key={}", enc(&key));
        let cas = format!("/v1/cas?key={}", enc(&key));
        let value = r#"{"value":"v"}"#;
        let swap = r#"{"expected":{"kind":"absent"},"value":"v"}"#;
        let good = fresh_id();
        let upper = good
            .to_uppercase()
            .replace(|c: char| c.is_ascii_digit(), "A");
        let bad: [(&str, Option<String>); 6] = [
            ("a missing key", None),
            ("an empty key", Some(String::new())),
            ("31 characters", Some(good[1..].to_owned())),
            ("33 characters", Some(format!("{good}0"))),
            ("upper-case hex", Some(upper)),
            ("non-hex characters", Some(format!("{}g", &good[1..]))),
        ];
        for (why, id) in &bad {
            let id = id.as_deref();
            error(
                &node.write("PUT", &put, id, Some(value)),
                400,
                "invalid_request",
                &format!("PUT with {why}"),
            );
            error(
                &node.write("DELETE", &put, id, None),
                400,
                "invalid_request",
                &format!("DELETE with {why}"),
            );
            error(
                &node.write("POST", &cas, id, Some(swap)),
                400,
                "invalid_request",
                &format!("CAS with {why}"),
            );
        }
        error(
            &node.read(&key),
            404,
            "not_found",
            "no refused write stored anything",
        );
    });
}

const BODIES: Tag = Tag {
    test: "malformed_write_bodies_are_invalid_requests",
    class: Class::Semantic,
    operations: &["putKey", "compareAndSwap"],
    error_codes: &["invalid_request"],
};

// api-test: putKey
// api-test: compareAndSwap
// api-test: invalid_request
#[test]
fn malformed_write_bodies_are_invalid_requests() {
    each_target(&BODIES, |node| {
        let key = format!("{}k", fresh_prefix());
        let put = format!("/v1/kv?key={}", enc(&key));
        let cas = format!("/v1/cas?key={}", enc(&key));
        for (why, body) in [
            ("an empty body", ""),
            ("truncated JSON", "{"),
            ("a JSON array", "[]"),
            ("a missing value", "{}"),
            ("a numeric value", r#"{"value":5}"#),
            ("a null value", r#"{"value":null}"#),
            ("an unknown field", r#"{"value":"v","extra":1}"#),
            ("a duplicate field", r#"{"value":"a","value":"b"}"#),
        ] {
            let id = fresh_id();
            let response = node.write("PUT", &put, Some(&id), Some(body));
            let details = error(
                &response,
                400,
                "invalid_request",
                &format!("PUT with {why}"),
            );
            assert_eq!(
                details["request_id"], id,
                "rule: a well-formed rejected write names its request ID (§8); case={why}"
            );
        }
        for (why, body) in [
            ("a missing expectation", r#"{"value":"v"}"#),
            ("a missing value", r#"{"expected":{"kind":"absent"}}"#),
            (
                "an unknown expectation kind",
                r#"{"expected":{"kind":"maybe"},"value":"v"}"#,
            ),
            (
                "version zero",
                r#"{"expected":{"kind":"version","version":0},"value":"v"}"#,
            ),
            (
                "a negative version",
                r#"{"expected":{"kind":"version","version":-1},"value":"v"}"#,
            ),
            (
                "a version above 2^53 - 1",
                r#"{"expected":{"kind":"version","version":9007199254740992},"value":"v"}"#,
            ),
            (
                "a version without a number",
                r#"{"expected":{"kind":"version"},"value":"v"}"#,
            ),
            (
                "a version on an absent expectation",
                r#"{"expected":{"kind":"absent","version":1},"value":"v"}"#,
            ),
        ] {
            let response = node.write("POST", &cas, Some(&fresh_id()), Some(body));
            error(
                &response,
                400,
                "invalid_request",
                &format!("CAS with {why}"),
            );
        }
        error(
            &node.read(&key),
            404,
            "not_found",
            "no refused write stored anything",
        );
    });
}

const DELETE: Tag = Tag {
    test: "delete_reports_existence_and_replays",
    class: Class::Semantic,
    operations: &["putKey", "getKey", "deleteKey"],
    error_codes: &["not_found"],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
// api-test: not_found
#[test]
pub(super) fn delete_reports_existence_and_replays() {
    each_target(&DELETE, |node| {
        let key = format!("{}k", fresh_prefix());
        let missing = ok(&node.delete(&key, &fresh_id()), "DELETE of an absent key");
        assert_keys(
            &missing,
            &["existed", "index", "replayed", "request_id", "served_by"],
            "a DELETE result is existed, index, replayed, request_id and served_by",
        );
        assert_eq!(
            missing["existed"], false,
            "rule: deleting an absent key succeeds; body={missing}"
        );
        let put = ok(&node.put(&key, "v", &fresh_id()), "PUT");
        assert!(
            index_of(&put) > index_of(&missing),
            "rule: even a no-op delete takes an index"
        );
        let id = fresh_id();
        let deleted = ok(&node.delete(&key, &id), "DELETE of a present key");
        assert_eq!(deleted["existed"], true, "rule: it existed; body={deleted}");
        assert_eq!(deleted["replayed"], false, "rule: a first execution");
        let gone = node.read_after(&key, index_of(&deleted));
        error(&gone, 404, "not_found", "GET after DELETE");
        let retry = ok(&node.delete(&key, &id), "retry of the DELETE");
        assert_eq!(
            retry["replayed"], true,
            "rule: a retry is a replay; body={retry}"
        );
        assert_eq!(
            retry["existed"], true,
            "rule: a replay keeps the original existed (F-6)"
        );
        assert_eq!(
            retry["index"], deleted["index"],
            "rule: and the original index"
        );
        // Recreating the key starts a fresh version, never an old one.
        let again = ok(&node.put(&key, "w", &fresh_id()), "PUT after DELETE");
        assert_eq!(
            again["version"], again["index"],
            "rule: version == index after recreation"
        );
        assert!(
            index_of(&again) > index_of(&deleted),
            "rule: indices keep increasing"
        );
    });
}

const NOT_FOUND: Tag = Tag {
    test: "get_of_an_absent_key_is_not_found_with_served_by",
    class: Class::Semantic,
    operations: &["getKey"],
    error_codes: &["not_found"],
};

// api-test: getKey
// api-test: not_found
#[test]
fn get_of_an_absent_key_is_not_found_with_served_by() {
    each_target(&NOT_FOUND, |node| {
        let response = node.read(&format!("{}absent", fresh_prefix()));
        let details = error(&response, 404, "not_found", "GET of an absent key");
        assert_keys(&details, &["served_by"], "a not_found carries served_by");
        assert_keys(
            &details["served_by"],
            &["applied_index", "node_id"],
            "served_by names the node and its applied index",
        );
        let body: Value = response.json().unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "rule: an error message is non-empty; {}",
            say(&response)
        );
    });
}
