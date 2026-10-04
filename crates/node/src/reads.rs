//! The read path (PLAN.md §7.9, F-2, F-9, A-2, C-3, C-4; W14.3).
//!
//! A GET or a status lookup is answered in the turn that takes it from the queue, from the
//! node's published view: the state machine as of its applied index. The controller applies
//! one whole committed entry at a time and never inside a turn's read, so a read sees exactly
//! the prefix `[1..applied_index]` it reports, never a partial application (C-3). The answer
//! copies what it reports (one value, at most 64 KiB; one stored result; or one SCAN page, at
//! most 128 items and 1 MiB of decoded key+value bytes) before any later entry can change the
//! view, so a page comes from one applied prefix (C-8).
//!
//! Every answer carries `served_by` (this node and its applied index). There is no leader
//! round trip and no quorum, so any `Ready` node answers, even while a sync is outstanding.
//! Within an incarnation the applied index only grows, so the reported prefix never goes
//! back (C-4). A node that is not `Ready` answers `recovering`, and a stopping one
//! `unavailable`: it never serves an empty or partial state (A-2).
//!
//! A SCAN page is the state machine's page of the range (§8.5), by §7.11's exact rule (W28).

use types::{
    AttemptId, Effect, Key, NodeId, Refusal, RequestId, RequestState, Response, ScanRange, ServedBy,
};

use crate::requests::View;

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

    /// A status lookup (F-9): `Known` with the request's original terminal result if this
    /// node has applied it, `Unknown` otherwise. It reads the history and nothing else: it
    /// never proposes or records anything, and `Unknown` never means "did not happen" (P8).
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
                state: view
                    .state()
                    .history()
                    .get(request_id)
                    .map_or(RequestState::Unknown, |record| {
                        RequestState::Known(record.result())
                    }),
                served_by,
            },
        };
        effects.push(Effect::Respond { attempt, response });
    }

    /// A SCAN page (F-4, §8.5): the range's page of the applied prefix, by the state
    /// machine's page rule (§7.11), with `served_by`. It is read and copied in this turn, so
    /// the whole page comes from the one prefix it reports (C-8).
    pub fn scan(
        &self,
        view: &View<'_>,
        attempt: AttemptId,
        range: &ScanRange,
        effects: &mut Vec<Effect>,
    ) {
        let response = match self.served_by(view) {
            Err(refusal) => Response::Refused(refusal),
            Ok(served_by) => Response::Page(view.state().scan(range, served_by)),
        };
        effects.push(Effect::Respond { attempt, response });
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
