//! The inbound port ([`Inbound::step`]) and the effect values the core returns
//! (PLAN.md §7.2, §7.4).
//!
//! The core performs no I/O. It turns each [`Input`] into a list of [`Effect`]s, and the
//! composition root executes them with its adapters. Completions come back as inputs.
//!
//! **Effect order is deterministic and binding.** The root executes the effects of one
//! step in the order returned:
//! - an `Append` or `Truncate` comes before the `Sync` that covers it, and a `Sync`
//!   covers every earlier `Append` and `Truncate` of the same incarnation;
//! - a message whose persistence obligation depends on a frame is only emitted after that
//!   frame's [`Input::SyncDone`] arrived (§7.4 step 4);
//! - an attempt gets at most one `Respond`.

use crate::client::{AttemptId, ClientOp, Response};
use crate::counts::ByteCount;
use crate::ids::NodeId;
use crate::telemetry::{LogEvent, Observation};

/// One life of a node process, between a start and a crash or stop (PLAN.md §7.2,
/// §10.2). A restart is a new incarnation.
///
/// Invariant: incarnations of one node only grow; every I/O completion carries the
/// incarnation that issued it, and the core ignores one from another incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Incarnation(u64);

impl Incarnation {
    /// The first incarnation.
    pub const FIRST: Self = Self(0);

    /// An incarnation number.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The incarnation after a restart, or `None` on overflow.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Names one requested sync, so its completion can be matched (PLAN.md §7.2).
///
/// Invariant: a node's sync sequence numbers strictly increase within an incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncSeq(u64);

impl SyncSeq {
    /// A sync sequence number.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next sequence number, or `None` on overflow.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// An event the core is driven by (the inbound port, §7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A client attempt, already parsed (client API port).
    Client {
        /// The attempt's ID, echoed in its `Respond`.
        attempt: AttemptId,
        /// What it asks for.
        op: ClientOp,
    },
    /// A peer message (peer transport port). The bytes are an encoded peer envelope; the
    /// core validates it before Raft sees it (§8.2).
    Peer {
        /// The sending node, as the transport reports it.
        from: NodeId,
        /// The encoded envelope, bounded by the transport.
        bytes: Vec<u8>,
    },
    /// One logical clock step (clock port).
    Tick,
    /// A requested sync completed (byte storage port). Never dropped by admission bounds
    /// (§7.11).
    SyncDone {
        /// The sync it completes.
        seq: SyncSeq,
        /// The incarnation that requested it.
        incarnation: Incarnation,
    },
}

impl Input {
    /// Whether this is a completion requested by an incarnation other than `current`,
    /// which the core must ignore (§7.2, B-17).
    #[must_use]
    pub fn is_stale_completion(&self, current: Incarnation) -> bool {
        match self {
            Self::SyncDone { incarnation, .. } => *incarnation != current,
            Self::Client { .. } | Self::Peer { .. } | Self::Tick => false,
        }
    }
}

/// Work the core asks the composition root to do (effect ports, §7.2), in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Send an encoded peer envelope to a node (peer transport).
    Send {
        /// The destination node.
        to: NodeId,
        /// The encoded envelope.
        bytes: Vec<u8>,
    },
    /// Append one encoded journal frame (byte storage, §7.6).
    Append {
        /// The frame's bytes, at most one frame (≤ 1 MiB, §7.11).
        frame: Vec<u8>,
    },
    /// Truncate the journal to `len` bytes, discarding a torn tail (DUR-6). Relied on only
    /// after its own sync completes.
    Truncate {
        /// The length to keep.
        len: ByteCount,
    },
    /// Make every earlier append and truncate durable, then report
    /// [`Input::SyncDone`] with this `seq` and `incarnation`.
    Sync {
        /// The sync's sequence number.
        seq: SyncSeq,
        /// The incarnation requesting it, echoed in the completion.
        incarnation: Incarnation,
    },
    /// Answer an attempt (client API).
    Respond {
        /// The attempt answered.
        attempt: AttemptId,
        /// The answer.
        response: Response,
    },
    /// Write a structured log record (telemetry, O-3).
    Log(LogEvent),
    /// Record an observation for the canonical trace and the checkers (telemetry, §10.2).
    Observe(Observation),
}

/// The inbound port: the deterministic core's single entry point (PLAN.md §7.2).
///
/// An implementation is deterministic: the same inputs from the same state give the same
/// effects in the same order (T-3).
pub trait Inbound {
    /// Handles one input and returns the effects to execute, in order.
    fn step(&mut self, input: Input) -> Vec<Effect>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_from_another_incarnation_are_stale() {
        let current = Incarnation::new(3);
        let fresh = Input::SyncDone {
            seq: SyncSeq::new(7),
            incarnation: current,
        };
        let old = Input::SyncDone {
            seq: SyncSeq::new(7),
            incarnation: Incarnation::new(2),
        };
        assert!(
            !fresh.is_stale_completion(current),
            "rule: this incarnation's completion counts (§7.2)"
        );
        assert!(
            old.is_stale_completion(current),
            "rule: an earlier incarnation's completion is ignored (§7.2, B-17)"
        );
        assert!(
            !Input::Tick.is_stale_completion(current),
            "rule: only completions can be stale"
        );
    }

    #[test]
    fn incarnations_and_sync_sequences_only_grow() {
        assert_eq!(
            Incarnation::FIRST.checked_next(),
            Some(Incarnation::new(1)),
            "rule: a restart is the next incarnation"
        );
        assert_eq!(
            Incarnation::new(u64::MAX).checked_next(),
            None,
            "rule: no wrap-around (CD-4)"
        );
        assert_eq!(
            SyncSeq::new(9).checked_next(),
            Some(SyncSeq::new(10)),
            "rule: sync sequence numbers increase"
        );
        assert_eq!(
            SyncSeq::new(u64::MAX).checked_next(),
            None,
            "rule: no wrap-around (CD-4)"
        );
    }

    /// A trivial inbound implementation: the port needs no I/O or library types.
    struct Echo;

    impl Inbound for Echo {
        fn step(&mut self, input: Input) -> Vec<Effect> {
            match input {
                Input::Tick => vec![Effect::Truncate {
                    len: ByteCount::ZERO,
                }],
                Input::Client { .. } | Input::Peer { .. } | Input::SyncDone { .. } => Vec::new(),
            }
        }
    }

    #[test]
    fn the_inbound_port_is_a_plain_trait() {
        let effects = Echo.step(Input::Tick);
        assert_eq!(
            effects,
            vec![Effect::Truncate {
                len: ByteCount::ZERO
            }],
            "rule: step(input) -> effects, returned as values (§7.2)"
        );
    }
}
