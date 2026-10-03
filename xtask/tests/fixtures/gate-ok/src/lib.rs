//! Gate fixture: formatted, lint-clean and tested.

/// Adds one.
#[must_use]
pub const fn add_one(value: u32) -> u32 {
    value + 1
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_one() {
        assert_eq!(super::add_one(1), 2);
    }

    #[test]
    fn adds_one_to_zero() {
        assert_eq!(super::add_one(0), 1);
    }
}
