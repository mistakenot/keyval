//! Compile-fail fixture: an entry index is built only by `EntryIndex::new`, so index 0 is unrepresentable (§8.1 EntryIndex, F-6).

/// Deliberate violation: an original index of 0, built without `EntryIndex::new`.
#[must_use]
pub const fn forged() -> types::EntryIndex {
    types::EntryIndex(0)
}
