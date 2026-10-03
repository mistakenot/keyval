//! Compile-fail fixture: a terminal result is built only by `TerminalResult::new`, so index 0 is unrepresentable (DM-5).

/// Deliberate violation: a result at index 0, built without `TerminalResult::new`.
#[must_use]
pub const fn forged() -> types::TerminalResult {
    types::TerminalResult {
        index: types::LogIndex::ZERO,
        decision: types::Decision::Set,
    }
}
