//! The node-side recorder's bookkeeping (PLAN.md §10.2, T-5; W36.3): what the cluster last
//! reported of each node, so the next step reports only what changed.
//!
//! The recorder turns the cluster's own mirrors of its nodes into `keyval.observation/v1`
//! node-side records, never a node's claim about itself (T-12):
//! - `recovered`: at a node incarnation's start, its recovered durable state and log;
//! - `append` / `truncate`: the change of a running node's log since the last report, as a
//!   truncation of the suffix that no longer matches followed by the entries appended;
//! - `node_state`: a node's process, lifecycle, role, term, vote, leader, commit, applied
//!   index and state digest, whenever any of them changed.
//!
//! Recording draws no randomness and changes nothing (P4). The records queue is bounded
//! (CD-9); one that fills drops everything after it and is reported as `overflowed`, which
//! makes the run incomplete (P10).

use std::collections::BTreeMap;

use statemachine::StateDigest;
use types::{Entry, EntryIndex, Incarnation, Lifecycle, LogIndex, NodeId, Role, Term};

/// Whether a node's process is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// Running and taking steps.
    Running,
    /// Stalled: its inputs wait (§9.1).
    Paused,
    /// Down, and restarting later.
    Crashed,
    /// Lost for good (§9.1).
    Lost,
}

/// A node's god-view state, as `node_state` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeSnapshot {
    /// The process.
    pub process: Process,
    /// The lifecycle.
    pub lifecycle: Lifecycle,
    /// The role.
    pub role: Role,
    /// The current term.
    pub term: Term,
    /// The node voted for in `term`, if any.
    pub vote: Option<NodeId>,
    /// The leader it believes in, if any.
    pub leader: Option<NodeId>,
    /// The commit index.
    pub commit: LogIndex,
    /// The applied index.
    pub applied: LogIndex,
    /// The applied-state digest.
    pub digest: StateDigest,
}

/// What was last reported of one node.
///
/// Invariant: `terms` holds the term of each entry of the reported log, in index order; it
/// is no longer than the node's log bound, since the log it mirrors is not.
#[derive(Clone, Debug)]
pub struct Reported {
    /// The incarnation the log belongs to.
    pub incarnation: Incarnation,
    /// The term of each reported entry.
    pub terms: Vec<Term>,
    /// The last reported state, if any.
    pub state: Option<NodeSnapshot>,
}

/// The last report per node. At most one entry per node of the cluster (CD-9).
pub type Ledger = BTreeMap<NodeId, Reported>;

/// How `log` differs from a reported log with entry terms `reported`.
///
/// An entry is the same when its index and term are: the leader of a term writes one entry
/// per index. Returns the first index to truncate from, if the reported log has a suffix
/// that `log` no longer has, and the position in `log` from which entries are new.
#[must_use]
pub fn diff(reported: &[Term], log: &[Entry]) -> (Option<EntryIndex>, usize) {
    let common = reported
        .iter()
        .zip(log)
        .take_while(|(term, entry)| **term == entry.term)
        .count();
    let truncate = if reported.len() > common {
        u64::try_from(common)
            .ok()
            .and_then(|common| EntryIndex::new(common.saturating_add(1)).ok())
    } else {
        None
    };
    (truncate, common)
}
