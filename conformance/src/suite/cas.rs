//! Compare-and-swap (F-5, F-6): absent and version expectations, and a mismatch as a
//! committed terminal result that consumes its request ID.

#![cfg(test)]

use serde_json::json;

use super::support::{assert_keys, each_target, error, fresh_id, fresh_prefix, index_of, ok};
use crate::tag::{Class, Tag};

const CREATE: Tag = Tag {
    test: "cas_absent_creates_and_version_swaps",
    class: Class::Semantic,
    operations: &["compareAndSwap", "getKey", "deleteKey"],
    error_codes: &[],
};

// api-test: compareAndSwap
// api-test: getKey
// api-test: deleteKey
#[test]
fn cas_absent_creates_and_version_swaps() {
    each_target(&CREATE, |node| {
        let key = format!("{}k", fresh_prefix());
        let absent = json!({"kind": "absent"});
        let created = ok(
            &node.cas(&key, &absent, "one", &fresh_id()),
            "CAS on an absent key",
        );
        assert_keys(
            &created,
            &["index", "replayed", "request_id", "served_by", "version"],
            "a successful CAS has the shape of a PUT result",
        );
        assert_eq!(
            created["version"], created["index"],
            "rule: version == execution index (DM-3)"
        );
        let version = json!({"kind": "version", "version": created["version"]});
        let swapped = ok(
            &node.cas(&key, &version, "two", &fresh_id()),
            "CAS at the current version",
        );
        assert!(
            index_of(&swapped) > index_of(&created),
            "rule: a swap takes a new index"
        );
        let got = ok(
            &node.read_after(&key, index_of(&swapped)),
            "GET after the swap",
        );
        assert_eq!(got["value"], "two", "rule: the swap applied; body={got}");
        assert_eq!(
            got["version"], swapped["version"],
            "rule: with the new version"
        );
        // After a delete the key is absent again, and the old version no longer matches.
        let deleted = ok(&node.delete(&key, &fresh_id()), "DELETE");
        ok(
            &node.cas(&key, &absent, "three", &fresh_id()),
            "CAS absent after DELETE",
        );
        let stale = node.cas(
            &key,
            &json!({"kind": "version", "version": swapped["version"]}),
            "x",
            &fresh_id(),
        );
        let details = error(
            &stale,
            409,
            "version_mismatch",
            "CAS at a version from before the delete",
        );
        assert!(
            details["current_version"].as_u64() > Some(index_of(&deleted).0),
            "rule: the current version is the recreation's; details={details}"
        );
    });
}

const MISMATCH: Tag = Tag {
    test: "cas_mismatch_is_a_terminal_result_that_consumes_the_id",
    class: Class::Semantic,
    operations: &["compareAndSwap", "getKey"],
    error_codes: &["version_mismatch"],
};

// api-test: compareAndSwap
// api-test: getKey
// api-test: version_mismatch
#[test]
pub(super) fn cas_mismatch_is_a_terminal_result_that_consumes_the_id() {
    each_target(&MISMATCH, |node| {
        let key = format!("{}k", fresh_prefix());
        let put = ok(&node.put(&key, "one", &fresh_id()), "PUT");
        let wrong = json!({"kind": "version", "version": index_of(&put).0 + 1000});
        let id = fresh_id();
        let first = node.cas(&key, &wrong, "never", &id);
        let details = error(&first, 409, "version_mismatch", "CAS at the wrong version");
        assert_keys(
            &details,
            &["current_version", "index", "replayed", "request_id"],
            "a version_mismatch carries current_version, index, replayed and request_id",
        );
        assert_eq!(
            details["current_version"], put["version"],
            "rule: the current version is named"
        );
        assert_eq!(
            details["replayed"], false,
            "rule: the first answer is not a replay"
        );
        assert_eq!(details["request_id"], id, "rule: the request ID is echoed");
        assert!(
            details["index"].as_u64() > Some(index_of(&put).0),
            "rule: a mismatch is committed and takes an index; details={details}"
        );
        // Make the expectation true; the same ID must still replay the mismatch, not re-run.
        let right = json!({"kind": "version", "version": put["version"]});
        let retry = node.cas(&key, &wrong, "never", &id);
        let again = error(
            &retry,
            409,
            "version_mismatch",
            "retry of the mismatched CAS",
        );
        assert_eq!(
            again["replayed"], true,
            "rule: the retry is a replay; details={again}"
        );
        assert_eq!(
            again["index"], details["index"],
            "rule: the original terminal result"
        );
        // A matching expectation under a new ID succeeds: the mismatch changed nothing.
        let fixed = ok(
            &node.cas(&key, &right, "two", &fresh_id()),
            "CAS under a new ID",
        );
        let got = ok(&node.read_after(&key, index_of(&fixed)), "GET");
        assert_eq!(
            got["value"], "two",
            "rule: only the matching CAS applied; body={got}"
        );
        // An absent expectation on a present key mismatches and names its version.
        let present = node.cas(&key, &json!({"kind": "absent"}), "x", &fresh_id());
        let details = error(
            &present,
            409,
            "version_mismatch",
            "CAS absent on a present key",
        );
        assert_eq!(
            details["current_version"], fixed["version"],
            "rule: the live version is named"
        );
    });
}

const ABSENT_MISMATCH: Tag = Tag {
    test: "cas_version_on_an_absent_key_mismatches_with_no_current_version",
    class: Class::Semantic,
    operations: &["compareAndSwap", "getKey"],
    error_codes: &["version_mismatch", "not_found"],
};

// api-test: compareAndSwap
// api-test: getKey
// api-test: version_mismatch
// api-test: not_found
#[test]
fn cas_version_on_an_absent_key_mismatches_with_no_current_version() {
    each_target(&ABSENT_MISMATCH, |node| {
        let key = format!("{}k", fresh_prefix());
        let response = node.cas(
            &key,
            &json!({"kind": "version", "version": 5}),
            "x",
            &fresh_id(),
        );
        let details = error(
            &response,
            409,
            "version_mismatch",
            "CAS version on an absent key",
        );
        assert!(
            details["current_version"].is_null(),
            "rule: an absent key has no current version; details={details}"
        );
        error(
            &node.read_after(&key, index_of(&details)),
            404,
            "not_found",
            "a mismatched CAS creates nothing",
        );
    });
}
