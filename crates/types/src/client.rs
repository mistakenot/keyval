//! The client API port's core types (PLAN.md §7.2): what an attempt asks for
//! ([`ClientOp`]) and what the core answers ([`Response`]).
//!
//! The HTTP adapter parses each request into a [`ClientOp`] (CD-3) and renders each
//! [`Response`] as the spec's body. Boundary-only errors (`invalid_request`,
//! `too_large`) never reach the core, so no [`Response`] carries them.

use crate::config::MAX_SCAN_PAGE_BYTES;
use crate::counts::PageSize;
use crate::data::{Key, Value};
use crate::error_code::ErrorCode;
use crate::ids::{NodeId, RequestId};
use crate::lifecycle::NotReadyLifecycle;
use crate::log_index::{EntryIndex, LogIndex, Version};
use crate::mutation::Mutation;
use crate::outcome::TerminalResult;

/// Names one attempt (one HTTP request) while the core works on it. The adapter
/// assigns it, and the core echoes it in the matching [`Respond`](crate::Effect::Respond).
///
/// Invariant: none beyond identity; the adapter keeps live IDs unique.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttemptId(u64);

impl AttemptId {
    /// An attempt ID.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw ID.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A SCAN's range and page size (PLAN.md §8.5, F-4).
///
/// Invariant: when both bounds are given, `start <= end` (equal bounds give an empty
/// page). `after` is an exclusive continuation bound, intersected with the range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRange {
    start: Option<Key>,
    end: Option<Key>,
    after: Option<Key>,
    limit: PageSize,
}

/// Why a SCAN range is invalid (`400 invalid_request`, §8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanRangeError {
    /// `start` sorts after `end`.
    StartAfterEnd,
}

impl ScanRange {
    /// Builds a range from parsed bounds.
    ///
    /// # Errors
    ///
    /// [`ScanRangeError::StartAfterEnd`] if both bounds are given and `start > end`.
    pub fn new(
        start: Option<Key>,
        end: Option<Key>,
        after: Option<Key>,
        limit: PageSize,
    ) -> Result<Self, ScanRangeError> {
        match (&start, &end) {
            (Some(start), Some(end)) if start > end => Err(ScanRangeError::StartAfterEnd),
            _ => Ok(Self {
                start,
                end,
                after,
                limit,
            }),
        }
    }

    /// The inclusive lower bound, if any.
    #[must_use]
    pub const fn start(&self) -> Option<&Key> {
        self.start.as_ref()
    }

    /// The exclusive upper bound, if any.
    #[must_use]
    pub const fn end(&self) -> Option<&Key> {
        self.end.as_ref()
    }

    /// The exclusive continuation bound, if any.
    #[must_use]
    pub const fn after(&self) -> Option<&Key> {
        self.after.as_ref()
    }

    /// The most items in the page.
    #[must_use]
    pub const fn limit(&self) -> PageSize {
        self.limit
    }

    /// Whether `key` lies in the page's range: in `[start, end)` and after `after`.
    #[must_use]
    pub fn contains(&self, key: &Key) -> bool {
        self.start.as_ref().is_none_or(|start| key >= start)
            && self.end.as_ref().is_none_or(|end| key < end)
            && self.after.as_ref().is_none_or(|after| key > after)
    }
}

/// What an attempt asks for, parsed at the boundary (CD-3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientOp {
    /// A PUT, DELETE or compare-and-swap, carrying its request ID (F-6).
    Write {
        /// The request ID from `Idempotency-Key`.
        request_id: RequestId,
        /// The command.
        mutation: Mutation,
    },
    /// GET one key (F-2).
    Get {
        /// The key to read.
        key: Key,
    },
    /// One SCAN page (F-4).
    Scan {
        /// The range and page size.
        range: ScanRange,
    },
    /// Look up a request's terminal result (F-9).
    Status {
        /// The request ID to look up.
        request_id: RequestId,
    },
}

/// Which node answered, and the applied prefix its answer reflects (C-3).
///
/// Invariant: `applied` is the serving node's applied index when it answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServedBy {
    /// The answering node.
    pub node: NodeId,
    /// Its applied index.
    pub applied: LogIndex,
}

/// One key in a SCAN page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanItem {
    /// The key.
    pub key: Key,
    /// Its value.
    pub value: Value,
    /// Its version.
    pub version: Version,
}

