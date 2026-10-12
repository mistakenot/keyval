//! The certified drain (PLAN.md §10.2, D-53; W43.2): how a DM-7 history-capacity heal knows
//! the cluster has settled without writing a catch-up marker, which a full history would
//! refuse.
//!
//! A drain is certified when, for the whole quiet period ([`QUIET_TICKS`] by default):
//! - no client request is open on any client (clients stopped, every write parked), and the
//!   quiet period outlasts the longest chaos delay, so no packet from before it can still
//!   arrive (peer heartbeats make an in-flight packet count unusable; the run asserts the
//!   bound instead);
//! - every recoverable node is up, in one term, with exactly one leader among them, which
//!   the others follow;
//! - on every recoverable node last log index = commit index = applied index, all the same
//!   index, with one state digest (the map and the request history).
//!
//! Then no uncommitted entry exists anywhere that could commit later, and a status lookup
//! answering `unknown` on every recoverable node proves the write never executed and never
//! can (the request history never expires, §10.2).
//!
//! [`Sample::judge`] decides one look at the cluster; [`QuietClock`] decides whether the
//! judged looks held for the quiet period and, only then, hands out a [`Certified`] — the
//! one value the status-lookup resolution accepts, so a lookup before the quiet period has
//! passed cannot be written (CD-2). Nothing here draws randomness or reads a clock: the
//! looks carry the virtual time.
//!
//! **Bounds (CD-9).** A sample holds one entry per voter, at most five.

use statemachine::StateDigest;
use types::{LogIndex, NodeId, Role, Term};

use crate::cluster::Cluster;
use crate::scheduler::VirtualTime;

/// The default quiet period, in ticks (§10.2).
pub const QUIET_TICKS: u64 = 100;

/// One recoverable node as one look saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeSample {
    /// The node.
    pub node: NodeId,
    /// Its role.
    pub role: Role,
    /// Its term.
    pub term: Term,
    /// The leader it believes in.
    pub leader: Option<NodeId>,
    /// Its last log index.
    pub last: LogIndex,
    /// Its commit index.
    pub commit: LogIndex,
    /// Its applied index.
    pub applied: LogIndex,
    /// Its state digest.
    pub digest: StateDigest,
    /// The client attempts and peer envelopes waiting in its queue.
    pub queued: usize,
}

/// One look at the cluster: the clients' work in flight and every recoverable node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Operations clients are waiting on.
    pub client_work: usize,
    /// The recoverable nodes that are running, in node order.
    pub nodes: Vec<NodeSample>,
    /// The recoverable nodes that are not running (crashed or paused), in node order.
    pub down: Vec<NodeId>,
}

/// Why a look does not show a drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uncertified {
    /// A client is waiting on an operation.
    ClientWork {
        /// How many.
        in_flight: usize,
    },
    /// A recoverable node is not running.
    NodeDown {
        /// The node.
        node: NodeId,
    },
    /// A node still has queued inputs.
    Queued {
        /// The node.
        node: NodeId,
        /// How many.
        queued: usize,
    },
    /// No recoverable node leads.
    NoLeader,
    /// More than one recoverable node leads.
    TwoLeaders {
        /// The two lowest-numbered leaders.
        nodes: [NodeId; 2],
    },
    /// A node is in a different term from the leader.
    Term {
        /// The node.
        node: NodeId,
    },
    /// A follower does not follow the leader.
    Follows {
        /// The node.
        node: NodeId,
    },
    /// A node's last, commit and applied indexes are not all equal.
    Indexes {
        /// The node.
        node: NodeId,
    },
    /// A node's common index differs from the first node's.
    IndexDiffers {
        /// The node.
        node: NodeId,
    },
    /// A node's state digest differs from the first node's.
    Digest {
        /// The node.
        node: NodeId,
    },
}

/// What a drained cluster agrees on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Agreement {
    /// The leader.
    pub leader: NodeId,
    /// The term.
    pub term: Term,
    /// The one index every recoverable node has as last, commit and applied.
    pub index: LogIndex,
    /// The one state digest.
    pub digest: StateDigest,
}

impl Sample {
    /// The look at `cluster` over `recoverable` with `client_work` operations in flight.
    #[must_use]
    pub fn of(cluster: &Cluster, recoverable: &[NodeId], client_work: usize) -> Self {
        let mut nodes = Vec::new();
        let mut down = Vec::new();
        for node in recoverable {
            match cluster.controller(*node) {
                Some(ctl) if !cluster.is_paused(*node) => {
                    let status = ctl.status();
                    nodes.push(NodeSample {
                        node: *node,
                        role: status.role,
                        term: status.term,
                        leader: status.leader,
                        last: status.last,
                        commit: status.commit,
                        applied: ctl.state().applied_index(),
                        digest: ctl.state().digest(),
                        queued: ctl.queued(),
                    });
                }
                Some(_) | None => down.push(*node),
            }
        }
        Self {
            client_work,
            nodes,
            down,
        }
    }

