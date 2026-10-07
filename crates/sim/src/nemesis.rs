//! The fault vocabulary of PLAN.md §9.1 (W37.1): every fault the simulator can inject, as
//! plain values the cluster schedules as events.
//!
//! A [`Fault`] is data. [`crate::Cluster::inject_at`] schedules it, so what a fault does and
//! when is part of the run's one event queue, and a driver slicing the run into
//! `run_until` steps cannot change it (P4). Nothing here draws from a random stream: a
//! fault that is seeded (the unsynced bytes a crash keeps) is chosen by the cluster's disk
//! stream when it fires, or fixed by naming a [`Survival`].
//!
//! The vocabulary, row by row:
//!
//! | §9.1 row | value |
//! |---|---|
//! | crash and restart, with a prefix, a garbage tail or a stale completion | [`Fault::Crash`] with a [`Survival`], armed at a [`CrashPoint`] by [`crate::Cluster::arm`]; [`Fault::Restart`] |
//! | all-node crash | [`Fault::CrashAll`] |
//! | permanent loss of up to two nodes | [`Fault::Lose`], refused beyond [`MAX_LOST`] |
//! | symmetric and asymmetric partitions, directed cycles | [`Fault::Partition`], [`Fault::Heal`], [`Fault::Cut`], [`Fault::Mend`] |
//! | loss (client responses too), delay and reordering, duplication | [`Fault::Profiles`] |
//! | a node paused past election timeouts | [`Fault::Pause`], [`Fault::Resume`] |

use std::fmt;

use types::NodeId;

use crate::disk::Survival;
use crate::net::LinkProfile;

/// The most nodes that may be lost for good (§9.1): a quorum of the five voters survives.
pub const MAX_LOST: usize = 2;

/// Where an armed crash fires: relative to the node's next sync becoming durable, or its
/// next client response.
///
/// A crash point is *state-triggered*: it waits for that moment in the run instead of a
/// wall of virtual time, so a scenario can name the interesting instant rather than search
/// for a time that lands on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Before the sync runs: every appended byte since the last sync is unsynced.
    BeforeSync,
    /// After the sync is durable but before its completion reaches the node: the
    /// completion arrives later, at a restarted incarnation, as a stale one.
    AfterSync,
    /// When the node is about to send a client response: the write it answers is committed
    /// and applied, and the reply is never sent (crash after commit, before the reply).
    BeforeReply,
}

/// A partition of the voters into groups that reach only their own members, or an explicit
/// set of directed links to cut.
///
/// Invariant: every group is non-empty and no node is in two groups; a cut link joins two
/// different nodes. Built only by the constructors, which refuse anything else (CD-2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Partition {
    /// Disjoint groups; every link between two groups is cut both ways, and nodes in no
    /// group keep the links they have.
    Groups(Vec<Vec<NodeId>>),
    /// Directed links `(from, to)` to cut, in both senses only if both are listed.
    Cuts(Vec<(NodeId, NodeId)>),
}

impl Partition {
    /// Disjoint `groups`, as in a symmetric 3/2, 4/1 or 2/2/1 split.
    ///
    /// # Errors
    ///
    /// [`NemesisError::EmptyGroup`] or [`NemesisError::OverlappingGroups`].
    pub fn symmetric(groups: &[&[NodeId]]) -> Result<Self, NemesisError> {
        let mut seen: Vec<NodeId> = Vec::new();
        for group in groups {
            if group.is_empty() {
                return Err(NemesisError::EmptyGroup);
            }
            for node in *group {
                if seen.contains(node) {
                    return Err(NemesisError::OverlappingGroups { node: *node });
                }
                seen.push(*node);
            }
        }
        Ok(Self::Groups(
            groups.iter().map(|group| group.to_vec()).collect(),
        ))
    }

    /// A directed cycle `nodes[0] → nodes[1] → … → nodes[0]`: only the links along the cycle
    /// stay open, and every other directed link among `nodes` is cut. Each node hears only
    /// its predecessor, so no node can be answered by the one it sends to.
    ///
    /// # Errors
    ///
    /// [`NemesisError::ShortCycle`] for fewer than three nodes, or
    /// [`NemesisError::OverlappingGroups`] for a repeated node.
    pub fn directed_cycle(nodes: &[NodeId]) -> Result<Self, NemesisError> {
        if nodes.len() < 3 {
            return Err(NemesisError::ShortCycle { len: nodes.len() });
        }
        for (i, node) in nodes.iter().enumerate() {
            if nodes[..i].contains(node) {
                return Err(NemesisError::OverlappingGroups { node: *node });
            }
        }
        let mut cuts = Vec::new();
        for (i, from) in nodes.iter().enumerate() {
            let next = nodes[(i + 1) % nodes.len()];
            for to in nodes {
                if to != from && *to != next {
                    cuts.push((*from, *to));
                }
            }
        }
        Ok(Self::Cuts(cuts))
    }

