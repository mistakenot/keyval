//! Compile-fail fixture: a lifecycle is one state, never independent flags (CD-2, O-1).

/// Deliberate violation: a node both ready and failed.
#[must_use]
pub const fn forged() -> types::Lifecycle {
    types::Lifecycle::Ready { failed: true }
}
