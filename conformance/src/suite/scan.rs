//! SCAN (F-4, §8.5): half-open ranges in byte order, empty ranges, exact page caps,
//! pagination by `after`, and parameter validation.

#![cfg(test)]

use serde_json::Value;

use super::support::{
    assert_keys, each_target, enc, error, fresh_id, fresh_prefix, index_of, ok, prefix_range,
};
use crate::poll::Index;
use crate::suite::support::Node;
use crate::tag::{Class, Tag};

/// Writes `count` keys `{prefix}{n:03}` holding `value(n)`; returns the keys and the last
/// write's index.
fn seed(
    node: &Node,
    prefix: &str,
    count: usize,
    value: impl Fn(&str) -> String,
) -> (Vec<String>, Index) {
    let mut keys = Vec::new();
    let mut last = Index(0);
    for n in 0..count {
        let key = format!("{prefix}{n:03}");
        let put = ok(&node.put(&key, &value(&key), &fresh_id()), "seeding PUT");
        last = last.max(index_of(&put));
        keys.push(key);
    }
    (keys, last)
}

fn keys_of(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("rule: a page has items; page={page}"))
        .iter()
        .map(|item| item["key"].as_str().unwrap_or_default().to_owned())
        .collect()
}

const EMPTY: Tag = Tag {
    test: "scan_empty_ranges_return_empty_pages",
    class: Class::Semantic,
    operations: &["scanKeys", "putKey"],
    error_codes: &[],
};

// api-test: scanKeys
// api-test: putKey
#[test]
fn scan_empty_ranges_return_empty_pages() {
    each_target(&EMPTY, |node| {
        let prefix = fresh_prefix();
        let (keys, last) = seed(node, &prefix, 3, |key| format!("v-{key}"));
        let empty = |query: &str, why: &str| {
            let page = ok(&node.scan_after(query, last), why);
            assert_keys(
                &page,
                &["has_more", "items", "next_after", "served_by"],
                "a page is has_more, items, next_after and served_by",
            );
            assert_eq!(
                keys_of(&page),
                Vec::<String>::new(),
                "rule: {why}; page={page}"
            );
            assert_eq!(
                page["has_more"], false,
                "rule: an empty page has no more; {why}"
            );
            assert!(
                page["next_after"].is_null(),
                "rule: an empty page has no cursor; {why}"
            );
        };
        let nothing = fresh_prefix();
        empty(&prefix_range(&nothing), "a range holding no keys");
        let start = enc(&keys[1]);
        empty(&format!("start={start}&end={start}"), "equal bounds");
        empty(
            &format!("{}&after={}", prefix_range(&prefix), enc(&keys[2])),
            "after the last key",
        );
        let end = enc(&keys[1]);
        empty(
            &format!("start={}&end={end}&after={end}", enc(&keys[0])),
            "after at the end bound",
        );
        empty(
            &format!("start={}&end={end}&after={}", enc(&keys[0]), enc(&keys[2])),
            "after beyond the end bound",
        );
    });
}

const ORDER: Tag = Tag {
    test: "scan_ranges_are_half_open_in_utf8_byte_order",
    class: Class::Semantic,
    operations: &["scanKeys", "putKey", "deleteKey"],
    error_codes: &[],
};

// api-test: scanKeys
// api-test: putKey
// api-test: deleteKey
#[test]
fn scan_ranges_are_half_open_in_utf8_byte_order() {
    each_target(&ORDER, |node| {
        let prefix = fresh_prefix();
        // Byte order differs from character-count and from UTF-16 order for the last two.
        let names = ["b", "a", "é", "😀", "\u{FF5E}", "c", "A"];
        let mut last = Index(0);
        let mut versions = Vec::new();
        for name in names {
            let put = ok(
                &node.put(&format!("{prefix}{name}"), name, &fresh_id()),
                "seeding PUT",
            );
            last = last.max(index_of(&put));
            versions.push((name, put["version"].clone()));
        }
        let mut sorted: Vec<String> = names.iter().map(|name| format!("{prefix}{name}")).collect();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let all = ok(
            &node.scan_after(&prefix_range(&prefix), last),
            "scan of the prefix",
        );
        assert_eq!(
            keys_of(&all),
            sorted,
            "rule: ascending byte-wise order of UTF-8 (DM-4)"
        );
        for item in all["items"].as_array().unwrap() {
            assert_keys(
                item,
                &["key", "value", "version"],
                "an item is key, value and version",
            );
            let name = item["key"]
                .as_str()
                .unwrap()
                .strip_prefix(&prefix)
                .unwrap()
                .to_owned();
            assert_eq!(item["value"], name, "rule: the item holds its key's value");
            let version = &versions.iter().find(|(n, _)| *n == name).unwrap().1;
            assert_eq!(
                &item["version"], version,
                "rule: the item carries the write's version"
            );
        }
        // [start, end): start is in, end is out.
        let (start, end) = (&sorted[1], &sorted[4]);
        let range = ok(
            &node.scan_after(&format!("start={}&end={}", enc(start), enc(end)), last),
            "scan of [start, end)",
        );
        assert_eq!(
            keys_of(&range),
            sorted[1..4].to_vec(),
            "rule: start inclusive, end exclusive"
        );
        // after is an exclusive lower bound intersected with the range.
        let after = ok(
            &node.scan_after(
                &format!("{}&after={}", prefix_range(&prefix), enc(&sorted[2])),
                last,
            ),
            "scan after a key",
        );
        assert_eq!(
            keys_of(&after),
            sorted[3..].to_vec(),
            "rule: after is exclusive"
        );
        let inside = ok(
            &node.scan_after(
                &format!(
                    "start={}&end={}&after={}",
                    enc(&sorted[3]),
                    enc(&sorted[5]),
                    enc(&sorted[0])
                ),
                last,
            ),
            "scan with after below start",
        );
        assert_eq!(
            keys_of(&inside),
            sorted[3..5].to_vec(),
            "rule: after is intersected with the range"
        );
        // Deleted keys are gone.
        let deleted = ok(&node.delete(&sorted[2], &fresh_id()), "DELETE");
        let page = ok(
            &node.scan_after(&prefix_range(&prefix), index_of(&deleted)),
            "scan after DELETE",
        );
        let mut expected = sorted.clone();
        expected.remove(2);
        assert_eq!(
            keys_of(&page),
            expected,
            "rule: a deleted key is not listed"
        );
    });
}

