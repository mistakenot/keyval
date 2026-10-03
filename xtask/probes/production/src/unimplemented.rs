//! Probe: `unimplemented!` is rejected (CD-8, `clippy::unimplemented`).

/// Deliberate violation.
pub fn probe() {
    unimplemented!()
}