    /// The directed links `(from, to)` to cut: every listed pair, which must join two
    /// different nodes.
    ///
    /// # Errors
    ///
    /// [`NemesisError::SelfLink`] for a pair that joins a node to itself.
    pub fn cuts(links: &[(NodeId, NodeId)]) -> Result<Self, NemesisError> {
        for (from, to) in links {
            if from == to {
                return Err(NemesisError::SelfLink { node: *from });
            }
        }
        Ok(Self::Cuts(links.to_vec()))
    }
}

/// One injectable fault (§9.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// `node` crashes, keeping `survival` of its unsynced appended bytes. It does nothing
    /// if the node is down.
    Crash {
        /// The node.
        node: NodeId,
        /// What its disk keeps.
        survival: Survival,
    },
    /// `node` is armed to crash at `point`, keeping `survival` of its unsynced bytes, as
    /// [`crate::Cluster::arm`] does. Arming is not a fault that happened: the crash is,
    /// when it fires (once, or never if the node crashes, restarts or is disarmed first).
    /// It does nothing if the node is down or lost.
    Arm {
        /// The node.
        node: NodeId,
        /// Where the crash fires.
        point: CrashPoint,
        /// What its disk keeps.
        survival: Survival,
    },
    /// Every running node crashes at once, each keeping `survival` (all-node crash).
    CrashAll {
        /// What each disk keeps.
        survival: Survival,
    },
    /// `node` restarts as its next incarnation from the bytes its disk kept.
    Restart {
        /// The node.
        node: NodeId,
    },
    /// `node` crashes and never returns: its restarts are ignored from now on. At most
    /// [`MAX_LOST`] nodes can be lost.
    Lose {
        /// The node.
        node: NodeId,
    },
    /// `node` stalls: it takes no input and ticks no clock until [`Fault::Resume`]. What
    /// arrives meanwhile is held, up to a bound, and given to it in order on resumption.
    Pause {
        /// The node.
        node: NodeId,
    },
    /// `node` runs again, and takes what was held.
    Resume {
        /// The node.
        node: NodeId,
    },
    /// The links the partition names are cut; earlier cuts stay.
    Partition(Partition),
    /// The one directed link `from → to` is cut.
    Cut {
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
    },
    /// The one directed link `from → to` is restored.
    Mend {
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
    },
    /// Every link is restored.
    Heal,
    /// The peer and client path profiles change: delay (a range makes messages overtake one
    /// another), loss and duplication. Client responses use the client profile.
    Profiles {
        /// Node-to-node links.
        peer: LinkProfile,
        /// Paths to and from clients.
        client: LinkProfile,
    },
}

/// Why a fault could not be built or scheduled. Always a misdriven scenario, never a
/// finding about the system under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NemesisError {
    /// A partition group was empty.
    EmptyGroup,
    /// A node appears twice in a partition or a cycle.
    OverlappingGroups {
        /// The node.
        node: NodeId,
    },
    /// A directed cycle needs at least three nodes.
    ShortCycle {
        /// The nodes given.
        len: usize,
    },
    /// A cut link joined a node to itself.
    SelfLink {
        /// The node.
        node: NodeId,
    },
    /// Losing another node would pass [`MAX_LOST`].
    TooManyLost {
        /// The node whose loss was refused.
        node: NodeId,
    },
    /// The fault names a node outside the cluster.
    UnknownNode {
        /// The node.
        node: NodeId,
    },
}

impl fmt::Display for NemesisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => f.write_str("a partition group is empty"),
            Self::OverlappingGroups { node } => write!(f, "node {node} is named twice"),
            Self::ShortCycle { len } => {
                write!(f, "a directed cycle needs at least 3 nodes, got {len}")
            }
            Self::SelfLink { node } => write!(f, "a link joins node {node} to itself"),
            Self::TooManyLost { node } => write!(
                f,
                "losing node {node} would pass the limit of {MAX_LOST} lost nodes"
            ),
            Self::UnknownNode { node } => write!(f, "node {node} is not in the cluster"),
        }
    }
}

impl std::error::Error for NemesisError {}
