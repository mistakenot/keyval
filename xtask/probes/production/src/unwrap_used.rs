//! Probe: `unwrap` in production code is rejected (CD-8, `clippy::unwrap_used`).

/// Deliberate violation.
#[must_use]
pub fn probe(value: Option<u8>) -> u8 {
    value.unwrap()
}
