//! Probe: `expect` in production code is rejected (CD-8, `clippy::expect_used`).

/// Deliberate violation.
#[must_use]
pub fn probe(value: Option<u8>) -> u8 {
    value.expect("probe")
}
