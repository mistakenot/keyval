//! The read-only request status lookup (F-7, §8.5): known results replay a write's terminal
//! outcome, unknown means no execution is recorded, and a lookup changes nothing.

#![cfg(test)]

use serde_json::json;

use super::support::{assert_keys, each_target, error, fresh_id, fresh_prefix, index_of, ok};
use crate::tag::{Class, Tag};

const KNOWN: Tag = Tag {
    test: "status_of_an_executed_write_is_known_and_replayed",
    class: Class::Semantic,
    operations: &["getRequestStatus", "putKey", "deleteKey", "compareAndSwap"],
    error_codes: &["version_mismatch"],
};

// api-test: getRequestStatus
// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: version_mismatch
#[test]
pub(super) fn status_of_an_executed_write_is_known_and_replayed() {
    each_target(&KNOWN, |node| {
        let key = format!("{}k", fresh_prefix());
        let (put_id, delete_id, cas_id) = (fresh_id(), fresh_id(), fresh_id());
        let put = ok(&node.put(&key, "v", &put_id), "PUT");
        let status = ok(&node.status(&put_id), "status of the PUT");
        assert_keys(
            &status,
            &["result", "served_by", "state"],
            "a known status is result, served_by and state",
        );
        assert_eq!(
            status["state"], "known",
            "rule: an executed write is known; body={status}"
        );
        let result = &status["result"];
        assert_eq!(
            result["replayed"], true,
            "rule: a status result reports a replay; {status}"
        );
        for field in ["index", "version", "request_id"] {
            assert_eq!(
                result[field], put[field],
                "rule: the status result keeps the PUT's {field}"
            );
        }
        let delete = ok(&node.delete(&key, &delete_id), "DELETE");
        let status = ok(&node.status(&delete_id), "status of the DELETE");
        assert_eq!(
            status["state"], "known",
            "rule: a DELETE is known; body={status}"
        );
        assert_eq!(
            status["result"]["existed"], true,
            "rule: with its original existed"
        );
        assert_eq!(
            status["result"]["index"], delete["index"],
            "rule: and its original index"
        );
        // A mismatched CAS is terminal too: its status is known, and carries the error shape.
        let wrong = json!({"kind": "version", "version": 1});
        let mismatch = node.cas(&key, &wrong, "x", &cas_id);
        let details = error(&mismatch, 409, "version_mismatch", "CAS on a deleted key");
        let status = ok(&node.status(&cas_id), "status of the mismatched CAS");
        assert_eq!(
            status["state"], "known",
            "rule: a committed mismatch is known; {status}"
        );
        let result = &status["result"]["error"];
        assert_eq!(
            result["code"], "version_mismatch",
            "rule: the terminal error is reported; {status}"
        );
        assert_eq!(
            result["details"]["index"], details["index"],
            "rule: with its original index"
        );
        assert_eq!(
            result["details"]["replayed"], true,
            "rule: as a replay; {status}"
        );
    });
}

const UNKNOWN: Tag = Tag {
    test: "status_of_an_unseen_id_is_unknown_and_changes_nothing",
    class: Class::Semantic,
    operations: &["getRequestStatus", "putKey", "getKey"],
    error_codes: &["invalid_request"],
};

// api-test: getRequestStatus
// api-test: putKey
// api-test: getKey
// api-test: invalid_request
#[test]
fn status_of_an_unseen_id_is_unknown_and_changes_nothing() {
    each_target(&UNKNOWN, |node| {
        let key = format!("{}k", fresh_prefix());
        let before = ok(&node.put(&key, "v", &fresh_id()), "PUT");
        let unseen = fresh_id();
        let status = ok(&node.status(&unseen), "status of an unseen ID");
        assert_keys(
            &status,
            &["served_by", "state"],
            "an unknown status is served_by and state only",
        );
        assert_eq!(
            status["state"], "unknown",
            "rule: an unseen ID is unknown; body={status}"
        );
        // Looking up an ID neither executes it nor reserves it: it is still unseen, and a
        // later write under it is a first execution.
        let again = ok(&node.status(&unseen), "second status of the unseen ID");
        assert_eq!(
            again["state"], "unknown",
            "rule: a lookup records nothing; body={again}"
        );
        let write = ok(&node.put(&key, "w", &unseen), "PUT under the looked-up ID");
        assert_eq!(
            write["replayed"], false,
            "rule: a lookup did not consume the ID; body={write}"
        );
        assert!(
            index_of(&write) > index_of(&before),
            "rule: the write took the next index"
        );
        for bad in [
            "",
            "xyz",
            "ABCDEF0123456789ABCDEF0123456789",
            "0123456789abcdef0123456789abcde",
        ] {
            let response = node.get(&format!("/v1/requests/{bad}"));
            assert!(
                response.status == 400 || response.status == 404,
                "rule: a malformed request ID is refused; id={bad:?} {}",
                super::support::say(&response)
            );
        }
        let malformed = node.get("/v1/requests/xyz");
        error(
            &malformed,
            400,
            "invalid_request",
            "a request ID is 32 lowercase hex characters",
        );
    });
}
