//! The consensus library's configuration, as core-owned values, and the library's own
//! constraints on it (PLAN.md §7.10, D-78).
//!
//! [`ConsensusSettings`] carries exactly the fields of raft-rs 0.7's `Config` that keyval
//! sets, in core-owned types, so no library type crosses the Consensus port (CD-1). The
//! adapter (W13) copies them field by field. [`ConsensusSettings::check`] repeats every
//! check of raft-rs 0.7's `Config::validate`, in its order, so a config the parser accepts
//! never fails at `RawNode` construction; W13 proves it by constructing a node from every
//! accepted fixture config in `config/fixtures/`.

use core::fmt;

/// How the consensus library serves read-only queries: raft-rs's `ReadOnlyOption`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// Read index confirmed by a quorum heartbeat round (`ReadOnlyOption::Safe`).
    Safe,
    /// Read index trusted to the leader's lease (`ReadOnlyOption::LeaseBased`).
    LeaseBased,
}

/// The consensus library's settings for one node.
///
/// Built only from a validated [`Config`](super::Config) by
/// [`Config::consensus`](super::Config::consensus). Invariant: [`check`](Self::check)
/// passes. Pre-vote and check-quorum are on and reads are [`ReadMode::Safe`] (§7.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsensusSettings {
    /// raft-rs `id`: the node's ID.
    pub(super) node_id: u64,
    /// raft-rs `heartbeat_tick`.
    pub(super) heartbeat_tick: u64,
    /// raft-rs `election_tick`; it also drives check-quorum (§7.10).
    pub(super) election_tick: u64,
    /// raft-rs `min_election_tick`: the election timeout's inclusive minimum.
    pub(super) min_election_tick: u64,
    /// raft-rs `max_election_tick`: the election timeout's exclusive maximum.
    pub(super) max_election_tick: u64,
    /// raft-rs `max_size_per_msg`: the largest replication batch in bytes.
    pub(super) max_size_per_msg: u64,
    /// raft-rs `max_inflight_msgs`: replication batches in flight per peer.
    pub(super) max_inflight_msgs: u64,
    /// raft-rs `max_uncommitted_size`: uncommitted proposal bytes.
    pub(super) max_uncommitted_size: u64,
    /// raft-rs `max_committed_size_per_ready`: committed entry bytes per Ready.
    pub(super) max_committed_size_per_ready: u64,
    /// raft-rs `check_quorum`.
    pub(super) check_quorum: bool,
    /// raft-rs `pre_vote`.
    pub(super) pre_vote: bool,
    /// raft-rs `read_only_option`.
    pub(super) read_mode: ReadMode,
}

/// A constraint of raft-rs 0.7's `Config::validate` that a [`ConsensusSettings`] breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsensusViolation {
    /// The node ID is 0, raft-rs's `INVALID_ID`.
    ZeroNodeId,
    /// `heartbeat_tick` is 0.
    ZeroHeartbeat,
    /// `election_tick` is not above `heartbeat_tick`.
    ElectionNotAboveHeartbeat,
    /// `min_election_tick` is below `election_tick`.
    MinElectionBelowElection,
    /// `min_election_tick` is not below `max_election_tick`.
    MinElectionNotBelowMax,
    /// `max_inflight_msgs` is 0.
    ZeroInflight,
    /// Lease-based reads without check-quorum.
    LeaseReadsWithoutCheckQuorum,
    /// `max_uncommitted_size` is below `max_size_per_msg`.
    UncommittedBelowMessageSize,
}

impl fmt::Display for ConsensusViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroNodeId => "the node ID is 0",
            Self::ZeroHeartbeat => "heartbeat_tick is 0",
            Self::ElectionNotAboveHeartbeat => "election_tick is not above heartbeat_tick",
            Self::MinElectionBelowElection => "min_election_tick is below election_tick",
            Self::MinElectionNotBelowMax => "min_election_tick is not below max_election_tick",
            Self::ZeroInflight => "max_inflight_msgs is 0",
            Self::LeaseReadsWithoutCheckQuorum => "lease-based reads need check_quorum",
            Self::UncommittedBelowMessageSize => {
                "max_uncommitted_size (max_uncommitted_bytes) is below max_size_per_msg \
                 (replication_batch_bytes)"
            }
        })
    }
}

impl ConsensusSettings {
    /// Every check of raft-rs 0.7's `Config::validate`, in its order (D-78).
    ///
    /// # Errors
    ///
    /// The first [`ConsensusViolation`] found.
    pub const fn check(&self) -> Result<(), ConsensusViolation> {
        if self.node_id == 0 {
            Err(ConsensusViolation::ZeroNodeId)
        } else if self.heartbeat_tick == 0 {
            Err(ConsensusViolation::ZeroHeartbeat)
        } else if self.election_tick <= self.heartbeat_tick {
            Err(ConsensusViolation::ElectionNotAboveHeartbeat)
        } else if self.min_election_tick < self.election_tick {
            Err(ConsensusViolation::MinElectionBelowElection)
        } else if self.min_election_tick >= self.max_election_tick {
            Err(ConsensusViolation::MinElectionNotBelowMax)
        } else if self.max_inflight_msgs == 0 {
            Err(ConsensusViolation::ZeroInflight)
        } else if matches!(self.read_mode, ReadMode::LeaseBased) && !self.check_quorum {
            Err(ConsensusViolation::LeaseReadsWithoutCheckQuorum)
        } else if self.max_uncommitted_size < self.max_size_per_msg {
            Err(ConsensusViolation::UncommittedBelowMessageSize)
        } else {
            Ok(())
        }
    }

    /// raft-rs `id`: the node's ID.
    #[must_use]
    pub const fn node_id(&self) -> u64 {
        self.node_id
    }

    /// raft-rs `heartbeat_tick`.
    #[must_use]
    pub const fn heartbeat_tick(&self) -> u64 {
        self.heartbeat_tick
    }

    /// raft-rs `election_tick`.
    #[must_use]
    pub const fn election_tick(&self) -> u64 {
        self.election_tick
    }

    /// raft-rs `min_election_tick`.
    #[must_use]
    pub const fn min_election_tick(&self) -> u64 {
        self.min_election_tick
    }

    /// raft-rs `max_election_tick` (exclusive).
    #[must_use]
    pub const fn max_election_tick(&self) -> u64 {
        self.max_election_tick
    }

    /// raft-rs `max_size_per_msg`.
    #[must_use]
    pub const fn max_size_per_msg(&self) -> u64 {
        self.max_size_per_msg
    }

    /// raft-rs `max_inflight_msgs`.
    #[must_use]
    pub const fn max_inflight_msgs(&self) -> u64 {
        self.max_inflight_msgs
    }

    /// raft-rs `max_uncommitted_size`.
    #[must_use]
    pub const fn max_uncommitted_size(&self) -> u64 {
        self.max_uncommitted_size
    }

    /// raft-rs `max_committed_size_per_ready`.
    #[must_use]
    pub const fn max_committed_size_per_ready(&self) -> u64 {
        self.max_committed_size_per_ready
    }

    /// raft-rs `check_quorum`: always on (§7.10).
    #[must_use]
    pub const fn check_quorum(&self) -> bool {
        self.check_quorum
    }

    /// raft-rs `pre_vote`: always on (§7.10).
    #[must_use]
    pub const fn pre_vote(&self) -> bool {
        self.pre_vote
    }

    /// raft-rs `read_only_option`: always [`ReadMode::Safe`].
    #[must_use]
    pub const fn read_mode(&self) -> ReadMode {
        self.read_mode
    }
}
