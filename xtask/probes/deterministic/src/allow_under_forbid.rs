//! Probe: in a deterministic crate, an `allow` cannot override a forbidden CD
//! lint (error E0453, §11.5 rule 11).

/// Deliberate violation.
#[allow(clippy::unwrap_used)]
#[must_use]
pub fn probe(value: Option<u8>) -> u8 {
    value.unwrap()
}
