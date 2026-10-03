//! Fixture for `./dev check`: known passing, failing and ignored libtest cases.

/// A doctest whose name contains `passes_`: `./dev check` must never select it, because
/// rustdoc splits test arguments on whitespace and doctest names contain spaces.
///
/// ```
/// assert_eq!(libtest_fixture::passes_doc(), 1);
/// ```
#[must_use]
pub const fn passes_doc() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn passes_one() {}

    #[test]
    fn passes_two() {}

    #[test]
    fn fails_on_purpose() {
        let (observed, expected) = (1 + 1, 3);
        assert_eq!(
            observed, expected,
            "deliberate fixture failure: proves ./dev check reports failing cases"
        );
    }

    #[test]
    #[ignore = "fixture: proves a selected #[ignore] case fails the check"]
    fn ignored_on_purpose() {}
}
