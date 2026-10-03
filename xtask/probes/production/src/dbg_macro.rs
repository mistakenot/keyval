//! Probe: `dbg!` is rejected (`clippy::dbg_macro`).

/// Deliberate violation.
#[must_use]
pub fn probe(value: u8) -> u8 {
    dbg!(value)
}
