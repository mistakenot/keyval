//! Compile-fail fixture: an attempt cannot be both submitted and terminal (CD-2, §7.8).

/// Deliberate violation: a submitted attempt that also carries a result.
#[must_use]
pub const fn forged(result: types::TerminalResult) -> types::Attempt {
    types::Attempt::Submitted(result)
}