/// One SCAN page, copied from a single applied prefix (C-8).
///
/// Invariants, asserted by [`ScanPage::new`], its one constructor (CD-7):
/// - items are unique, in ascending key order and inside the range;
/// - there are at most the range's limit, and at most [`MAX_SCAN_PAGE_BYTES`] of decoded
///   key and value bytes (§7.11);
/// - `continuation`, when present, is the last returned key, so the API's
///   `has_more`/`next_after` pair is consistent by construction. The scan sets it
///   exactly when another key in range existed beyond the page in that view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanPage {
    /// The page's items.
    items: Vec<ScanItem>,
    /// Where the next page starts (`next_after`), if more keys remain.
    continuation: Option<Key>,
    /// The serving node and its applied prefix.
    served_by: ServedBy,
}

impl ScanPage {
    /// A page of `items` read from one applied prefix for `range` (§8.5).
    ///
    /// # Panics
    ///
    /// On a scan bug (CD-7): items not strictly ascending (invariant `SCAN-PAGE-ORDER`);
    /// an item outside `range` (`SCAN-PAGE-RANGE`); more items than `range`'s limit
    /// (`SCAN-PAGE-LIMIT`); more than [`MAX_SCAN_PAGE_BYTES`] decoded bytes
    /// (`SCAN-PAGE-BYTES`); a continuation that is not the last returned key
    /// (`SCAN-PAGE-CONTINUATION`).
    #[must_use]
    pub fn new(
        range: &ScanRange,
        items: Vec<ScanItem>,
        continuation: Option<Key>,
        served_by: ServedBy,
    ) -> Self {
        for (earlier, later) in items.iter().zip(items.iter().skip(1)) {
            crate::invariant!(
                earlier.key < later.key,
                "SCAN-PAGE-ORDER",
                "key {:?} is not before {:?}",
                earlier.key.as_str(),
                later.key.as_str()
            );
        }
        for item in &items {
            crate::invariant!(
                range.contains(&item.key),
                "SCAN-PAGE-RANGE",
                "key {:?} is outside the range {range:?}",
                item.key.as_str()
            );
        }
        let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
        crate::invariant!(
            count <= range.limit().get(),
            "SCAN-PAGE-LIMIT",
            "{count} items exceed the limit {}",
            range.limit()
        );
        let bytes = items.iter().fold(0_u64, |total, item| {
            let item_bytes = item.key.byte_len().saturating_add(item.value.byte_len());
            total.saturating_add(u64::try_from(item_bytes).unwrap_or(u64::MAX))
        });
        crate::invariant!(
            bytes <= MAX_SCAN_PAGE_BYTES,
            "SCAN-PAGE-BYTES",
            "{bytes} decoded bytes in {count} items exceed {MAX_SCAN_PAGE_BYTES}"
        );
        if let Some(next) = &continuation {
            let last = items.last().map(|item| item.key.as_str());
            crate::invariant!(
                last == Some(next.as_str()),
                "SCAN-PAGE-CONTINUATION",
                "continuation {:?} is not the last returned key {last:?}",
                next.as_str()
            );
        }
        Self {
            items,
            continuation,
            served_by,
        }
    }

    /// The page's items, in ascending key order.
    #[must_use]
    pub fn items(&self) -> &[ScanItem] {
        &self.items
    }

    /// Where the next page starts (`next_after`), if more keys remain.
    #[must_use]
    pub const fn continuation(&self) -> Option<&Key> {
        self.continuation.as_ref()
    }

    /// The serving node and its applied prefix.
    #[must_use]
    pub const fn served_by(&self) -> ServedBy {
        self.served_by
    }

    /// The page's parts, by value: items, continuation and serving node.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ScanItem>, Option<Key>, ServedBy) {
        (self.items, self.continuation, self.served_by)
    }
}

/// Which history capacity refused a write before proposal (DM-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryLimit {
    /// The commit index reached `max_log_entries`.
    LogEntries,
    /// First-execution command bytes reached `max_history_bytes`.
    HistoryBytes,
}

impl HistoryLimit {
    /// The limit as the API names it (the configuration parameter, §7.10).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LogEntries => "max_log_entries",
            Self::HistoryBytes => "max_history_bytes",
        }
    }
}