    /// Whether this look shows a drain, and what the nodes agree on if so. The first rule
    /// broken is reported, in the order of the module documentation.
    ///
    /// # Errors
    ///
    /// [`Uncertified`] naming the rule.
    pub fn judge(&self) -> Result<Agreement, Uncertified> {
        if self.client_work > 0 {
            return Err(Uncertified::ClientWork {
                in_flight: self.client_work,
            });
        }
        if let Some(node) = self.down.first() {
            return Err(Uncertified::NodeDown { node: *node });
        }
        if let Some(busy) = self.nodes.iter().find(|node| node.queued > 0) {
            return Err(Uncertified::Queued {
                node: busy.node,
                queued: busy.queued,
            });
        }
        let leaders: Vec<&NodeSample> = self
            .nodes
            .iter()
            .filter(|node| node.role == Role::Leader)
            .collect();
        let leader = match leaders.as_slice() {
            [] => return Err(Uncertified::NoLeader),
            [only] => *only,
            [first, second, ..] => {
                return Err(Uncertified::TwoLeaders {
                    nodes: [first.node, second.node],
                });
            }
        };
        for node in &self.nodes {
            if node.term != leader.term {
                return Err(Uncertified::Term { node: node.node });
            }
            if node.node != leader.node && node.leader != Some(leader.node) {
                return Err(Uncertified::Follows { node: node.node });
            }
            if node.last != node.commit || node.commit != node.applied {
                return Err(Uncertified::Indexes { node: node.node });
            }
            if node.applied != leader.applied {
                return Err(Uncertified::IndexDiffers { node: node.node });
            }
            if node.digest != leader.digest {
                return Err(Uncertified::Digest { node: node.node });
            }
        }
        Ok(Agreement {
            leader: leader.node,
            term: leader.term,
            index: leader.applied,
            digest: leader.digest,
        })
    }
}

/// A drain that held for the whole quiet period. Only [`QuietClock::observe`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Certified {
    agreement: Agreement,
    quiet_ticks: u64,
    since: VirtualTime,
    at: VirtualTime,
}

impl Certified {
    /// What the nodes agreed on throughout.
    #[must_use]
    pub const fn agreement(&self) -> Agreement {
        self.agreement
    }

    /// The quiet period it held for, in ticks.
    #[must_use]
    pub const fn quiet_ticks(&self) -> u64 {
        self.quiet_ticks
    }

    /// When the agreement was first seen.
    #[must_use]
    pub const fn since(&self) -> VirtualTime {
        self.since
    }

    /// When the quiet period was complete.
    #[must_use]
    pub const fn at(&self) -> VirtualTime {
        self.at
    }
}

/// Judges successive looks: the quiet period starts at the first look showing an agreement
/// and restarts whenever a look breaks it or shows a different one.
#[derive(Clone, Debug)]
pub struct QuietClock {
    quiet_ticks: u64,
    quiet_us: u64,
    holding: Option<(Agreement, VirtualTime)>,
}

impl QuietClock {
    /// A clock for a quiet period of `quiet_ticks` ticks of `tick_ms` milliseconds each.
    #[must_use]
    pub const fn new(quiet_ticks: u64, tick_ms: u64) -> Self {
        Self {
            quiet_ticks,
            quiet_us: quiet_ticks.saturating_mul(tick_ms).saturating_mul(1_000),
            holding: None,
        }
    }

    /// The agreement being held and since when, if the last look showed one.
    #[must_use]
    pub const fn holding(&self) -> Option<(Agreement, VirtualTime)> {
        self.holding
    }

    /// Takes the look `judged` made at `now`. Returns the certification once the same
    /// agreement has been seen at looks spanning the whole quiet period.
    pub fn observe(
        &mut self,
        now: VirtualTime,
        judged: &Result<Agreement, Uncertified>,
    ) -> Option<Certified> {
        let Ok(agreement) = *judged else {
            self.holding = None;
            return None;
        };
        let since = match self.holding {
            Some((held, since)) if held == agreement => since,
            Some(_) | None => now,
        };
        self.holding = Some((agreement, since));
        (now.as_micros().saturating_sub(since.as_micros()) >= self.quiet_us).then_some(Certified {
            agreement,
            quiet_ticks: self.quiet_ticks,
            since,
            at: now,
        })
    }
}
