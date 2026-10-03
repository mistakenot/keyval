//! Compile-fail fixture: a SCAN range is built only by `ScanRange::new`, so start > end is unrepresentable (§8.5).

/// Deliberate violation: an inverted range, built without `ScanRange::new`.
#[must_use]
pub const fn forged(start: types::Key, end: types::Key) -> types::ScanRange {
    types::ScanRange {
        start: Some(end),
        end: Some(start),
        after: None,
        limit: types::PageSize::DEFAULT,
    }
}
