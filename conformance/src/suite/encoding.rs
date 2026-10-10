//! Query-string encoding of the `key` parameter (§8.2): `%2B` is a plus, `%20` a space, a
//! raw `+` is refused, and invalid percent escapes, invalid UTF-8 and repeated parameters
//! are `400 invalid_request`.

#![cfg(test)]

use super::support::{each_target, enc, error, fresh_id, fresh_prefix, index_of, ok, say};
use crate::tag::{Class, Tag};

const ROUND_TRIP: Tag = Tag {
    test: "escaped_plus_and_space_name_distinct_keys",
    class: Class::Semantic,
    operations: &["putKey", "getKey"],
    error_codes: &["not_found"],
};

// api-test: putKey
// api-test: getKey
// api-test: not_found
#[test]
pub(super) fn escaped_plus_and_space_name_distinct_keys() {
    each_target(&ROUND_TRIP, |node| {
        let prefix = fresh_prefix();
        let (plus, space) = (format!("{prefix}a+b"), format!("{prefix}a b"));
        let put = ok(&node.put(&plus, "plus", &fresh_id()), "PUT of a%2Bb");
        let wrote = index_of(&put);
        let got = ok(&node.read_after(&plus, wrote), "GET of a%2Bb");
        assert_eq!(got["key"], plus, "rule: %2B decodes to a plus; body={got}");
        assert_eq!(
            got["value"], "plus",
            "rule: the plus key holds its value; body={got}"
        );
        let miss = node.read_after(&space, wrote);
        error(
            &miss,
            404,
            "not_found",
            "a%20b is a different key from a%2Bb",
        );
        let put = ok(&node.put(&space, "space", &fresh_id()), "PUT of a%20b");
        let got = ok(&node.read_after(&space, index_of(&put)), "GET of a%20b");
        assert_eq!(
            got["key"], space,
            "rule: %20 decodes to a space; body={got}"
        );
        assert_eq!(
            got["value"], "space",
            "rule: the space key holds its own value"
        );
        let again = ok(
            &node.read_after(&plus, index_of(&put)),
            "GET of a%2Bb again",
        );
        assert_eq!(
            again["value"], "plus",
            "rule: writing a%20b leaves a%2Bb alone"
        );
    });
}

