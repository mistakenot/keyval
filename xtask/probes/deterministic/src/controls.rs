//! Controls: code the deterministic-crate lint policy must accept.

use std::collections::BTreeMap;

/// A lifecycle like the node's (CD-2): exhaustive matches only (CD-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lifecycle {
    /// Starting.
    Starting,
    /// Recovering.
    Recovering,
    /// Ready.
    Ready,
    /// Failed.
    Failed,
}

/// Every variant is named, so adding one breaks the build until it is handled (CD-5).
#[must_use]
pub const fn serves_reads(state: Lifecycle) -> bool {
    match state {
        Lifecycle::Ready => true,
        Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Failed => false,
    }
}

/// Ordered maps are deterministic (§7.14); invariants use `invariant!` (CD-7).
///
/// # Panics
///
/// Panics if a key's count would overflow, which would be a bug in the caller.
#[must_use]
pub fn tally(states: &[Lifecycle]) -> BTreeMap<Lifecycle, u64> {
    let mut counts = BTreeMap::new();
    for &state in states {
        let count: &mut u64 = counts.entry(state).or_default();
        types::invariant!(
            *count < u64::MAX,
            "PROBE-COUNT-BOUNDED",
            "count for {state:?} would overflow"
        );
        *count += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_may_unwrap_expect_and_panic_in_deterministic_crates_too() {
        let counts = tally(&[Lifecycle::Ready, Lifecycle::Ready]);
        assert_eq!(counts.get(&Lifecycle::Ready).copied().unwrap(), 2);
        assert_eq!(counts.get(&Lifecycle::Ready).copied().expect("present"), 2);
        assert!(serves_reads(Lifecycle::Ready));
        assert!(!serves_reads(Lifecycle::Failed));
    }

    #[test]
    #[should_panic(expected = "tests may panic")]
    fn tests_may_panic() {
        panic!("tests may panic");
    }
}
