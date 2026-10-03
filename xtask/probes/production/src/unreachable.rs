//! Probe: `unreachable!` is rejected: `invariant!` is the only sanctioned panic (CD-8,
//! `clippy::unreachable`).

/// Deliberate violation.
pub fn probe(value: u8) {
    if value > 1 {
        unreachable!("probe");
    }
}
