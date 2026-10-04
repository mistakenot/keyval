//! SCAN pages over the model's state (PLAN.md F-4, §8.5, §7.11 "SCAN page").
//!
//! - **Range.** `[start, end)` in byte-wise key order; an omitted bound is unbounded.
//!   `start > end` is invalid; equal bounds give an empty page. `after` is an exclusive
//!   lower continuation bound, intersected with the range, so `after >= end` gives an
//!   empty page.
//! - **Page.** At most `limit` items (1 to 128, default 32) and at most 1 MiB of decoded
//!   key and value bytes. Items are taken in key order. The first item is always taken;
//!   each later one only if the page stays within both caps, and the page stops before
//!   the first item that would exceed either. Only returned items' key and value bytes
//!   count.
//! - **Continuation.** `has_more` is true when another key in the range lies beyond the
//!   returned items, and then `next_after` is the last returned key; otherwise it is
//!   `None`. Repeating the range with `after = next_after` continues the traversal, and a
//!   traversal over unchanged state returns every key in the range exactly once.

use std::error::Error;
use std::fmt;
use std::ops::Bound;

use crate::apply::Model;
use crate::data::{DataError, Key, Value, Version};

/// The most items a page holds (F-4).
pub const MAX_PAGE_ITEMS: u64 = 128;

/// The page size when a SCAN gives no `limit` (F-4).
pub const DEFAULT_PAGE_ITEMS: u64 = 32;

/// The most decoded key and value bytes a page holds: 1 MiB (F-4, §7.11).
pub const MAX_PAGE_BYTES: u64 = 1 << 20;

/// Why a SCAN request is invalid (`400 invalid_request`, §8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// `start` is after `end`.
    StartAfterEnd,
    /// `limit` is outside 1 to 128.
    Limit {
        /// The limit given.
        value: u64,
    },
    /// A bound breaks the configured key limit, which is in force wherever a key is parsed
    /// (§7.10, D-80).
    Key(DataError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartAfterEnd => f.write_str("start is after end"),
            Self::Limit { value } => write!(f, "limit {value} is outside 1 to 128"),
            Self::Key(error) => write!(f, "bound: {error}"),
        }
    }
}

impl Error for ScanError {}

/// A valid SCAN request. An empty key cannot be a bound: a [`Key`] has at least one
/// byte, so an empty bound is refused when the key is made (§8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRequest {
    start: Option<Key>,
    end: Option<Key>,
    after: Option<Key>,
    limit: u64,
}

impl ScanRequest {
    /// The request for `[start, end)` after `after`, with `limit` items per page
    /// (default 32).
    ///
    /// # Errors
    ///
    /// [`ScanError::StartAfterEnd`] or [`ScanError::Limit`].
    pub fn new(
        start: Option<Key>,
        end: Option<Key>,
        after: Option<Key>,
        limit: Option<u64>,
    ) -> Result<Self, ScanError> {
        if let (Some(start), Some(end)) = (&start, &end)
            && start > end
        {
            return Err(ScanError::StartAfterEnd);
        }
        let limit = limit.unwrap_or(DEFAULT_PAGE_ITEMS);
        if !(1..=MAX_PAGE_ITEMS).contains(&limit) {
            return Err(ScanError::Limit { value: limit });
        }
        Ok(Self {
            start,
            end,
            after,
            limit,
        })
    }

    /// The same range and limit, continuing after `after`.
    #[must_use]
    pub fn continued(&self, after: Key) -> Self {
        Self {
            after: Some(after),
            ..self.clone()
        }
    }
}

/// One returned item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The key.
    pub key: Key,
    /// Its value.
    pub value: Value,
    /// Its version.
    pub version: Version,
}

/// One SCAN page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The items, in key order.
    pub items: Vec<Item>,
    /// Whether another key in the range lies beyond the returned items.
    pub has_more: bool,
    /// The last returned key when `has_more`; otherwise `None`.
    pub next_after: Option<Key>,
}

