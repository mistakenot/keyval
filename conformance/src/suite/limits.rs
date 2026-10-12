//! Decoded size limits (DM-1, DM-2, §8.2) and the live-state quotas (DM-6).
//!
//! Limits are on decoded bytes, never on the escaped form: a key of 256 bytes is 768
//! characters once every byte is percent-escaped, and a 64 KiB value of control characters
//! is six times that as JSON, and both must be accepted.

#![cfg(test)]

use model::Limits;

use super::support::{
    assert_keys, each_target, error, fresh_id, fresh_prefix, index_of, ok, with_model,
};
use crate::poll::Index;
use crate::tag::{Class, Tag};

/// A key of exactly `bytes` UTF-8 bytes under `prefix`, padded with `filler`.
fn key_of(prefix: &str, bytes: usize, filler: char) -> String {
    let mut key = prefix.to_owned();
    while key.len() + filler.len_utf8() <= bytes {
        key.push(filler);
    }
    key
}

const KEY_LIMIT: Tag = Tag {
    test: "key_limit_counts_decoded_utf8_bytes",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &["invalid_request"],
};

// api-test: putKey
// api-test: getKey
// api-test: invalid_request
#[test]
fn key_limit_counts_decoded_utf8_bytes() {
    each_target(&KEY_LIMIT, |node| {
        let prefix = fresh_prefix();
        // 255 and 256 bytes are accepted, 257 is not.
        for bytes in [255, 256] {
            let key = key_of(&prefix, bytes, 'k');
            assert_eq!(key.len(), bytes, "rule: the fixture is {bytes} bytes");
            let put = ok(&node.put(&key, "v", &fresh_id()), "PUT at the key limit");
            let got = ok(
                &node.read_after(&key, index_of(&put)),
                "GET at the key limit",
            );
            assert_eq!(got["key"], key, "rule: a {bytes}-byte key round-trips");
        }
        let long = key_of(&prefix, 257, 'k');
        error(
            &node.put(&long, "v", &fresh_id()),
            400,
            "invalid_request",
            "PUT of a 257-byte key",
        );
        error(
            &node.read(&long),
            400,
            "invalid_request",
            "GET of a 257-byte key",
        );
        error(
            &node.delete(&long, &fresh_id()),
            400,
            "invalid_request",
            "DELETE of a 257-byte key",
        );
        error(
            &node.cas(
                &long,
                &serde_json::json!({"kind": "absent"}),
                "v",
                &fresh_id(),
            ),
            400,
            "invalid_request",
            "CAS of a 257-byte key",
        );
        // The limit is bytes, not characters: 2-byte characters reach it at half the count.
        let wide = key_of(&prefix, 256, 'é');
        assert!(
            wide.len() <= 256 && wide.len() >= 255,
            "rule: fixture is near 256 bytes"
        );
        ok(
            &node.put(&wide, "v", &fresh_id()),
            "PUT of a 2-byte-character key at the limit",
        );
        let over = format!("{wide}éé");
        assert!(
            over.len() > 256,
            "rule: the fixture exceeds 256 bytes; len={}",
            over.len()
        );
        assert!(
            over.chars().count() < 256,
            "rule: the fixture is under 256 characters, over 256 bytes"
        );
        error(
            &node.put(&over, "v", &fresh_id()),
            400,
            "invalid_request",
            "PUT over 256 bytes",
        );
    });
}

const VALUE_LIMIT: Tag = Tag {
    test: "value_limit_counts_decoded_utf8_bytes",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &["too_large"],
};

// api-test: putKey
// api-test: getKey
// api-test: too_large
#[test]
fn value_limit_counts_decoded_utf8_bytes() {
    each_target(&VALUE_LIMIT, |node| {
        let key = format!("{}v", fresh_prefix());
        for value in [
            "a".repeat(65_535),
            "a".repeat(65_536),
            "😀".repeat(16_384),
            String::new(),
        ] {
            let put = ok(
                &node.put(&key, &value, &fresh_id()),
                "PUT at or under the value limit",
            );
            let got = ok(
                &node.read_after(&key, index_of(&put)),
                "GET of a large value",
            );
            assert_eq!(
                got["value"].as_str().map(str::len),
                Some(value.len()),
                "rule: a {}-byte value round-trips",
                value.len()
            );
        }
        for value in ["a".repeat(65_537), "😀".repeat(16_385), "é".repeat(32_769)] {
            let id = fresh_id();
            let details = error(
                &node.put(&key, &value, &id),
                413,
                "too_large",
                "PUT over the value limit",
            );
            assert_eq!(
                details["request_id"],
                id,
                "rule: a well-formed rejected write names its request ID (§8); value bytes={}",
                value.len()
            );
        }
    });
}

