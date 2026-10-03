//! Probe: `panic!` in production code is rejected (CD-8, `clippy::panic`).

/// Deliberate violation.
pub fn probe(fail: bool) {
    if fail {
        panic!("probe");
    }
}
