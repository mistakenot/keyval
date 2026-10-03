//! Probe: a catch-all *binding* arm (`other => ...`) on an enum is rejected too
//! (CD-5, `clippy::wildcard_enum_match_arm`).

/// A lifecycle like the node's (CD-2).
#[derive(Clone, Copy)]
pub enum Lifecycle {
    /// Starting.
    Starting,
    /// Recovering.
    Recovering,
    /// Ready.
    Ready,
    /// Failed.
    Failed,
}

/// Deliberate violation.
#[must_use]
pub const fn code(state: Lifecycle) -> u8 {
    match state {
        Lifecycle::Ready => 1,
        other => other as u8,
    }
}
