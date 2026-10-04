//! The read path (PLAN.md §7.9, F-2, F-9, A-2, C-3, C-4; W14.3).
//!
//! A GET or a status lookup is answered in the turn that takes it from the queue, from the
//! node's published view: the state machine as of its applied index. The controller applies
//! one whole committed entry at a time and never inside a turn's read, so a read sees exactly
//! the prefix `[1..applied_index]` it reports, never a partial application (C-3). The answer
//! copies what it reports (one value, at most 64 KiB, or one stored result) before any later
//! entry can change the view.
//!
//! Every answer carries `served_by` (this node and its applied index). There is no leader
//! round trip and no quorum, so any `Ready` node answers, even while a sync is outstanding.
//! Within an incarnation the applied index only grows, so the reported prefix never goes
//! back (C-4). A node that is not `Ready` answers `recovering`, and a stopping one
//! `unavailable`: it never serves an empty or partial state (A-2).
//!
//! A SCAN page is W28's: until it implements the page rule, none reaches the read path.

use types::{AttemptId, Effect, Key, NodeId, Refusal, RequestId, Response, ServedBy, invariant};

use crate::requests::View;

#[cfg(test)]
mod invariant_fixtures;

/// The read path of one node. It keeps no state: every answer comes from the view.
#[derive(Clone, Copy, Debug)]
pub struct ReadPath {
    /// This node.
    node: NodeId,
}

impl ReadPath {
    /// The read path of `node`.
    #[must_use]
    pub const fn new(node: NodeId) -> Self {
        Self { node }
    }

    /// A GET (F-2): the key's value and version, or `not_found`.
    pub fn get(&self, view: &View<'_>, attempt: AttemptId, key: &Key, effects: &mut Vec<Effect>) {
        let response = match self.served_by(view) {
            Err(refusal) => Response::Refused(refusal),
            Ok(served_by) => {
                view.state()
                    .get(key)
                    .map_or(Response::NotFound { served_by }, |stored| Response::Found {
                        value: stored.value().clone(),
                        version: stored.version(),
                        served_by,
                    })
            }
        };
        effects.push(Effect::Respond { attempt, response });
    }

    /// A status lookup (F-9): the request's stored terminal result if this node has applied
    /// it, `None` (`unknown`) otherwise. It never proposes anything, and `unknown` never
    /// means "did not happen".
    pub fn status(
        &self,
        view: &View<'_>,
        attempt: AttemptId,
        request_id: &RequestId,
        effects: &mut Vec<Effect>,
    ) {
        let response = match self.served_by(view) {
            Err(refusal) => Response::Refused(refusal),
            Ok(served_by) => Response::Status {
                result: view
                    .state()
                    .history()
                    .get(request_id)
                    .map(statemachine::Record::result),
                served_by,
            },
        };
        effects.push(Effect::Respond { attempt, response });
    }

    /// A SCAN page, which no node serves before W28 implements the page rule.
    ///
    /// # Panics
    ///
    /// `NODE-READ-NO-SCAN` always: no composition root submits a SCAN before W28.
    pub fn scan(&self, attempt: AttemptId) {
        let served = false;
        invariant!(
            served,
            "NODE-READ-NO-SCAN",
            "node {}: attempt {} is a SCAN, which no node serves before W28 implements its \
             page rule",
            self.node,
            attempt.get()
        );
    }

    /// Who answers, and from which prefix: this node at its applied index, if it may serve
    /// reads now; otherwise the refusal (A-2).
    const fn served_by(self, view: &View<'_>) -> Result<ServedBy, Refusal> {
        if let Some(lifecycle) = view.lifecycle().not_ready() {
            return Err(Refusal::Recovering { lifecycle });
        }
        if view.shutting_down() {
            return Err(Refusal::Unavailable);
        }
        Ok(ServedBy {
            node: self.node,
            applied: view.state().applied_index(),
        })
    }
}