/// A non-terminal answer about this attempt (PLAN.md §8.1 shape 3). It says nothing
/// about earlier attempts with the same request ID (A-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `307`: a write reached a non-leader that believes `leader` leads. Not submitted.
    NotLeader {
        /// The believed leader.
        leader: NodeId,
    },
    /// `429`: a local admission bound was hit, or the proposal was dropped synchronously.
    /// Not submitted.
    Busy,
    /// `503`: no leader is known, or the node is shutting down. Not submitted.
    Unavailable,
    /// `503`: the node is not ready. Not submitted.
    Recovering {
        /// The node's state: never `ready`, by its type.
        lifecycle: NotReadyLifecycle,
    },
    /// `504`: submitted, but the commit was not observed. Unresolved.
    UnknownOutcome,
    /// `507`: the history capacity refused admission before proposal. Not submitted.
    HistoryFull {
        /// Which capacity refused it.
        limit: HistoryLimit,
    },
}

impl Refusal {
    /// The API error code.
    #[must_use]
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::NotLeader { .. } => ErrorCode::NotLeader,
            Self::Busy => ErrorCode::Busy,
            Self::Unavailable => ErrorCode::Unavailable,
            Self::Recovering { .. } => ErrorCode::Recovering,
            Self::UnknownOutcome => ErrorCode::UnknownOutcome,
            Self::HistoryFull { .. } => ErrorCode::HistoryFull,
        }
    }

    /// Whether this attempt submitted a proposal: only an unknown outcome did (A-5).
    #[must_use]
    pub const fn submitted(self) -> bool {
        // Every variant is named, so a new refusal must decide whether it was submitted
        // (CD-5); a catch-all here would answer "not submitted" for it and break A-5.
        match self {
            Self::UnknownOutcome => true,
            Self::NotLeader { .. }
            | Self::Busy
            | Self::Unavailable
            | Self::Recovering { .. }
            | Self::HistoryFull { .. } => false,
        }
    }
}

/// What the core answers an attempt with (the client API port's effect, §7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// A write reached its terminal result: a success, `version_mismatch` or
    /// `capacity_exceeded` (§8.3). `replayed` is true when this answer returns a result
    /// recorded earlier.
    Written {
        /// The stored terminal result.
        result: TerminalResult,
        /// Whether the result was recorded earlier (a retry or a duplicate entry).
        replayed: bool,
        /// The serving node and its applied prefix.
        served_by: ServedBy,
    },
    /// `409 request_id_conflict`: the ID was first used, with a different command, at
    /// `original_index`. Nothing changed.
    Conflict {
        /// The original execution's log index: an entry, so at least 1 (the spec's
        /// `EntryIndex`).
        original_index: EntryIndex,
    },
    /// GET found the key.
    Found {
        /// The key's value.
        value: Value,
        /// The key's version.
        version: Version,
        /// The serving node and its applied prefix.
        served_by: ServedBy,
    },
    /// `404 not_found`: GET did not find the key.
    NotFound {
        /// The serving node and its applied prefix.
        served_by: ServedBy,
    },
    /// One SCAN page.
    Page(ScanPage),
    /// A status lookup (F-9): the stored result if this node has applied the request.
    /// `None` is `unknown`, which never means "did not happen".
    Status {
        /// The stored terminal result, if known here.
        result: Option<TerminalResult>,
        /// The serving node and its applied prefix.
        served_by: ServedBy,
    },
    /// A non-terminal refusal.
    Refused(Refusal),
}

