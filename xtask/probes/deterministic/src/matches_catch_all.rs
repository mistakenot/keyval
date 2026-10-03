//! Probe: `matches!` on a domain enum is rejected (CD-5, `clippy::disallowed_macros`).
//!
//! The macro expands to a `match` with a hidden `_ => false` arm: a catch-all that
//! `clippy::wildcard_enum_match_arm` does not see inside a macro expansion. So a variant
//! added later would silently answer `false` (br-matches-bypasses-cd5-5i0g). The
//! deterministic configuration therefore bans the macro, and the probe runner also
//! asserts that the wildcard lint stays silent here, which is the reason for the ban.

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

/// Deliberate violation: a new state would silently answer `false`.
#[must_use]
pub const fn is_ready(state: Lifecycle) -> bool {
    matches!(state, Lifecycle::Ready)
}
