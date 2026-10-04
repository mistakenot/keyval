//! The core's entropy port as the patched library's mandatory entropy source (PLAN.md §7.3,
//! W02.3, P4).

use types::{Entropy, EntropyStream, invariant};

/// The core's [`Entropy`] port as `raft::Entropy`.
///
/// Every randomised election timeout, at construction and on every reset, is a draw from the
/// port's [`ElectionTimeout`](EntropyStream::ElectionTimeout) stream. A seeded port therefore
/// gives replayable timeouts (P4, T-3).
#[derive(Debug)]
pub struct EntropySource<E> {
    port: E,
}

impl<E> EntropySource<E> {
    /// The library's entropy, drawn from `port`.
    pub const fn new(port: E) -> Self {
        Self { port }
    }
}

impl<E: Entropy> raft::Entropy for EntropySource<E> {
    /// A value in `[low, high)`: `low` plus the next draw modulo the range's width.
    ///
    /// # Panics
    ///
    /// `RA-ENTROPY-RANGE` if the range is empty. The library asks only for the randomised
    /// election timeout range, which validated settings keep non-empty.
    fn gen_range(&mut self, low: usize, high: usize) -> usize {
        invariant!(
            low < high,
            "RA-ENTROPY-RANGE",
            "the library asked for a value in the empty range [{low}, {high})"
        );
        let width = u64::try_from(high.saturating_sub(low))
            .unwrap_or(u64::MAX)
            .max(1);
        let offset = self.port.next_u64(EntropyStream::ElectionTimeout) % width;
        low.saturating_add(usize::try_from(offset).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use raft::Entropy as _;

    use super::*;
    use crate::test_support::Seeded;

    #[test]
    fn draws_land_in_the_range_and_repeat_for_a_seed() {
        let draws = |seed| {
            let mut source = EntropySource::new(Seeded::new(seed));
            (0..200)
                .map(|_| source.gen_range(10, 20))
                .collect::<Vec<_>>()
        };
        let first = draws(3);
        assert!(
            first.iter().all(|value| (10..20).contains(value)),
            "rule: every draw is in [low, high); draws={first:?}"
        );
        assert_eq!(
            first,
            draws(3),
            "rule: the same seed gives the same timeouts (P4)"
        );
        assert_ne!(first, draws(4), "rule: another seed gives other timeouts");
        assert!(
            (10..20).all(|value| first.contains(&value)),
            "rule: the whole range is reachable; draws={first:?}"
        );
    }

    #[test]
    #[should_panic(
        expected = "invariant RA-ENTROPY-RANGE violated: the library asked for a value \
                               in the empty range [5, 5)"
    )]
    fn an_empty_range_breaks_its_invariant() {
        EntropySource::new(Seeded::new(1)).gen_range(5, 5);
    }
}
