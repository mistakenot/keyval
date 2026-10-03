//! Probe: the `clippy::all` group is denied, not warned: a default-warn style lint
//! (`clippy::needless_return`) is an error.

/// Deliberate violation.
#[must_use]
pub const fn probe(value: u8) -> u8 {
    return value;
}
