//! Probe: `todo!` is rejected (CD-8, `clippy::todo`).

/// Deliberate violation.
pub fn probe() {
    todo!()
}