const RAW_PLUS: Tag = Tag {
    test: "raw_plus_is_refused_and_changes_nothing",
    class: Class::Semantic,
    operations: &["putKey", "getKey", "deleteKey", "scanKeys"],
    error_codes: &["invalid_request"],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
// api-test: scanKeys
// api-test: invalid_request
#[test]
pub(super) fn raw_plus_is_refused_and_changes_nothing() {
    each_target(&RAW_PLUS, |node| {
        let prefix = fresh_prefix();
        let (key, id) = (format!("{prefix}x"), fresh_id());
        let target = format!("/v1/kv?key={}x+y", enc(&prefix));
        let get = node.get(&target);
        error(
            &get,
            400,
            "invalid_request",
            "a raw + in GET key is refused",
        );
        let put = node.write("PUT", &target, Some(&id), Some(r#"{"value":"v"}"#));
        error(
            &put,
            400,
            "invalid_request",
            "a raw + in PUT key is refused",
        );
        let delete = node.write("DELETE", &target, Some(&fresh_id()), None);
        error(
            &delete,
            400,
            "invalid_request",
            "a raw + in DELETE key is refused",
        );
        for parameter in ["start", "end", "after"] {
            let scan = node.get(&format!("/v1/scan?{parameter}=a+b"));
            error(
                &scan,
                400,
                "invalid_request",
                "a raw + in a scan bound is refused",
            );
        }
        for stored in [format!("{prefix}x y"), format!("{prefix}x+y"), key] {
            let read = node.read(&stored);
            assert_eq!(
                read.status,
                404,
                "rule: a refused PUT stores nothing; key={stored:?} {}",
                say(&read)
            );
        }
    });
}

const BAD_QUERY: Tag = Tag {
    test: "invalid_percent_utf8_repeated_and_missing_key_are_refused",
    class: Class::Semantic,
    operations: &["putKey", "getKey", "deleteKey"],
    error_codes: &["invalid_request"],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
// api-test: invalid_request
#[test]
fn invalid_percent_utf8_repeated_and_missing_key_are_refused() {
    each_target(&BAD_QUERY, |node| {
        for (why, query) in [
            ("a percent sign followed by non-hex", "key=%zz"),
            ("a truncated escape", "key=ab%4"),
            ("a trailing percent", "key=ab%"),
            ("a lone continuation byte", "key=%80"),
            ("an invalid UTF-8 byte", "key=%FF"),
            ("an overlong encoding", "key=%C0%AF"),
            ("a truncated multibyte sequence", "key=%E2%82"),
            ("a repeated key parameter", "key=a&key=b"),
            ("an empty key", "key="),
            ("a missing key parameter", ""),
            ("an unrelated parameter only", "other=a"),
        ] {
            let target = if query.is_empty() {
                "/v1/kv".to_owned()
            } else {
                format!("/v1/kv?{query}")
            };
            let get = node.get(&target);
            error(&get, 400, "invalid_request", &format!("GET with {why}"));
            let put = node.write("PUT", &target, Some(&fresh_id()), Some(r#"{"value":"v"}"#));
            error(&put, 400, "invalid_request", &format!("PUT with {why}"));
            let delete = node.write("DELETE", &target, Some(&fresh_id()), None);
            error(
                &delete,
                400,
                "invalid_request",
                &format!("DELETE with {why}"),
            );
        }
    });
}

const MULTIBYTE: Tag = Tag {
    test: "multibyte_and_reserved_characters_round_trip",
    class: Class::Semantic,
    operations: &["putKey", "getKey", "deleteKey"],
    error_codes: &[],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
#[test]
fn multibyte_and_reserved_characters_round_trip() {
    each_target(&MULTIBYTE, |node| {
        let prefix = fresh_prefix();
        for name in [
            "é",
            "ключ",
            "日本語",
            "😀",
            "a&b=c",
            "q?x#y",
            "100%",
            "sl/ash",
            "tab\tnl\n",
        ] {
            let key = format!("{prefix}{name}");
            let value = format!("value of {name} ✓");
            let put = ok(
                &node.put(&key, &value, &fresh_id()),
                "PUT of an escaped key",
            );
            let got = ok(
                &node.read_after(&key, index_of(&put)),
                "GET of an escaped key",
            );
            assert_eq!(
                got["key"], key,
                "rule: the key survives escaping; name={name:?}"
            );
            assert_eq!(
                got["value"], value,
                "rule: the value survives escaping; name={name:?}"
            );
            let gone = ok(&node.delete(&key, &fresh_id()), "DELETE of an escaped key");
            assert_eq!(
                gone["existed"], true,
                "rule: the escaped key was one key; name={name:?}"
            );
        }
        // Keys are stored exactly (DM-1): composed and decomposed forms are different keys.
        let (composed, decomposed) = (format!("{prefix}\u{e9}"), format!("{prefix}e\u{301}"));
        ok(
            &node.put(&composed, "composed", &fresh_id()),
            "PUT of the composed form",
        );
        let last = ok(
            &node.put(&decomposed, "decomposed", &fresh_id()),
            "PUT of the decomposed form",
        );
        let got = ok(
            &node.read_after(&composed, index_of(&last)),
            "GET of the composed form",
        );
        assert_eq!(
            got["value"], "composed",
            "rule: no Unicode normalisation (DM-1); body={got}"
        );
        let got = ok(
            &node.read_after(&decomposed, index_of(&last)),
            "GET of the decomposed form",
        );
        assert_eq!(
            got["value"], "decomposed",
            "rule: no Unicode normalisation (DM-1); body={got}"
        );
    });
}
