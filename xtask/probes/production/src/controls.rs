//! Controls: code the production-tier lint policy must accept.

/// Uses the one sanctioned panic path, an invariant assertion (CD-7).
///
/// # Panics
///
/// Panics if `index` exceeds `limit`; that would be a bug in the caller.
#[must_use]
pub fn remaining(index: u64, limit: u64) -> u64 {
    types::invariant!(
        index <= limit,
        "PROBE-INDEX-LE-LIMIT",
        "index {index} exceeds limit {limit}"
    );
    limit - index
}

#[cfg(test)]
mod tests {
    #[test]
    fn tests_may_unwrap_and_expect() {
        let value = std::hint::black_box(Some(3_u8));
        assert_eq!(value.unwrap(), 3);
        assert_eq!(value.expect("present"), 3);
    }

    #[test]
    #[should_panic(expected = "tests may panic")]
    fn tests_may_panic() {
        panic!("tests may panic");
    }

    #[test]
    fn invariant_that_holds_passes() {
        assert_eq!(super::remaining(2, 5), 3);
    }

    #[test]
    #[should_panic(expected = "invariant PROBE-INDEX-LE-LIMIT violated: index 6 exceeds limit 5")]
    fn invariant_violation_panics_with_context() {
        let _ = super::remaining(6, 5);
    }
}
