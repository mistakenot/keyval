//! The request lifecycle (PLAN.md §7.8, §7.9; W14): the node's [`Requests`], which the
//! composition roots give the [`Controller`](crate::Controller).
//!
//! It routes each client attempt by its kind: a write to the [`WritePath`] (F-7's decision
//! order and the attempt state machine), a GET or a status lookup to the [`ReadPath`]. Every
//! other call of the seam (proposals, applications, deadlines, status changes, shutdown)
//! belongs to the write path, because only writes wait.
//!
//! It also tells the trace each time the published view advances: one
//! `Observe(Applied { index })` per applied entry, the election no-op included, before
//! anything that entry answers (C-3, C-4). Responses need no observation of their own: each
//! `Respond` effect is in the trace, in generation order.

use types::{AttemptId, ClientOp, Config, Effect, NodeId, Observation, ProposeError};

use crate::reads::ReadPath;
use crate::requests::{Applying, Proposal, Requests, View};
use crate::write_path::WritePath;

/// The node's request lifecycle: its write path and its read path.
#[derive(Debug)]
pub struct RequestLifecycle {
    /// Writes.
    writes: WritePath,
    /// Reads.
    reads: ReadPath,
}

impl RequestLifecycle {
    /// The request lifecycle of `node` under `config`.
    #[must_use]
    pub fn new(config: &Config, node: NodeId) -> Self {
        Self {
            writes: WritePath::new(config, node),
            reads: ReadPath::new(node),
        }
    }

    /// The write path.
    #[must_use]
    pub const fn writes(&self) -> &WritePath {
        &self.writes
    }
}

impl Requests for RequestLifecycle {
    /// # Panics
    ///
    /// The write path's and the read path's invariants, such as `NODE-WRITE-VALID` for a
    /// write whose command does not decode under the configured limits.
    fn attempt(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        op: ClientOp,
        effects: &mut Vec<Effect>,
    ) {
        match op {
            ClientOp::Write {
                request_id,
                mutation,
            } => self
                .writes
                .attempt(view, attempt, request_id, mutation, effects),
            ClientOp::Get { key } => self.reads.get(view, attempt, &key, effects),
            ClientOp::Status { request_id } => {
                self.reads.status(view, attempt, &request_id, effects);
            }
            ClientOp::Scan { range } => self.reads.scan(view, attempt, &range, effects),
        }
    }

    fn next_proposal(&mut self, view: &mut View<'_>) -> Option<Proposal> {
        self.writes.next_proposal(view)
    }

    fn proposed(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        result: Result<(), ProposeError>,
        effects: &mut Vec<Effect>,
    ) {
        self.writes.proposed(view, attempt, result, effects);
    }

    fn applied(&mut self, applying: &mut Applying<'_>, effects: &mut Vec<Effect>) {
        effects.push(Effect::Observe(Observation::Applied {
            index: applying.state().applied_index(),
        }));
        self.writes.applied(applying, effects);
    }

    fn expired(&mut self, view: &mut View<'_>, attempt: AttemptId, effects: &mut Vec<Effect>) {
        self.writes.expired(view, attempt, effects);
    }

    fn status_changed(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        self.writes.status_changed(view, effects);
    }

    fn shutdown(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        self.writes.shutdown(view, effects);
    }
}