const EXPANSION: Tag = Tag {
    test: "escaping_expansion_is_not_counted_against_limits",
    class: Class::Semantic,
    operations: &["putKey", "getKey", "deleteKey"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
#[test]
fn escaping_expansion_is_not_counted_against_limits() {
    each_target(&EXPANSION, |node| {
        let prefix = fresh_prefix();
        // Every byte of the key is percent-escaped: 768 characters of query for 256 bytes.
        let key = key_of(&prefix, 256, '%');
        let put = ok(
            &node.put(&key, "v", &fresh_id()),
            "PUT of a fully escaped 256-byte key",
        );
        let got = ok(
            &node.read_after(&key, index_of(&put)),
            "GET of a fully escaped key",
        );
        assert_eq!(got["key"], key, "rule: a percent-heavy key round-trips");
        // Control characters expand six-fold as JSON (\u0001); quotes and backslashes two-fold.
        for filler in ['\u{1}', '"', '\\', '\n'] {
            let value = filler.to_string().repeat(65_536);
            let put = ok(
                &node.put(&key, &value, &fresh_id()),
                "PUT of an escaping-heavy value",
            );
            let got = ok(
                &node.read_after(&key, index_of(&put)),
                "GET of an escaping-heavy value",
            );
            assert_eq!(
                got["value"].as_str(),
                Some(value.as_str()),
                "rule: a 64 KiB value of {filler:?} round-trips whatever it expands to"
            );
        }
        ok(&node.delete(&key, &fresh_id()), "DELETE of the escaped key");
    });
}

const BODY_LIMIT: Tag = Tag {
    test: "body_over_512_kib_is_too_large",
    class: Class::Semantic,
    operations: &["putKey", "compareAndSwap"],
    error_codes: &["too_large"],
};

// api-test: putKey
// api-test: compareAndSwap
// api-test: too_large
#[test]
fn body_over_512_kib_is_too_large() {
    each_target(&BODY_LIMIT, |node| {
        let key = format!("{}b", fresh_prefix());
        let padding = " ".repeat(524_288);
        // A small value in a body padded past the limit: the body, not the value, is too large.
        let put = format!(r#"{{"value":"v"{padding}}}"#);
        let id = fresh_id();
        let target = format!("/v1/kv?key={}", super::support::enc(&key));
        let response = node.write("PUT", &target, Some(&id), Some(&put));
        error(&response, 413, "too_large", "PUT with a body over 512 KiB");
        let cas = format!(r#"{{"expected":{{"kind":"absent"}},"value":"v"{padding}}}"#);
        let target = format!("/v1/cas?key={}", super::support::enc(&key));
        let response = node.write("POST", &target, Some(&fresh_id()), Some(&cas));
        error(&response, 413, "too_large", "CAS with a body over 512 KiB");
        error(
            &node.read(&key),
            404,
            "not_found",
            "a refused oversized write stores nothing",
        );
    });
}

const QUOTAS: Tag = Tag {
    test: "live_key_quota_refuses_new_keys_as_a_terminal_result",
    class: Class::Semantic,
    operations: &["putKey", "deleteKey", "getRequestStatus"],
    error_codes: &["capacity_exceeded"],
};

// api-test: putKey
// api-test: deleteKey
// api-test: getRequestStatus
// api-test: capacity_exceeded
#[test]
fn live_key_quota_refuses_new_keys_as_a_terminal_result() {
    let limits = Limits::new(256, 65_536, 2, 1 << 20).unwrap();
    with_model(&QUOTAS, limits, |node| {
        let (a, b, c) = ("quota/a", "quota/b", "quota/c");
        ok(&node.put(a, "1", &fresh_id()), "first key fits");
        ok(&node.put(b, "2", &fresh_id()), "second key fits");
        let overwrite = ok(&node.put(a, "3", &fresh_id()), "overwriting at quota fits");
        let refused_id = fresh_id();
        let refused = node.put(c, "4", &refused_id);
        let details = error(&refused, 507, "capacity_exceeded", "a third live key");
        assert_keys(
            &details,
            &["index", "limit", "replayed", "request_id"],
            "a capacity_exceeded terminal result carries its index, limit, replayed and request_id",
        );
        assert_eq!(
            details["limit"], "max_live_keys",
            "rule: the limit is named; {details}"
        );
        assert_eq!(
            details["replayed"], false,
            "rule: the first answer is not a replay"
        );
        assert_eq!(
            details["request_id"], refused_id,
            "rule: the request ID is echoed"
        );
        assert!(
            details["index"].as_u64() > Some(index_of(&overwrite).0),
            "rule: a refused write still takes a log index (DM-3); {details}"
        );
        // The refusal is terminal: it consumed the ID, and a retry returns it, not a re-run.
        ok(&node.delete(a, &fresh_id()), "deleting frees a slot");
        let again = node.put(c, "4", &refused_id);
        let replay = error(
            &again,
            507,
            "capacity_exceeded",
            "a retry of the refused write",
        );
        assert_eq!(
            replay["replayed"], true,
            "rule: the retry is a replay; {replay}"
        );
        assert_eq!(
            replay["index"], details["index"],
            "rule: the same terminal result"
        );
        let status = ok(
            &node.status_after(&refused_id, Index(details["index"].as_u64().unwrap_or(0))),
            "status of the refused write",
        );
        assert_eq!(
            status["state"], "known",
            "rule: a terminal refusal is known; {status}"
        );
        // A new ID may now use the freed slot.
        ok(
            &node.put(c, "5", &fresh_id()),
            "a new ID after the delete fits",
        );
    });
}

const BYTE_QUOTA: Tag = Tag {
    test: "live_byte_quota_names_its_limit",
    class: Class::Semantic,
    operations: &["putKey"],
    error_codes: &["capacity_exceeded"],
};

// api-test: putKey
// api-test: capacity_exceeded
#[test]
fn live_byte_quota_names_its_limit() {
    let limits = Limits::new(256, 65_536, 100, 64).unwrap();
    with_model(&BYTE_QUOTA, limits, |node| {
        ok(
            &node.put("k", &"v".repeat(40), &fresh_id()),
            "40 value bytes fit in 64",
        );
        let details = error(
            &node.put("j", &"w".repeat(40), &fresh_id()),
            507,
            "capacity_exceeded",
            "80 live bytes do not fit in 64",
        );
        assert_eq!(
            details["limit"], "max_live_bytes",
            "rule: the byte quota is named; {details}"
        );
    });
}
