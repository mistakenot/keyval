//! Probe: the `clippy::pedantic` group is denied: `clippy::cast_possible_truncation`
//! is an error.

/// Deliberate violation.
#[must_use]
pub const fn probe(value: u64) -> u32 {
    value as u32
}
