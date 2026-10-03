//! Probe: the `clippy::nursery` group is denied: `clippy::missing_const_for_fn` is an
//! error.

/// Deliberate violation.
#[must_use]
pub fn probe(value: u8) -> u8 {
    value
}
