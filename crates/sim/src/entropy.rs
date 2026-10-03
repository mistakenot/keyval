//! Seeded randomness for the simulator (PLAN.md §10.2).
//!
//! A run has one master seed. Every consumer of randomness draws from its own stream,
//! split from that seed, so drawing from one stream never shifts another:
//!
//! - one stream per [`Subsystem`]: clients, network, disk, faults and the scheduler;
//! - one [`NodeStreams`] entropy port per node and incarnation, so a restarted node draws
//!   fresh randomness and no other node's draws move.
//!
//! Observing a run never consumes protocol randomness. Streams are values handed to the
//! component that owns them; nothing that only observes a run (the trace, logs,
//! checkers) is ever given one.
//!
//! The generator is `SplitMix64` (Steele, Lea and Flood, 2014), pinned by golden
//! vectors: it is part of what "same build" means for byte-identical traces (T-3), as are
//! the domain tags below, which therefore never change.

use types::{Entropy, EntropyStream, Incarnation, NodeId};

/// The `SplitMix64` generator: 64 bits of state, 64 random bits per step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator starting from `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A part of the simulator that draws randomness, each from its own stream (§10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subsystem {
    /// Client request arrivals and choices.
    Clients,
    /// Network delay, loss, duplication and reordering.
    Network,
    /// Simulated disk latency and failures.
    Disk,
    /// Fault injection (the nemesis).
    Faults,
    /// The scheduler's tie priorities between simultaneous events.
    Scheduler,
}

impl Subsystem {
    /// This subsystem's domain tag in the stream derivation.
    const fn tag(self) -> u64 {
        match self {
            Self::Clients => 1,
            Self::Network => 2,
            Self::Disk => 3,
            Self::Faults => 4,
            Self::Scheduler => 5,
        }
    }
}

/// The domain tag of the per-node streams, distinct from every subsystem's tag.
const NODE_TAG: u64 = 0x100;

/// The domain tag of one entropy-port stream within a node incarnation.
const fn port_tag(stream: EntropyStream) -> u64 {
    match stream {
        EntropyStream::ElectionTimeout => 1,
    }
}

/// A stream seed derived from `master` and a path of domain words. Each word is mixed in
/// through a fresh generator step, so distinct paths give unrelated streams.
fn derive(master: u64, path: &[u64]) -> u64 {
    path.iter()
        .fold(SplitMix64::new(master), |mut generator, word| {
            SplitMix64::new(generator.next_u64() ^ word)
        })
        .next_u64()
}

/// A run's master seed, from which every stream is split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterSeed(u64);

impl MasterSeed {
    /// The master seed `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The seed's value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The stream of `subsystem`.
    #[must_use]
    pub fn stream(self, subsystem: Subsystem) -> SplitMix64 {
        SplitMix64::new(derive(self.0, &[subsystem.tag()]))
    }

    /// The entropy port of `node` in `incarnation`.
    #[must_use]
    pub fn node(self, node: NodeId, incarnation: Incarnation) -> NodeStreams {
        let port = |stream| {
            SplitMix64::new(derive(
                self.0,
                &[NODE_TAG, node.get(), incarnation.get(), port_tag(stream)],
            ))
        };
        NodeStreams {
            election_timeout: port(EntropyStream::ElectionTimeout),
        }
    }
}

/// One node incarnation's entropy port (§7.2): a separate stream per
/// [`EntropyStream`], so one consumer's draws never shift another's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeStreams {
    election_timeout: SplitMix64,
}

impl Entropy for NodeStreams {
    fn next_u64(&mut self, stream: EntropyStream) -> u64 {
        match stream {
            EntropyStream::ElectionTimeout => self.election_timeout.next_u64(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Subsystem; 5] = [
        Subsystem::Clients,
        Subsystem::Network,
        Subsystem::Disk,
        Subsystem::Faults,
        Subsystem::Scheduler,
    ];

    fn draws(mut generator: SplitMix64, count: usize) -> Vec<u64> {
        (0..count).map(|_| generator.next_u64()).collect()
    }

    fn port_draws(mut port: NodeStreams, count: usize) -> Vec<u64> {
        (0..count)
            .map(|_| port.next_u64(EntropyStream::ElectionTimeout))
            .collect()
    }

    fn node(raw: u64) -> NodeId {
        NodeId::new(raw).unwrap()
    }

    #[test]
    fn splitmix64_matches_its_reference_vectors() {
        // Independently computed from the published algorithm (Steele, Lea and Flood).
        assert_eq!(
            draws(SplitMix64::new(0), 3),
            [
                0xE220_A839_7B1D_CDAF,
                0x6E78_9E6A_A1B9_65F4,
                0x06C4_5D18_8009_454F
            ],
            "rule: the PRNG is part of the build identity (T-3)"
        );
        assert_eq!(
            draws(SplitMix64::new(1_234_567), 5),
            [
                0x599E_D017_FB08_FC85,
                0x2C73_F084_5854_0FA5,
                0x883E_BCE5_A3F2_7C77,
                0x3FBE_F740_E917_7B3F,
                0xE3B8_3467_08CB_5ECD,
            ]
        );
    }

    #[test]
    fn each_subsystem_draws_from_its_own_stream() {
        let seed = MasterSeed::new(42);
        for (index, first) in ALL.iter().enumerate() {
            for second in ALL.iter().skip(index + 1) {
                assert_ne!(
                    draws(seed.stream(*first), 8),
                    draws(seed.stream(*second), 8),
                    "rule: {first:?} and {second:?} have independent streams"
                );
            }
        }
    }

    #[test]
    fn drawing_from_one_stream_never_shifts_another() {
        let seed = MasterSeed::new(7);
        let network = draws(seed.stream(Subsystem::Network), 16);
        let node_draws = port_draws(seed.node(node(2), Incarnation::new(1)), 16);
        let mut clients = seed.stream(Subsystem::Clients);
        let mut port = seed.node(node(1), Incarnation::new(1));
        for _ in 0..1_000 {
            clients.next_u64();
            port.next_u64(EntropyStream::ElectionTimeout);
        }
        assert_eq!(draws(seed.stream(Subsystem::Network), 16), network);
        assert_eq!(
            port_draws(seed.node(node(2), Incarnation::new(1)), 16),
            node_draws,
            "rule: one node's draws never move another node's (§10.2)"
        );
    }

    #[test]
    fn a_restart_draws_fresh_node_entropy() {
        let seed = MasterSeed::new(9);
        let first = port_draws(seed.node(node(1), Incarnation::new(1)), 8);
        assert_eq!(
            port_draws(seed.node(node(1), Incarnation::new(1)), 8),
            first,
            "rule: the same node incarnation always gets the same stream"
        );
        assert_ne!(
            port_draws(seed.node(node(1), Incarnation::new(2)), 8),
            first,
            "rule: streams are split per incarnation"
        );
        assert_ne!(
            port_draws(seed.node(node(2), Incarnation::new(1)), 8),
            first,
            "rule: streams are split per node"
        );
        for subsystem in ALL {
            assert_ne!(draws(seed.stream(subsystem), 8), first);
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_streams_and_another_seed_does_not() {
        for subsystem in ALL {
            assert_eq!(
                draws(MasterSeed::new(5).stream(subsystem), 8),
                draws(MasterSeed::new(5).stream(subsystem), 8)
            );
            assert_ne!(
                draws(MasterSeed::new(5).stream(subsystem), 8),
                draws(MasterSeed::new(6).stream(subsystem), 8)
            );
        }
        assert_eq!(MasterSeed::new(5).get(), 5);
    }
}