const CAPS: Tag = Tag {
    test: "scan_limit_caps_are_exact",
    class: Class::Semantic,
    operations: &["scanKeys", "putKey"],
    error_codes: &["invalid_request"],
};

// api-test: scanKeys
// api-test: putKey
// api-test: invalid_request
#[test]
pub(super) fn scan_limit_caps_are_exact() {
    each_target(&CAPS, |node| {
        let prefix = fresh_prefix();
        let (keys, last) = seed(node, &prefix, 129, |_| "v".to_owned());
        let range = prefix_range(&prefix);
        let page = |limit: Option<&str>| {
            let query =
                limit.map_or_else(|| range.clone(), |limit| format!("{range}&limit={limit}"));
            ok(&node.scan_after(&query, last), "scan with a limit")
        };
        let default = page(None);
        assert_eq!(
            keys_of(&default),
            keys[..32].to_vec(),
            "rule: the default limit is 32"
        );
        assert_eq!(
            default["has_more"], true,
            "rule: more remain after 32 of 129"
        );
        assert_eq!(
            default["next_after"], keys[31],
            "rule: next_after is the last returned key"
        );
        let one = page(Some("1"));
        assert_eq!(
            keys_of(&one),
            keys[..1].to_vec(),
            "rule: limit 1 returns one item"
        );
        assert_eq!(
            one["has_more"], true,
            "rule: has_more after a one-item page"
        );
        let max = page(Some("128"));
        assert_eq!(
            keys_of(&max),
            keys[..128].to_vec(),
            "rule: limit 128 returns 128 items"
        );
        assert_eq!(
            max["has_more"], true,
            "rule: the 129th key is still beyond the page"
        );
        assert_eq!(
            max["next_after"], keys[127],
            "rule: next_after is the 128th key"
        );
        let tail = ok(
            &node.scan_after(
                &format!("{range}&limit=128&after={}", enc(&keys[127])),
                last,
            ),
            "the last page",
        );
        assert_eq!(
            keys_of(&tail),
            keys[128..].to_vec(),
            "rule: the remaining key"
        );
        assert_eq!(tail["has_more"], false, "rule: nothing beyond the last key");
        assert!(
            tail["next_after"].is_null(),
            "rule: no cursor on the last page; page={tail}"
        );
        // Exactly `limit` keys left is a complete page with no more.
        let exact = ok(
            &node.scan_after(&format!("{range}&limit=2&after={}", enc(&keys[126])), last),
            "a page that exactly exhausts the range",
        );
        assert_eq!(
            keys_of(&exact),
            keys[127..].to_vec(),
            "rule: two keys remain"
        );
        assert_eq!(
            exact["has_more"], false,
            "rule: a page that takes the last key has no more; page={exact}"
        );
        assert!(
            exact["next_after"].is_null(),
            "rule: no cursor when nothing follows; page={exact}"
        );
        let drained = ok(
            &node.scan_after(&format!("{range}&limit=2&after={}", enc(&keys[127])), last),
            "a page that holds the final key",
        );
        assert_eq!(
            drained["has_more"], false,
            "rule: one key left under a limit of 2 is the end"
        );
        refuses_bad_limits(node, &range);
    });
}

/// Limits outside 1..=128, malformed or repeated are refused.
fn refuses_bad_limits(node: &Node, range: &str) {
    for bad in ["0", "129", "-1", "abc", "", "1.5", "99999999999999999999"] {
        error(
            &node.get(&format!("/v1/scan?{range}&limit={bad}")),
            400,
            "invalid_request",
            &format!("limit={bad:?}"),
        );
    }
    error(
        &node.get(&format!("/v1/scan?{range}&limit=1&limit=2")),
        400,
        "invalid_request",
        "a repeated limit",
    );
}