impl Model {
    /// One SCAN page of the current state.
    ///
    /// # Errors
    ///
    /// [`ScanError::Key`] if a bound breaks the configured key limit (`invalid_request`).
    pub fn scan(&self, request: &ScanRequest) -> Result<Page, ScanError> {
        for bound in [&request.start, &request.end, &request.after]
            .into_iter()
            .flatten()
        {
            self.limits().check_key(bound).map_err(ScanError::Key)?;
        }
        let lower = match (&request.start, &request.after) {
            (Some(start), Some(after)) if after < start => Bound::Included(start),
            (_, Some(after)) => Bound::Excluded(after),
            (Some(start), None) => Bound::Included(start),
            (None, None) => Bound::Unbounded,
        };
        let in_range = self
            .entries_from(lower)
            .take_while(|(key, _, _)| request.end.as_ref().is_none_or(|end| *key < end));
        let mut items: Vec<Item> = Vec::new();
        let mut bytes = 0;
        let mut has_more = false;
        for (key, value, version) in in_range {
            let item_bytes = key.bytes() + value.bytes();
            let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
            let fits =
                items.is_empty() || (count < request.limit && bytes + item_bytes <= MAX_PAGE_BYTES);
            if !fits {
                has_more = true;
                break;
            }
            bytes += item_bytes;
            items.push(Item {
                key: key.clone(),
                value: value.clone(),
                version,
            });
        }
        let next_after = if has_more {
            items.last().map(|item| item.key.clone())
        } else {
            None
        };
        Ok(Page {
            items,
            has_more,
            next_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{Command, Entry, Op};
    use crate::data::{Index, Limits, RequestId};

    /// A page of a request valid under the model's limits.
    trait ScanOk {
        fn scan_ok(&self, request: &ScanRequest) -> Page;
    }

    impl ScanOk for Model {
        fn scan_ok(&self, request: &ScanRequest) -> Page {
            self.scan(request).unwrap()
        }
    }

    fn key(text: &str) -> Key {
        Key::new(text).unwrap()
    }

    fn page_bytes(page: &Page) -> u64 {
        page.items
            .iter()
            .map(|item| item.key.bytes() + item.value.bytes())
            .sum()
    }

    /// A model holding `pairs`, written at indices 1, 2, … in order.
    fn model_of(pairs: &[(&str, String)]) -> Model {
        let mut model = Model::new(Limits::DEFAULT);
        for (n, (k, v)) in (1_u64..).zip(pairs) {
            let entry = Entry::Command(Command {
                request_id: RequestId::from_bytes(n.to_be_bytes().repeat(2).try_into().unwrap()),
                op: Op::Put {
                    key: key(k),
                    value: Value::new(v.clone()).unwrap(),
                },
            });
            model.apply(Index::new(n).unwrap(), &entry).unwrap();
        }
        model
    }

    fn keys(page: &Page) -> Vec<&str> {
        page.items.iter().map(|item| item.key.as_str()).collect()
    }

    fn request(
        start: Option<&str>,
        end: Option<&str>,
        after: Option<&str>,
        limit: Option<u64>,
    ) -> ScanRequest {
        ScanRequest::new(start.map(key), end.map(key), after.map(key), limit).unwrap()
    }

    fn abcde() -> Model {
        model_of(&["a", "b", "c", "d", "e"].map(|k| (k, format!("v{k}"))))
    }

    #[test]
    fn the_range_is_start_inclusive_end_exclusive_in_key_order() {
        let model = abcde();
        let cases = [
            (
                request(None, None, None, None),
                vec!["a", "b", "c", "d", "e"],
            ),
            (request(Some("b"), Some("d"), None, None), vec!["b", "c"]),
            (request(Some("bb"), None, None, None), vec!["c", "d", "e"]),
            (request(None, Some("c"), None, None), vec!["a", "b"]),
            (request(Some("c"), Some("c"), None, None), vec![]),
        ];
        for (scan, expected) in cases {
            let page = model.scan_ok(&scan);
            assert_eq!(keys(&page), expected, "rule: [start, end) (§8.5); {scan:?}");
            assert!(!page.has_more && page.next_after.is_none());
        }
        assert_eq!(
            model.scan_ok(&request(None, None, None, None)).items[1].version,
            Version::new(2).unwrap(),
            "rule: items carry their versions (F-4)"
        );
    }

    #[test]
    fn after_is_an_exclusive_bound_intersected_with_the_range() {
        let model = abcde();
        let cases = [
            (
                request(Some("b"), Some("e"), Some("b"), None),
                vec!["c", "d"],
            ),
            (
                request(Some("c"), None, Some("a"), None),
                vec!["c", "d", "e"],
            ),
            (request(None, Some("d"), Some("d"), None), vec![]),
            (request(None, Some("d"), Some("z"), None), vec![]),
            (request(None, None, Some("cc"), None), vec!["d", "e"]),
        ];
        for (scan, expected) in cases {
            assert_eq!(
                keys(&model.scan_ok(&scan)),
                expected,
                "rule: after is exclusive and intersected with the range; after >= end is \
                 empty (§8.5); {scan:?}"
            );
        }
    }

    #[test]
    fn invalid_requests_are_refused() {
        assert_eq!(
            ScanRequest::new(Some(key("b")), Some(key("a")), None, None),
            Err(ScanError::StartAfterEnd),
            "rule: start > end is invalid (§8.5)"
        );
        for limit in [0, 129] {
            assert_eq!(
                ScanRequest::new(None, None, None, Some(limit)),
                Err(ScanError::Limit { value: limit }),
                "rule: limit is 1 to 128 (F-4)"
            );
        }
        for limit in [1, 128] {
            assert!(ScanRequest::new(None, None, None, Some(limit)).is_ok());
        }
    }

    #[test]
    fn the_item_limit_stops_the_page_and_sets_the_continuation() {
        let model = abcde();
        let page = model.scan_ok(&request(None, None, None, Some(2)));
        assert_eq!(
            (
                keys(&page),
                page.has_more,
                page.next_after.as_ref().map(Key::as_str)
            ),
            (vec!["a", "b"], true, Some("b")),
            "rule: at most limit items; next_after is the last returned key (F-4)"
        );
        let exact = model.scan_ok(&request(Some("d"), None, None, Some(2)));
        assert_eq!(
            (keys(&exact), exact.has_more, exact.next_after.as_ref()),
            (vec!["d", "e"], false, None),
            "rule: has_more only when another key in range lies beyond the page"
        );
    }

    #[test]
    fn the_default_limit_is_32() {
        let names: Vec<String> = (0..40).map(|n| format!("k{n:02}")).collect();
        let pairs: Vec<(&str, String)> =
            names.iter().map(|k| (k.as_str(), String::new())).collect();
        let page = model_of(&pairs).scan_ok(&request(None, None, None, None));
        assert_eq!(
            page.items.len(),
            32,
            "rule: the default page is 32 items (F-4)"
        );
        assert!(page.has_more);
    }

    #[test]
    fn the_byte_cap_stops_before_the_item_that_would_exceed_it() {
        // 16 items of exactly 64 KiB (3-byte key + 65,533-byte value) fill 1 MiB exactly.
        let names: Vec<String> = (0..20).map(|n| format!("k{n:02}")).collect();
        let exact: Vec<(&str, String)> = names
            .iter()
            .map(|k| (k.as_str(), "v".repeat(65_533)))
            .collect();
        let page = model_of(&exact).scan_ok(&request(None, None, None, Some(128)));
        assert_eq!(
            (page.items.len(), page.has_more),
            (16, true),
            "rule: items up to exactly 1 MiB of decoded bytes fit; the next would exceed it \
             (§7.11)"
        );
        // One byte more per item: the 16th would exceed 1 MiB, so 15 fit.
        let over: Vec<(&str, String)> = names
            .iter()
            .map(|k| (k.as_str(), "v".repeat(65_534)))
            .collect();
        let page = model_of(&over).scan_ok(&request(None, None, None, Some(128)));
        assert_eq!(
            (page.items.len(), page.next_after.as_ref().map(Key::as_str)),
            (15, Some("k14")),
            "rule: the page stops before the item that would exceed the byte cap (§7.11)"
        );
    }

    #[test]
    fn a_maximum_size_item_always_fits() {
        // The largest item: a 256-byte key and a 64 KiB value, 65,792 bytes in all.
        let longest = "k".repeat(256);
        let pairs = [(longest.as_str(), "v".repeat(65_536))];
        let page = model_of(&pairs).scan_ok(&request(None, None, None, Some(1)));
        assert_eq!(
            (page.items.len(), page.has_more, page_bytes(&page)),
            (1, false, 65_792),
            "rule: one maximum-size item always fits (F-4, §7.11)"
        );
    }

    #[test]
    fn a_page_filled_to_exactly_1_mib_with_nothing_beyond_has_no_more() {
        let names: Vec<String> = (0..16).map(|n| format!("k{n:02}")).collect();
        let pairs: Vec<(&str, String)> = names
            .iter()
            .map(|k| (k.as_str(), "v".repeat(65_533)))
            .collect();
        let page = model_of(&pairs).scan_ok(&request(None, None, None, Some(128)));
        assert_eq!(
            (
                page.items.len(),
                page_bytes(&page),
                page.has_more,
                page.next_after
            ),
            (16, MAX_PAGE_BYTES, false, None),
            "rule: a page at exactly 1 MiB holding the last key in range has no more (F-4)"
        );
    }

    #[test]
    fn a_bound_over_the_configured_key_limit_is_refused() {
        let model = Model::new(Limits::new(4, 8, 100, 1 << 20).unwrap());
        for (start, end, after) in [
            (Some("abcde"), None, None),
            (None, Some("abcde"), None),
            (None, None, Some("abcde")),
        ] {
            assert_eq!(
                model.scan(&request(start, end, after, None)),
                Err(ScanError::Key(DataError::KeyTooLong { bytes: 5, limit: 4 })),
                "rule: the configured key limit is in force for SCAN bounds too (§7.10, D-80, \
                 F-8)"
            );
        }
        assert_eq!(
            model
                .scan(&request(Some("abcd"), None, None, None))
                .map(|page| page.items.len()),
            Ok(0),
            "rule: a bound at the limit is fine"
        );
    }

    #[test]
    fn a_continued_traversal_returns_every_key_in_range_exactly_once() {
        let names: Vec<String> = (0..50).map(|n| format!("k{n:02}")).collect();
        let pairs: Vec<(&str, String)> = names
            .iter()
            .enumerate()
            .map(|(n, k)| (k.as_str(), "v".repeat(n * 9_973 % 65_536)))
            .collect();
        let model = model_of(&pairs);
        for limit in [1, 3, 7, 128] {
            let first = request(Some("k05"), Some("k45"), None, Some(limit));
            let mut seen: Vec<String> = Vec::new();
            let mut pages = 0;
            let mut scan = first.clone();
            loop {
                let page = model.scan_ok(&scan);
                assert!(
                    !page.items.is_empty() && page_bytes(&page) <= MAX_PAGE_BYTES,
                    "rule: a continued page is never empty here, and never over 1 MiB"
                );
                pages += 1;
                seen.extend(page.items.iter().map(|item| item.key.as_str().to_owned()));
                match page.next_after {
                    Some(after) => scan = first.continued(after),
                    None => break,
                }
            }
            let expected: Vec<String> = names[5..45].to_vec();
            assert_eq!(
                seen, expected,
                "rule: a normally continued traversal returns each key once, in order (C-8); \
                 limit={limit}"
            );
            if limit == 128 {
                assert!(
                    pages > 1,
                    "test setup: at limit 128 the 1 MiB byte cap, not the item limit, splits \
                     the 40 keys into pages; pages={pages}"
                );
            }
        }
    }
}
