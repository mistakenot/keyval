//! Compile-fail fixture: a Ready is built only by `Ready::new`, which refuses entries without a sync (§7.4, CD-7).

/// Deliberate violation: new entries that skip the frame, built without `Ready::new`.
#[must_use]
pub const fn forged(entries: Vec<types::Entry>) -> types::Ready {
    types::Ready {
        entries,
        hard_state: None,
        must_sync: false,
        messages: Vec::new(),
        committed: Vec::new(),
    }
}
