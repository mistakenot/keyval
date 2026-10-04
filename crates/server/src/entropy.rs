//! Boot-seeded entropy (PLAN.md §7.2, P4): the production entropy port.
//!
//! It is seeded once at boot from the operating system, and the seed is logged so that a
//! run's random choices (Raft's election timeouts) can be explained afterwards. Each
//! [`EntropyStream`] draws its own `SplitMix64` sequence from the seed.

use std::fs::File;
use std::io::{self, Read};

use types::{Entropy, EntropyStream};

/// A seed for one boot, read from the operating system (`/dev/urandom`).
///
/// # Errors
///
/// The operating system's error if it cannot be read.
pub fn boot_seed() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// The entropy port for one boot: one `SplitMix64` sequence per stream, each derived from
/// the boot seed, so the same seed gives the same draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootEntropy {
    /// The boot seed.
    seed: u64,
    /// The election-timeout stream's state.
    election: u64,
}

/// The election-timeout stream's tag, mixed into the seed: "ELECTION".
const ELECTION_STREAM: u64 = 0x454C_4543_5449_4F4E;

/// `SplitMix64`'s increment (the golden ratio).
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

impl BootEntropy {
    /// The entropy of a boot seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            seed,
            election: mix(seed ^ ELECTION_STREAM),
        }
    }

    /// The boot seed, for the log.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }
}

impl Entropy for BootEntropy {
    fn next_u64(&mut self, stream: EntropyStream) -> u64 {
        let state = match stream {
            EntropyStream::ElectionTimeout => &mut self.election,
        };
        *state = state.wrapping_add(GOLDEN);
        mix(*state)
    }
}

/// `SplitMix64`'s output function.
const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use types::{Entropy, EntropyStream};

    use super::{BootEntropy, boot_seed};

    fn draws(seed: u64) -> Vec<u64> {
        let mut entropy = BootEntropy::new(seed);
        (0..8)
            .map(|_| entropy.next_u64(EntropyStream::ElectionTimeout))
            .collect()
    }

    #[test]
    fn the_same_seed_gives_the_same_draws_and_another_seed_others() {
        let first = draws(42);
        let mut distinct = first.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            (
                first.clone(),
                first == draws(43),
                distinct.len(),
                BootEntropy::new(42).seed()
            ),
            (draws(42), false, first.len(), 42),
            "rule: a boot's draws follow from its seed alone, which is logged, and each draw \
             moves the stream on (§7.2, P4); draws: {first:?}"
        );
    }

    #[test]
    fn the_operating_system_gives_a_boot_seed() {
        let seeds: Vec<u64> = (0..4).map(|_| boot_seed().unwrap()).collect();
        assert!(
            seeds.windows(2).any(|pair| pair[0] != pair[1]),
            "rule: each boot reads a fresh seed from the operating system; got {seeds:?}"
        );
    }
}