const PAGES: Tag = Tag {
    test: "scan_continuation_visits_every_key_once",
    class: Class::Semantic,
    operations: &["scanKeys", "putKey"],
    error_codes: &[],
};

// api-test: scanKeys
// api-test: putKey
#[test]
fn scan_continuation_visits_every_key_once() {
    each_target(&PAGES, |node| {
        let prefix = fresh_prefix();
        let (keys, last) = seed(node, &prefix, 70, |key| format!("value of {key}"));
        let range = prefix_range(&prefix);
        let mut seen: Vec<String> = Vec::new();
        let mut query = format!("{range}&limit=25");
        for round in 0..10 {
            let page = ok(&node.scan_after(&query, last), "a continued page");
            seen.extend(keys_of(&page));
            if page["has_more"] == false {
                assert_eq!(
                    seen, keys,
                    "rule: continuation returns every key once, in order"
                );
                assert_eq!(round, 2, "rule: 70 keys at 25 per page take three pages");
                return;
            }
            let cursor = page["next_after"]
                .as_str()
                .expect("has_more gives next_after");
            assert_eq!(
                Some(cursor),
                seen.last().map(String::as_str),
                "rule: next_after is exactly the last returned key"
            );
            query = format!("{range}&limit=25&after={}", enc(cursor));
        }
        panic!(
            "rule: a 70-key traversal ends within three pages; seen={}",
            seen.len()
        );
    });
}

const BYTES: Tag = Tag {
    test: "scan_page_stops_at_one_mebibyte_of_key_and_value_bytes",
    class: Class::Semantic,
    operations: &["scanKeys", "putKey"],
    error_codes: &[],
};

// api-test: scanKeys
// api-test: putKey
#[test]
fn scan_page_stops_at_one_mebibyte_of_key_and_value_bytes() {
    each_target(&BYTES, |node| {
        let prefix = fresh_prefix();
        // Each item is exactly 64 KiB of key plus value, so 16 items are exactly 1 MiB.
        let sized = |key: &str| "x".repeat(65_536 - key.len());
        let (keys, last) = seed(node, &prefix, 17, sized);
        let range = prefix_range(&prefix);
        let page = ok(
            &node.scan_after(&format!("{range}&limit=128"), last),
            "a 17-item range",
        );
        assert_eq!(
            keys_of(&page),
            keys[..16].to_vec(),
            "rule: a page holds up to exactly 1 MiB of key+value bytes (16 items)"
        );
        assert_eq!(
            page["has_more"], true,
            "rule: the 17th item would exceed the cap"
        );
        assert_eq!(
            page["next_after"], keys[15],
            "rule: next_after is the last returned key"
        );
        let rest = ok(
            &node.scan_after(&format!("{range}&limit=128&after={}", enc(&keys[15])), last),
            "the continuation",
        );
        assert_eq!(
            keys_of(&rest),
            keys[16..].to_vec(),
            "rule: the continuation holds the 17th"
        );
        assert_eq!(rest["has_more"], false, "rule: and it is the end");
        // One byte more per item pushes the 16th over the cap: 15 items fit.
        let prefix = fresh_prefix();
        let (keys, last) = seed(node, &prefix, 16, |key| "x".repeat(65_537 - key.len()));
        let page = ok(
            &node.scan_after(&format!("{}&limit=128", prefix_range(&prefix)), last),
            "a range of 64 KiB + 1 items",
        );
        assert_eq!(
            keys_of(&page),
            keys[..15].to_vec(),
            "rule: the page stops before the item that would exceed 1 MiB"
        );
        assert_eq!(page["has_more"], true, "rule: the 16th item remains");
        // "The first item is always returned" cannot be provoked over HTTP: a valid item is at
        // most about 64.25 KiB, far below the cap, so that rule is covered by the model's own tests.
    });
}

const PARAMS: Tag = Tag {
    test: "scan_rejects_malformed_parameters",
    class: Class::Semantic,
    operations: &["scanKeys"],
    error_codes: &["invalid_request"],
};

// api-test: scanKeys
// api-test: invalid_request
#[test]
fn scan_rejects_malformed_parameters() {
    each_target(&PARAMS, |node| {
        for (why, query) in [
            ("start after end", "start=b&end=a"),
            ("an empty start", "start="),
            ("an empty end", "end="),
            ("an empty after", "after="),
            ("an invalid percent escape in start", "start=%zz"),
            ("an invalid percent escape in end", "end=%4"),
            ("invalid UTF-8 in after", "after=%FF"),
            ("invalid UTF-8 in start", "start=%C0%AF"),
            ("a repeated start", "start=a&start=b"),
            ("a repeated end", "end=a&end=b"),
            ("a repeated after", "after=a&after=b"),
            ("a raw plus in start", "start=a+b"),
        ] {
            error(
                &node.get(&format!("/v1/scan?{query}")),
                400,
                "invalid_request",
                why,
            );
        }
    });
}
