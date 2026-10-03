//! UBS gate fixture: production code that never panics, with unit tests that do.

pub mod halve;

/// Doubles a number, saturating at `u32::MAX`.
#[must_use]
pub const fn double(value: u32) -> u32 {
    value.saturating_mul(2)
}

#[cfg(test)]
mod tests {
    use super::double;

    #[test]
    fn doubles() {
        if double(2) != 4 {
            panic!("rule: 2 doubled is 4; observed={}", double(2));
        }
    }
}
