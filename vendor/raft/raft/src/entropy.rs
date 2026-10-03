// keyval (W02.3, PLAN.md §7.3, D-57): the determinism patch. Not part of upstream raft-rs.

//! Randomness for the randomized election timeout, supplied by the embedding application.
//!
//! Upstream raft-rs 0.7.0 draws every election timeout from `rand::thread_rng()`, which is
//! seeded by the operating system and so differs on every run. This crate instead takes a
//! mandatory [`Entropy`] source in [`Raft::new`](crate::Raft::new) and
//! [`RawNode::new`](crate::RawNode::new), and draws from nothing else. There is no ambient
//! default: an embedding that wants reproducible runs supplies a seeded source, and one
//! that wants operating-system randomness wraps it in this trait itself.
//!
//! The source is consulted only by `reset_randomized_election_timeout`, at construction and
//! on every `reset` (each change of role or term). It never influences vote,
//! log-matching, quorum or commit rules.

/// A source of randomness for Raft's randomized election timeout.
pub trait Entropy {
    /// Returns a value in `low..high`, ideally uniformly distributed. `Config::validate`
    /// guarantees `low < high`; a value outside the range is a broken source and panics.
    fn gen_range(&mut self, low: usize, high: usize) -> usize;
}

impl<E: Entropy + ?Sized> Entropy for Box<E> {
    fn gen_range(&mut self, low: usize, high: usize) -> usize {
        (**self).gen_range(low, high)
    }
}

/// A small seeded generator (SplitMix64) for tests, examples and benches.
///
/// The same seed always yields the same sequence, in every process and on every platform.
/// It is not cryptographic. Its modulo reduction has a bias of at most
/// `(high - low) / 2^64`, which is negligible for election timeouts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator starting from `seed`.
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    /// The next 64 pseudo-random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl Entropy for SplitMix64 {
    fn gen_range(&mut self, low: usize, high: usize) -> usize {
        assert!(low < high, "empty range {}..{}", low, high);
        let span = (high - low) as u64;
        low + (self.next_u64() % span) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_sequence() {
        let mut a = SplitMix64::new(7);
        let mut b = SplitMix64::new(7);
        let xs: Vec<usize> = (0..64).map(|_| a.gen_range(10, 20)).collect();
        let ys: Vec<usize> = (0..64).map(|_| b.gen_range(10, 20)).collect();
        assert_eq!(xs, ys);
        assert!(xs.iter().all(|x| (10..20).contains(x)), "{:?}", xs);
        let mut c = SplitMix64::new(8);
        let zs: Vec<usize> = (0..64).map(|_| c.gen_range(10, 20)).collect();
        assert_ne!(xs, zs, "different seeds give different sequences");
    }

    #[test]
    fn the_reference_vector_is_stable() {
        // SplitMix64 from seed 0 (Steele, Lea and Flood 2014; Vigna's reference code).
        let mut g = SplitMix64::new(0);
        assert_eq!(g.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(g.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(g.next_u64(), 0x06C4_5D18_8009_454F);
    }
}
