//! UBS gate fixture: a `panic!` in production code, which CD-8 forbids.

/// Halves an even number.
#[must_use]
pub fn halve(value: u32) -> u32 {
    if value % 2 != 0 {
        panic!("odd input {value}");
    }
    value / 2
}