impl Response {
    /// The API error code this response is answered with, or `None` for a `200`.
    #[must_use]
    pub const fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Written { result, .. } => result.decision().error_code(),
            Self::Conflict { .. } => Some(ErrorCode::RequestIdConflict),
            Self::NotFound { .. } => Some(ErrorCode::NotFound),
            Self::Refused(refusal) => Some(refusal.error_code()),
            Self::Found { .. } | Self::Page(_) | Self::Status { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    // check: PORTS

    use super::*;
    use crate::outcome::{Decision, LiveStateLimit};

    /// A SCAN range over [`start`, `end`) with page size `limit`.
    fn scan_range(start: &str, end: &str, limit: u64) -> ScanRange {
        ScanRange::new(
            Some(Key::parse(start.to_owned()).unwrap()),
            Some(Key::parse(end.to_owned()).unwrap()),
            None,
            PageSize::new(limit).unwrap(),
        )
        .unwrap()
    }

    /// A scan item with `value_bytes` bytes of value, at version 1.
    fn scan_item(key: &str, value_bytes: usize) -> ScanItem {
        ScanItem {
            key: Key::parse(key.to_owned()).unwrap(),
            value: Value::parse("v".repeat(value_bytes)).unwrap(),
            version: Version::new(1).unwrap(),
        }
    }

    /// A serving node at applied index 9.
    fn served() -> ServedBy {
        ServedBy {
            node: NodeId::new(1).unwrap(),
            applied: LogIndex::new(9).unwrap(),
        }
    }

    #[test]
    fn a_scan_page_keeps_its_items_continuation_and_server() {
        let range = scan_range("a", "z", 3);
        let items = vec![scan_item("b", 1), scan_item("c", 1), scan_item("d", 1)];
        let continuation = Some(Key::parse("d".to_owned()).unwrap());
        let page = ScanPage::new(&range, items.clone(), continuation.clone(), served());
        assert_eq!(
            page.items(),
            items.as_slice(),
            "rule: items are kept in order (§8.5)"
        );
        assert_eq!(page.continuation(), continuation.as_ref());
        assert_eq!(page.served_by(), served());
        assert_eq!(page.into_parts(), (items, continuation, served()));
        let empty = ScanPage::new(&range, Vec::new(), None, served());
        assert!(
            empty.items().is_empty(),
            "rule: an empty page with no continuation is valid"
        );
    }

    #[test]
    fn a_scan_page_may_hold_exactly_one_mebibyte() {
        // 16 items of a 1-byte key and 65,535 value bytes: exactly 1 MiB (§7.11).
        let range = scan_range("a", "z", 128);
        let items: Vec<ScanItem> = ('b'..='q')
            .map(|key| scan_item(&key.to_string(), 65_535))
            .collect();
        let page = ScanPage::new(&range, items, None, served());
        assert_eq!(page.items().len(), 16, "rule: the 1 MiB cap is inclusive");
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-ORDER violated")]
    fn a_scan_page_out_of_order_is_an_invariant_failure() {
        let range = scan_range("a", "z", 3);
        let _ = ScanPage::new(
            &range,
            vec![scan_item("c", 1), scan_item("b", 1)],
            None,
            served(),
        );
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-ORDER violated")]
    fn a_scan_page_with_a_repeated_key_is_an_invariant_failure() {
        let range = scan_range("a", "z", 3);
        let _ = ScanPage::new(
            &range,
            vec![scan_item("b", 1), scan_item("b", 2)],
            None,
            served(),
        );
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-RANGE violated")]
    fn a_scan_page_item_outside_the_range_is_an_invariant_failure() {
        let range = scan_range("b", "d", 3);
        let _ = ScanPage::new(
            &range,
            vec![scan_item("b", 1), scan_item("d", 1)],
            None,
            served(),
        );
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-LIMIT violated")]
    fn a_scan_page_over_its_limit_is_an_invariant_failure() {
        let range = scan_range("a", "z", 2);
        let items = vec![scan_item("b", 1), scan_item("c", 1), scan_item("d", 1)];
        let _ = ScanPage::new(&range, items, None, served());
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-BYTES violated")]
    fn a_scan_page_over_one_mebibyte_is_an_invariant_failure() {
        // 16 items of a 1-byte key and 65,536 value bytes: 16 bytes over 1 MiB.
        let range = scan_range("a", "z", 128);
        let items: Vec<ScanItem> = ('b'..='q')
            .map(|key| scan_item(&key.to_string(), 65_536))
            .collect();
        let _ = ScanPage::new(&range, items, None, served());
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-CONTINUATION violated")]
    fn a_continuation_other_than_the_last_key_is_an_invariant_failure() {
        let range = scan_range("a", "z", 3);
        let continuation = Some(Key::parse("b".to_owned()).unwrap());
        let _ = ScanPage::new(
            &range,
            vec![scan_item("b", 1), scan_item("c", 1)],
            continuation,
            served(),
        );
    }

    #[test]
    #[should_panic(expected = "invariant SCAN-PAGE-CONTINUATION violated")]
    fn a_continuation_on_an_empty_page_is_an_invariant_failure() {
        let range = scan_range("a", "z", 3);
        let continuation = Some(Key::parse("b".to_owned()).unwrap());
        let _ = ScanPage::new(&range, Vec::new(), continuation, served());
    }

    fn key(text: &str) -> Key {
        Key::parse(text.to_owned()).unwrap()
    }

    #[test]
    fn scan_ranges_refuse_start_after_end_and_contain_correctly() {
        let limit = PageSize::DEFAULT;
        assert_eq!(
            ScanRange::new(Some(key("b")), Some(key("a")), None, limit),
            Err(ScanRangeError::StartAfterEnd),
            "rule: start > end is invalid (§8.5)"
        );
        let empty = ScanRange::new(Some(key("b")), Some(key("b")), None, limit).unwrap();
        assert!(
            !empty.contains(&key("b")),
            "rule: equal bounds give an empty page (§8.5)"
        );
        let range = ScanRange::new(Some(key("b")), Some(key("e")), Some(key("c")), limit).unwrap();
        let inside: Vec<&str> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .filter(|k| range.contains(&key(k)))
            .collect();
        assert_eq!(
            inside,
            ["d"],
            "rule: [start, end) intersected with (after, ∞) (§8.5)"
        );
        let unbounded = ScanRange::new(None, None, None, limit).unwrap();
        assert!(
            unbounded.contains(&key("\u{10ffff}")),
            "rule: omitted bounds are unbounded (§8.5)"
        );
    }

    #[test]
    fn every_core_answer_maps_to_its_code_and_boundary_codes_stay_out() {
        let served_by = ServedBy {
            node: NodeId::new(2).unwrap(),
            applied: LogIndex::new(42).unwrap(),
        };
        let at = |decision| TerminalResult::new(LogIndex::new(42).unwrap(), decision).unwrap();
        let responses = [
            Response::Written {
                result: at(Decision::Set),
                replayed: false,
                served_by,
            },
            Response::Written {
                result: at(Decision::VersionMismatch { current: None }),
                replayed: true,
                served_by,
            },
            Response::Written {
                result: at(Decision::CapacityExceeded {
                    limit: LiveStateLimit::Keys,
                }),
                replayed: false,
                served_by,
            },
            Response::Conflict {
                original_index: EntryIndex::new(3).unwrap(),
            },
            Response::NotFound { served_by },
            Response::Status {
                result: None,
                served_by,
            },
            Response::Refused(Refusal::NotLeader {
                leader: NodeId::new(1).unwrap(),
            }),
            Response::Refused(Refusal::Busy),
            Response::Refused(Refusal::Unavailable),
            Response::Refused(Refusal::Recovering {
                lifecycle: NotReadyLifecycle::Recovering,
            }),
            Response::Refused(Refusal::UnknownOutcome),
            Response::Refused(Refusal::HistoryFull {
                limit: HistoryLimit::LogEntries,
            }),
        ];
        let mut produced: Vec<ErrorCode> =
            responses.iter().filter_map(Response::error_code).collect();
        produced.sort();
        produced.dedup();
        let mut expected: Vec<ErrorCode> = ErrorCode::ALL
            .into_iter()
            .filter(|code| ![ErrorCode::InvalidRequest, ErrorCode::TooLarge].contains(code))
            .collect();
        expected.sort();
        assert_eq!(
            produced, expected,
            "rule: the core can answer every §8.4 code except the boundary's invalid_request and too_large"
        );
    }

    #[test]
    fn only_an_unknown_outcome_was_submitted() {
        let refusals = [
            Refusal::NotLeader {
                leader: NodeId::new(1).unwrap(),
            },
            Refusal::Busy,
            Refusal::Unavailable,
            Refusal::Recovering {
                lifecycle: NotReadyLifecycle::Starting,
            },
            Refusal::UnknownOutcome,
            Refusal::HistoryFull {
                limit: HistoryLimit::HistoryBytes,
            },
        ];
        for refusal in refusals {
            assert_eq!(
                refusal.submitted(),
                refusal == Refusal::UnknownOutcome,
                "rule: attempt_state is submitted only for unknown_outcome (§8.4, A-5); refusal={refusal:?}"
            );
        }
        assert_eq!(
            [
                HistoryLimit::LogEntries.as_str(),
                HistoryLimit::HistoryBytes.as_str()
            ],
            ["max_log_entries", "max_history_bytes"],
            "rule: the API's HistoryLimit enum names the config parameters"
        );
    }
}
