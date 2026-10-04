//! SCAN over the applied state (PLAN.md §8.5, §7.11, F-4, DM-4; leaf area
//! `statemachine/scan`, W28.1).
//!
//! A page is read from the state machine as it stands: one applied prefix, which the page
//! reports in `served_by` (C-3, C-8). The read path answers a SCAN within one turn, so no
//! entry is applied while a page is read.
//!
//! - **Range:** the keys in `[start, end)` in byte-wise order of their UTF-8 encoding
//!   (DM-4, which is [`Key`]'s own order), after `after` when given. Omitted bounds are
//!   open. `after` is exclusive and intersected with the range, so `after ≥ end` gives an
//!   empty page.
//! - **Accumulation (§7.11), exact:** keys are taken in order while the page holds fewer
//!   than `limit` items and the next item's decoded key+value bytes keep the page within
//!   [`MAX_SCAN_PAGE_BYTES`]. The page stops before the first item that would exceed
//!   either cap; it never skips ahead to a smaller one. The first item is always taken,
//!   and one maximum-size item (256 + 65,536 bytes) always fits.
//! - **Continuation:** the next in-range key is looked at only to learn whether one exists
//!   (the probe). Its bytes never count, and neither do `next_after`'s. When it exists,
//!   `has_more` is true and `next_after` is the last returned key; otherwise there is no
//!   continuation.

use std::ops::Bound;

use types::{Key, MAX_SCAN_PAGE_BYTES, ScanItem, ScanPage, ScanRange, ServedBy, Value, invariant};

use crate::StateMachine;

impl StateMachine {
    /// One SCAN page of `range` from this state (§8.5), answered by `served_by`.
    ///
    /// # Panics
    ///
    /// `SM-SCAN-PREFIX` if `served_by` reports an applied index other than this state's:
    /// a page names exactly the prefix it was read from (C-3). [`ScanPage::new`] asserts
    /// the page's own invariants.
    #[must_use]
    pub fn scan(&self, range: &ScanRange, served_by: ServedBy) -> ScanPage {
        invariant!(
            served_by.applied == self.applied_index(),
            "SM-SCAN-PREFIX",
            "node {}: a page answered at applied index {} is read from applied index {}",
            served_by.node,
            served_by.applied,
            self.applied_index()
        );
        let limit = range.limit().get();
        let mut items: Vec<ScanItem> = Vec::new();
        let mut bytes = 0_u64;
        let mut more = false;
        for (key, stored) in self.keys_from(lower(range)) {
            if range.end().is_some_and(|end| key >= end) {
                break;
            }
            let size = item_bytes(key, stored.value());
            let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
            let full =
                count >= limit || (count > 0 && bytes.saturating_add(size) > MAX_SCAN_PAGE_BYTES);
            if full {
                // The probe: an in-range key beyond the page. Its bytes never count.
                more = true;
                break;
            }
            bytes = bytes.saturating_add(size);
            items.push(ScanItem {
                key: key.clone(),
                value: stored.value().clone(),
                version: stored.version(),
            });
        }
        let continuation = if more {
            items.last().map(|item| item.key.clone())
        } else {
            None
        };
        ScanPage::new(range, items, continuation, served_by)
    }
}

/// Where a page starts: after `after` when it is at or past `start`, else at `start`.
fn lower(range: &ScanRange) -> Bound<&Key> {
    match (range.start(), range.after()) {
        (Some(start), Some(after)) => {
            if after < start {
                Bound::Included(start)
            } else {
                Bound::Excluded(after)
            }
        }
        (None, Some(after)) => Bound::Excluded(after),
        (Some(start), None) => Bound::Included(start),
        (None, None) => Bound::Unbounded,
    }
}

/// An item's decoded size, as §7.11 counts it: its key's and value's UTF-8 bytes.
fn item_bytes(key: &Key, value: &Value) -> u64 {
    u64::try_from(key.byte_len().saturating_add(value.byte_len())).unwrap_or(u64::MAX)
}
