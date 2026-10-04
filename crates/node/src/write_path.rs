//! The write path (PLAN.md §7.8, F-7, A-5; W14.2): the fixed decision order for a write
//! and the attempt state machine, for PUT, DELETE and compare-and-swap alike (W27.3).
//!
//! # The decision order (F-7)
//!
//! Every node evaluates a write in the same order, and the first step that answers wins:
//!
//! 1. **Validate** (F-8): the boundary parsed the write under the configured limits (CD-3,
//!    D-80), so its canonical command decodes. No step evaluates what the write would do: a
//!    compare-and-swap's expectation and a DELETE's key are judged only when its entry
//!    applies (C-5).
//! 2. **Lifecycle:** a node that is not `Ready` answers `recovering`; a stopping node
//!    answers `unavailable`.
//! 3. **Local admission bounds** (§7.11): `pending_attempts` (256) attempts and
//!    `pending_attempt_bytes` (16 MiB) of commands wait at most; past them, `busy`.
//! 4. **Short-circuit** from the applied history (F-6): a known request ID is answered here,
//!    on any `Ready` node, leader or not: the original result (`replayed`) for the same
//!    command, `request_id_conflict` for another.
//! 5. **Attach** (§7.8): an attempt with the ID of a pending proposal waits on it, at most
//!    `waiters_per_request` (16) per ID. With the same command it attaches and shares the
//!    proposal's fate. With another command it waits for the proposal to resolve, and is
//!    then evaluated against the committed history; if the proposal fails without applying,
//!    the waiter enters this order again and may propose.
//! 6. **Leader check:** a node that is not the leader answers `not_leader` naming the leader
//!    it believes in, or `unavailable` if it knows none.
//! 7. **History admission** (DM-7): `history_full` once the commit index reaches
//!    `max_log_entries`, or the committed command bytes reach `max_history_bytes`. When both
//!    are reached it names `max_log_entries`, the first in DM-7 (br-camk may rank them).
//! 8. **Uncommitted bytes:** `busy` if the commands proposed this leadership and not yet
//!    applied, with this one, would pass `max_uncommitted_bytes`. The library's own bound
//!    stays as the backstop: its synchronous `ProposalDropped` is `busy` too.
//! 9. **Propose:** the attempt owns a new pending proposal, which waits for the library.
//!
//! Short-circuits are answered even while a sync is outstanding; attach and propose queue
//! behind it (§7.4 step 3).
//!
//! # The attempt state machine (§7.8, A-5)
//!
//! An attempt is `NotSubmitted` until its proposal is handed to the library, and
//! `Submitted` from the moment it is handed over, before the library sees it. Only the
//! application of an entry carrying its request ID answers it with a terminal result
//! (DUR-1): with its command, that entry's result; with another command,
//! `request_id_conflict` naming the original index, because its own entry can then only
//! apply as a conflict (§7.8, §7.5 step 3). A queued proposal is then withdrawn, and a
//! submitted one keeps its bytes in flight until its own copy applies, its leadership is
//! lost, or the node fails or stops. An attempt that attached to a same-command proposal
//! counts as submitted (A-5).
//!
//! - A synchronous refusal by the library with nothing appended: the owner was never
//!   submitted (`busy`, or `not_leader`/`unavailable`); attached waiters answer
//!   `unknown_outcome`.
//! - The deadline (`write_deadline_ticks` from admission), the loss of the leadership the
//!   proposal was made under, or the node failing: `unknown_outcome` for a submitted or
//!   attached attempt. An attempt whose command was never proposed answers `busy` at its
//!   deadline, and enters the decision order again when its leadership is lost.
//! - Shutdown: `unknown_outcome` for submitted and attached attempts, `unavailable` for the
//!   rest and for every new attempt.

use std::collections::{BTreeMap, VecDeque};

use statemachine::codec::{self, Command};
use statemachine::{Applied, CommandDigest};
use types::{
    AttemptId, ByteCount, Config, Effect, EntryPayload, HistoryLimit, KeyValueLimits, Limit,
    LogIndex, Mutation, NodeId, ProposeError, Refusal, RequestId, Response, Role, ServedBy, Term,
    Tick, invariant,
};

use crate::requests::{Applying, Proposal, View};

#[cfg(test)]
mod invariant_fixtures;

/// The configured limits the write path enforces.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// The key and value limits the boundary parses writes under (D-80).
    key_values: KeyValueLimits,
    /// `max_log_entries` (DM-7).
    max_log_entries: LogIndex,
    /// `max_history_bytes` (DM-7).
    max_history_bytes: ByteCount,
    /// `max_uncommitted_bytes` (§7.11).
    max_uncommitted_bytes: u64,
    /// `pending_attempts` (§7.11).
    pending_attempts: usize,
    /// `pending_attempt_bytes` (§7.11).
    pending_attempt_bytes: u64,
    /// `waiters_per_request` (§7.11).
    waiters_per_request: usize,
}

/// How an attempt in the table waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    /// It owns the pending proposal for its request ID.
    Owner,
    /// It attached to a pending proposal with the same command: it counts as submitted.
    Attached,
    /// Same ID, another command: it waits for the pending proposal to resolve (§7.8).
    Behind,
}

/// One write: its request ID, canonical command and the command's digest.
#[derive(Clone, Debug)]
struct Write {
    /// The request ID.
    request_id: RequestId,
    /// The canonical command.
    command: Vec<u8>,
    /// The command's digest.
    digest: CommandDigest,
}

/// An attempt waiting in the table.
#[derive(Clone, Debug)]
struct Waiting {
    /// Its write.
    write: Write,
    /// How it waits.
    wait: Wait,
}

/// Where a pending proposal is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Waiting for the library to take an input: not submitted.
    Queued,
    /// Handed to the library: submitted.
    Submitted,
}

/// The pending proposal for one request ID, and the attempts waiting on it.
#[derive(Clone, Debug)]
struct Pending {
    /// The proposal's command.
    command: Vec<u8>,
    /// Its digest.
    digest: CommandDigest,
    /// Where it is.
    stage: Stage,
    /// The attempt that made it, while that attempt waits.
    owner: Option<AttemptId>,
    /// Attempts with the same command, attached.
    attached: Vec<AttemptId>,
    /// Attempts with another command, waiting for it to resolve.
    behind: Vec<AttemptId>,
}

impl Pending {
    /// Its waiters: attached and behind.
    const fn waiters(&self) -> usize {
        self.attached.len() + self.behind.len()
    }
}

/// A [`View`] or an [`Applying`]: what can cancel a deadline.
trait CancelDeadline {
    /// Removes `attempt`'s deadline.
    fn cancel(&mut self, attempt: AttemptId);
}

impl CancelDeadline for View<'_> {
    fn cancel(&mut self, attempt: AttemptId) {
        self.cancel_deadline(attempt);
    }
}

impl CancelDeadline for Applying<'_> {
    fn cancel(&mut self, attempt: AttemptId) {
        self.cancel_deadline(attempt);
    }
}

/// The write path's state: the attempts that wait, and the pending proposals.
///
/// Invariants (`NODE-WRITE-TABLE`):
/// - at most `pending_attempts` attempts wait, whose commands total `attempt_bytes`, at
///   most `pending_attempt_bytes`;
/// - at most `pending_attempts` proposals are pending, each with at most
///   `waiters_per_request` waiters;
/// - every attempt a pending proposal names waits in the table on that proposal's request
///   ID, in the role the proposal names it in, and every waiting attempt is named once;
/// - `to_propose` holds exactly the queued proposals' request IDs, each once (the W14.2 review): a
///   proposal leaves it when it is handed over or taken out, so it is never handed over
///   twice.
#[derive(Debug)]
pub struct WritePath {
    /// This node.
    node: NodeId,
    /// The configured limits.
    limits: Limits,
    /// The attempts that wait.
    attempts: BTreeMap<AttemptId, Waiting>,
    /// Their commands' total length.
    attempt_bytes: u64,
    /// The pending proposals, by request ID.
    pending: BTreeMap<RequestId, Pending>,
    /// Queued proposals, in admission order.
    to_propose: VecDeque<RequestId>,
    /// The term of the leadership the pending proposals were made under.
    leader_term: Option<Term>,
}

impl WritePath {
    /// The write path of `node` under `config`'s limits.
    #[must_use]
    pub fn new(config: &Config, node: NodeId) -> Self {
        let limit = |limit: Limit| config.limit(limit);
        Self {
            node,
            limits: Limits {
                key_values: config.key_value_limits(),
                max_log_entries: config.max_log_entries(),
                max_history_bytes: config.max_history_bytes(),
                max_uncommitted_bytes: config.max_uncommitted_bytes().get(),
                pending_attempts: usize::try_from(limit(Limit::PendingAttempts))
                    .unwrap_or(usize::MAX),
                pending_attempt_bytes: limit(Limit::PendingAttemptBytes),
                waiters_per_request: usize::try_from(limit(Limit::WaitersPerRequest))
                    .unwrap_or(usize::MAX),
            },
            attempts: BTreeMap::new(),
            attempt_bytes: 0,
            pending: BTreeMap::new(),
            to_propose: VecDeque::new(),
            leader_term: None,
        }
    }

    /// The attempts waiting.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.attempts.len()
    }

    /// The pending proposals.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The bytes of the pending proposals: proposed this leadership, or waiting to be, and
    /// not yet applied (§7.11).
    #[must_use]
    pub fn in_flight_bytes(&self) -> u64 {
        self.pending
            .values()
            .map(|pending| len_of(&pending.command))
            .fold(0, u64::saturating_add)
    }

    /// A write attempt, taken from the queue (§7.8, F-7). Every kind of write takes the same
    /// path: a DELETE or compare-and-swap is proposed exactly as sent, and only its entry's
    /// application decides it (C-5).
    ///
    /// # Panics
    ///
    /// `NODE-WRITE-VALID` if its command does not decode under the configured key and value
    /// limits: the boundary validates every write (F-8, D-80).
    pub fn attempt(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        request_id: RequestId,
        mutation: Mutation,
        effects: &mut Vec<Effect>,
    ) {
        let command = codec::encode(&Command {
            request_id,
            mutation,
        });
        let valid = codec::decode(&command, self.limits.key_values);
        invariant!(
            valid.is_ok(),
            "NODE-WRITE-VALID",
            "node {}: attempt {}'s command does not decode under the configured limits \
             ({valid:?}); the boundary validates every write (F-8, D-80)",
            self.node,
            attempt.get()
        );
        let digest = CommandDigest::of_canonical(&command);
        let write = Write {
            request_id,
            command,
            digest,
        };
        self.decide(view, attempt, write, true, effects);
        self.check();
    }

    /// The next queued proposal, handed to the library now: from here its owner is
    /// submitted (§7.8). Nothing is handed over while the node does not lead, or leads in
    /// another term than the proposal was made in; a status change then settles it.
    pub fn next_proposal(&mut self, view: &mut View<'_>) -> Option<Proposal> {
        if !self.leads_as_admitted(view) {
            return None;
        }
        while let Some(request_id) = self.to_propose.pop_front() {
            let Some(pending) = self.pending.get_mut(&request_id) else {
                continue;
            };
            let (Stage::Queued, Some(owner)) = (pending.stage, pending.owner) else {
                continue;
            };
            pending.stage = Stage::Submitted;
            let proposal = Proposal {
                attempt: owner,
                command: pending.command.clone(),
            };
            self.check();
            return Some(proposal);
        }
        None
    }

    /// The library took the proposal of `attempt`, or refused it with nothing appended
    /// (§7.8, A-5).
    pub fn proposed(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        result: Result<(), ProposeError>,
        effects: &mut Vec<Effect>,
    ) {
        let Err(error) = result else {
            return;
        };
        let Some(request_id) = self
            .attempts
            .get(&attempt)
            .map(|waiting| waiting.write.request_id)
        else {
            return;
        };
        // Only the proposal just handed over, owned by `attempt`, appended nothing (br-nxsb).
        let handed_over = self.pending.get(&request_id).is_some_and(|pending| {
            pending.stage == Stage::Submitted && pending.owner == Some(attempt)
        });
        if !handed_over {
            return;
        }
        let Some(mut pending) = self.take_pending(&request_id) else {
            return;
        };
        // Nothing was appended: the owner was never submitted.
        pending.stage = Stage::Queued;
        let refusal = match error {
            ProposeError::Dropped => Refusal::Busy,
            ProposeError::NotLeader => leader_refusal(self.node, view).unwrap_or(Refusal::Busy),
        };
        self.fail_pending(view, pending, Some(refusal), effects);
        self.check();
    }

    /// The loop applied an entry (§7.4 step 5). If it carries a pending proposal's request
    /// ID, every attempt waiting on that proposal is answered. With the proposal's command,
    /// the attempts with that command get the entry's result and the others are answered
    /// against the committed history. With another command, the proposal is decided by it
    /// (see `decided_by_another`).
    pub fn applied(&mut self, applying: &mut Applying<'_>, effects: &mut Vec<Effect>) {
        let EntryPayload::Command(bytes) = &applying.entry().payload else {
            return;
        };
        let digest = CommandDigest::of_canonical(bytes);
        let request_id = match applying.applied() {
            Applied::Executed { request_id, .. }
            | Applied::Replayed { request_id, .. }
            | Applied::Conflict { request_id, .. } => *request_id,
            Applied::Empty => return,
        };
        let Some(pending_digest) = self.pending.get(&request_id).map(|pending| pending.digest)
        else {
            return;
        };
        if pending_digest != digest {
            self.decided_by_another(applying, request_id, effects);
            self.check();
            return;
        }
        let Some(pending) = self.take_pending(&request_id) else {
            return;
        };
        let served_by = ServedBy {
            node: self.node,
            applied: applying.state().applied_index(),
        };
        let response = match applying.applied() {
            Applied::Executed { result, .. } => Response::Written {
                result: *result,
                replayed: false,
                served_by,
            },
            Applied::Replayed { result, .. } => Response::Written {
                result: *result,
                replayed: true,
                served_by,
            },
            Applied::Conflict { original_index, .. } => Response::Conflict {
                original_index: *original_index,
            },
            Applied::Empty => return,
        };
        let record = applying.state().history().get(&request_id).copied();
        for attempt in pending.owner.iter().chain(&pending.attached) {
            self.answer(applying, *attempt, response.clone(), effects);
        }
        for attempt in pending.behind {
            let Some(waiting) = self.attempts.get(&attempt) else {
                continue;
            };
            // The history now holds this request ID: the waiter is answered from it (§7.5
            // step 3), never with the other command's result.
            let answer = match record {
                Some(record) if record.digest() == waiting.write.digest => Response::Written {
                    result: record.result(),
                    replayed: true,
                    served_by,
                },
                Some(record) => Response::Conflict {
                    original_index: record.first_index(),
                },
                None => response.clone(),
            };
            self.answer(applying, attempt, answer, effects);
        }
        self.check();
    }

    /// Another command carrying `request_id` applied while a proposal for it is pending
    /// (§7.8 Submitted row). The history now holds the ID, so the proposal's own entry can
    /// only apply as a conflict (§7.5 step 3):
    /// - its owner and attached waiters answer `request_id_conflict` naming the original
    ///   index, never the other command's result;
    /// - the attempts behind it are answered from the history: `replayed` with the original
    ///   result for the applied command, `request_id_conflict` for any other;
    /// - a queued proposal is withdrawn, never proposed;
    /// - a submitted one stays pending with no waiters, so its bytes count as in flight until
    ///   its own copy applies, its leadership is lost, or the node fails or stops (§7.11,
    ///   br-evs6).
    fn decided_by_another(
        &mut self,
        applying: &mut Applying<'_>,
        request_id: RequestId,
        effects: &mut Vec<Effect>,
    ) {
        let Some(record) = applying.state().history().get(&request_id).copied() else {
            return;
        };
        let Some(pending) = self.pending.get_mut(&request_id) else {
            return;
        };
        let stage = pending.stage;
        let decided: Vec<AttemptId> = pending
            .owner
            .take()
            .into_iter()
            .chain(std::mem::take(&mut pending.attached))
            .collect();
        let behind = std::mem::take(&mut pending.behind);
        match stage {
            Stage::Queued => {
                self.take_pending(&request_id);
            }
            Stage::Submitted => {}
        }
        let served_by = ServedBy {
            node: self.node,
            applied: applying.state().applied_index(),
        };
        let conflict = Response::Conflict {
            original_index: record.first_index(),
        };
        for attempt in decided {
            self.answer(applying, attempt, conflict.clone(), effects);
        }
        for attempt in behind {
            let Some(waiting) = self.attempts.get(&attempt) else {
                continue;
            };
            let answer = if record.digest() == waiting.write.digest {
                Response::Written {
                    result: record.result(),
                    replayed: true,
                    served_by,
                }
            } else {
                conflict.clone()
            };
            self.answer(applying, attempt, answer, effects);
        }
    }

    /// `attempt`'s deadline passed (§7.8): a submitted or attached attempt answers
    /// `unknown_outcome`, one whose command was never proposed answers `busy`.
    pub fn expired(&mut self, view: &mut View<'_>, attempt: AttemptId, effects: &mut Vec<Effect>) {
        let Some(waiting) = self.attempts.get(&attempt).cloned() else {
            return;
        };
        let request_id = waiting.write.request_id;
        match waiting.wait {
            Wait::Owner => {
                let queued = self
                    .pending
                    .get(&request_id)
                    .is_some_and(|pending| pending.stage == Stage::Queued);
                if queued {
                    // Never proposed: the proposal is withdrawn with its owner.
                    if let Some(pending) = self.take_pending(&request_id) {
                        self.fail_pending(view, pending, Some(Refusal::Busy), effects);
                    }
                } else {
                    // Submitted: the proposal stays in flight, and may still commit.
                    if let Some(pending) = self.pending.get_mut(&request_id) {
                        pending.owner = None;
                    }
                    self.answer(
                        view,
                        attempt,
                        Response::Refused(Refusal::UnknownOutcome),
                        effects,
                    );
                }
            }
            Wait::Attached => {
                if let Some(pending) = self.pending.get_mut(&request_id) {
                    pending.attached.retain(|other| *other != attempt);
                }
                self.answer(
                    view,
                    attempt,
                    Response::Refused(Refusal::UnknownOutcome),
                    effects,
                );
            }
            Wait::Behind => {
                if let Some(pending) = self.pending.get_mut(&request_id) {
                    pending.behind.retain(|other| *other != attempt);
                }
                self.answer(view, attempt, Response::Refused(Refusal::Busy), effects);
            }
        }
        self.check();
    }

    /// The status changed (§7.8). If the node no longer leads in the term its pending
    /// proposals were made in, or is no longer `Ready`, they fail: whether a submitted one
    /// commits is unknown.
    pub fn status_changed(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        if self.pending.is_empty() {
            self.leader_term = None;
            self.to_propose.clear();
            return;
        }
        if self.leads_as_admitted(view) {
            return;
        }
        self.leader_term = None;
        self.to_propose.clear();
        let lost = std::mem::take(&mut self.pending);
        for pending in lost.into_values() {
            self.fail_pending(view, pending, None, effects);
        }
        self.check();
    }

    /// The process is stopping (§7.8, §8.4): submitted and attached attempts answer
    /// `unknown_outcome`, every other waiting attempt `unavailable`.
    pub fn shutdown(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        self.leader_term = None;
        self.to_propose.clear();
        let all = std::mem::take(&mut self.pending);
        for pending in all.into_values() {
            let owner_answer = match pending.stage {
                Stage::Queued => Refusal::Unavailable,
                Stage::Submitted => Refusal::UnknownOutcome,
            };
            if let Some(owner) = pending.owner {
                self.answer(view, owner, Response::Refused(owner_answer), effects);
            }
            for attempt in pending.attached {
                self.answer(
                    view,
                    attempt,
                    Response::Refused(Refusal::UnknownOutcome),
                    effects,
                );
            }
            for attempt in pending.behind {
                self.answer(
                    view,
                    attempt,
                    Response::Refused(Refusal::Unavailable),
                    effects,
                );
            }
        }
        self.check();
    }

    /// The decision order for `attempt`, from step 2 (module documentation). A `fresh`
    /// attempt is new; otherwise it already waits in the table and enters the order again,
    /// keeping its deadline and its place in the admission bounds.
    fn decide(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        write: Write,
        fresh: bool,
        effects: &mut Vec<Effect>,
    ) {
        let len = len_of(&write.command);
        // 2. Lifecycle.
        if let Some(refusal) = not_admitted(view) {
            self.answer(view, attempt, Response::Refused(refusal), effects);
            return;
        }
        // 3. Local admission bounds.
        if fresh
            && (self.attempts.len() >= self.limits.pending_attempts
                || self.attempt_bytes.saturating_add(len) > self.limits.pending_attempt_bytes)
        {
            self.answer(view, attempt, Response::Refused(Refusal::Busy), effects);
            return;
        }
        // 4. Short-circuit from the applied history.
        if let Some(record) = view.state().history().get(&write.request_id).copied() {
            let response = if record.digest() == write.digest {
                Response::Written {
                    result: record.result(),
                    replayed: true,
                    served_by: ServedBy {
                        node: self.node,
                        applied: view.state().applied_index(),
                    },
                }
            } else {
                Response::Conflict {
                    original_index: record.first_index(),
                }
            };
            self.answer(view, attempt, response, effects);
            return;
        }
        // 5. Attach to a pending proposal with this ID.
        if let Some(pending) = self.pending.get_mut(&write.request_id) {
            if pending.waiters() >= self.limits.waiters_per_request {
                self.answer(view, attempt, Response::Refused(Refusal::Busy), effects);
                return;
            }
            let wait = if pending.digest == write.digest {
                pending.attached.push(attempt);
                Wait::Attached
            } else {
                pending.behind.push(attempt);
                Wait::Behind
            };
            self.wait(view, attempt, write, wait, fresh);
            return;
        }
        // 6. Leader check.
        if let Some(refusal) = leader_refusal(self.node, view) {
            self.answer(view, attempt, Response::Refused(refusal), effects);
            return;
        }
        // 7. History admission (DM-7).
        if let Some(limit) = self.history_limit(view) {
            let refusal = Refusal::HistoryFull { limit };
            self.answer(view, attempt, Response::Refused(refusal), effects);
            return;
        }
        // 8. Uncommitted bytes, and the bound on pending proposals.
        if self.in_flight_bytes().saturating_add(len) > self.limits.max_uncommitted_bytes
            || self.pending.len() >= self.limits.pending_attempts
        {
            self.answer(view, attempt, Response::Refused(Refusal::Busy), effects);
            return;
        }
        // 9. Propose: a new pending proposal, queued for the library.
        self.pending.insert(
            write.request_id,
            Pending {
                command: write.command.clone(),
                digest: write.digest,
                stage: Stage::Queued,
                owner: Some(attempt),
                attached: Vec::new(),
                behind: Vec::new(),
            },
        );
        self.to_propose.push_back(write.request_id);
        self.leader_term = Some(view.status().term);
        self.wait(view, attempt, write, Wait::Owner, fresh);
    }

    /// Puts `attempt` in the table, waiting as `wait`: a fresh attempt gets its deadline,
    /// one that enters the order again keeps it.
    ///
    /// # Panics
    ///
    /// `NODE-WRITE-DEADLINE` if a fresh attempt gets no deadline: the deadline book holds
    /// one per waiting attempt, and both are bounded by `pending_attempts`.
    fn wait(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        write: Write,
        wait: Wait,
        fresh: bool,
    ) {
        if fresh {
            let deadline: Result<Tick, _> = view.start_deadline(attempt);
            invariant!(
                deadline.is_ok(),
                "NODE-WRITE-DEADLINE",
                "node {}: attempt {} waits without a deadline: {deadline:?}",
                self.node,
                attempt.get()
            );
            self.attempt_bytes = self.attempt_bytes.saturating_add(len_of(&write.command));
        }
        self.attempts.insert(attempt, Waiting { write, wait });
    }

    /// Settles a pending proposal that failed without its entry applying. A queued owner
    /// was never submitted: it gets `owner_refusal`, or enters the decision order again if
    /// that is `None`. A submitted owner and attached waiters answer `unknown_outcome`.
    /// Attempts behind it enter the decision order again.
    fn fail_pending(
        &mut self,
        view: &mut View<'_>,
        pending: Pending,
        owner_refusal: Option<Refusal>,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(owner) = pending.owner {
            match (pending.stage, owner_refusal) {
                (Stage::Submitted, _) => {
                    self.answer(
                        view,
                        owner,
                        Response::Refused(Refusal::UnknownOutcome),
                        effects,
                    );
                }
                (Stage::Queued, Some(refusal)) => {
                    self.answer(view, owner, Response::Refused(refusal), effects);
                }
                (Stage::Queued, None) => self.reenter(view, owner, effects),
            }
        }
        for attempt in pending.attached {
            self.answer(
                view,
                attempt,
                Response::Refused(Refusal::UnknownOutcome),
                effects,
            );
        }
        for attempt in pending.behind {
            self.reenter(view, attempt, effects);
        }
    }

    /// `attempt`, which waits in the table, enters the decision order again (§7.8).
    fn reenter(&mut self, view: &mut View<'_>, attempt: AttemptId, effects: &mut Vec<Effect>) {
        let Some(waiting) = self.attempts.get(&attempt).cloned() else {
            return;
        };
        self.decide(view, attempt, waiting.write, false, effects);
    }

    /// Takes the pending proposal for `request_id` out of the table and out of
    /// `to_propose`, so a proposal with the same ID made later is queued once.
    fn take_pending(&mut self, request_id: &RequestId) -> Option<Pending> {
        let pending = self.pending.remove(request_id)?;
        self.to_propose.retain(|queued| queued != request_id);
        Some(pending)
    }

    /// Answers `attempt` with `response`, taking it out of the table and cancelling its
    /// deadline if it waited.
    fn answer(
        &mut self,
        deadlines: &mut impl CancelDeadline,
        attempt: AttemptId,
        response: Response,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(waiting) = self.attempts.remove(&attempt) {
            self.attempt_bytes = self
                .attempt_bytes
                .saturating_sub(len_of(&waiting.write.command));
            deadlines.cancel(attempt);
        }
        effects.push(Effect::Respond { attempt, response });
    }

    /// Whether the node still leads, `Ready`, in the term the pending proposals were made
    /// in.
    fn leads_as_admitted(&self, view: &View<'_>) -> bool {
        not_admitted(view).is_none()
            && view.status().role == Role::Leader
            && self
                .leader_term
                .is_none_or(|term| term == view.status().term)
    }

    /// DM-7's history admission: the capacity reached, if any.
    fn history_limit(&self, view: &View<'_>) -> Option<HistoryLimit> {
        if view.status().commit >= self.limits.max_log_entries {
            Some(HistoryLimit::LogEntries)
        } else if view.state().committed_command_bytes() >= self.limits.max_history_bytes {
            Some(HistoryLimit::HistoryBytes)
        } else {
            None
        }
    }

    /// # Panics
    ///
    /// `NODE-WRITE-TABLE` if the table breaks an invariant of [`WritePath`].
    fn check(&self) {
        let bytes = self
            .attempts
            .values()
            .map(|waiting| len_of(&waiting.write.command))
            .fold(0, u64::saturating_add);
        let named: usize = self
            .pending
            .values()
            .map(|pending| usize::from(pending.owner.is_some()) + pending.waiters())
            .sum();
        let roles_agree = self.pending.iter().all(|(request_id, pending)| {
            let is = |attempt: &AttemptId, wait: Wait| {
                self.attempts.get(attempt).is_some_and(|waiting| {
                    waiting.write.request_id == *request_id && waiting.wait == wait
                })
            };
            pending.owner.iter().all(|owner| is(owner, Wait::Owner))
                && pending
                    .attached
                    .iter()
                    .all(|attempt| is(attempt, Wait::Attached))
                && pending
                    .behind
                    .iter()
                    .all(|attempt| is(attempt, Wait::Behind))
                && pending.waiters() <= self.limits.waiters_per_request
        });
        let queued = self
            .pending
            .values()
            .filter(|pending| pending.stage == Stage::Queued)
            .count();
        let to_propose_exact = self.to_propose.len() == queued
            && self.to_propose.iter().enumerate().all(|(at, request_id)| {
                self.pending
                    .get(request_id)
                    .is_some_and(|pending| pending.stage == Stage::Queued)
                    && !self
                        .to_propose
                        .iter()
                        .skip(at + 1)
                        .any(|later| later == request_id)
            });
        invariant!(
            bytes == self.attempt_bytes
                && self.attempts.len() <= self.limits.pending_attempts
                && self.attempt_bytes <= self.limits.pending_attempt_bytes
                && self.pending.len() <= self.limits.pending_attempts
                && named == self.attempts.len()
                && roles_agree
                && to_propose_exact,
            "NODE-WRITE-TABLE",
            "node {}: {} attempts wait ({} bytes counted, {bytes} held), {} named by {} pending \
             proposals; roles agree: {roles_agree}; to_propose holds {} for {queued} queued \
             (exact: {to_propose_exact})",
            self.node,
            self.attempts.len(),
            self.attempt_bytes,
            named,
            self.pending.len(),
            self.to_propose.len()
        );
    }
}

/// Step 2: the refusal of a node that admits no write now: `recovering` if it is not
/// `Ready`, `unavailable` if it is stopping.
fn not_admitted(view: &View<'_>) -> Option<Refusal> {
    if let Some(lifecycle) = view.lifecycle().not_ready() {
        return Some(Refusal::Recovering { lifecycle });
    }
    view.shutting_down().then_some(Refusal::Unavailable)
}

/// Step 6: the refusal of a node that is not the leader: `not_leader` naming the leader it
/// believes in, or `unavailable` if it knows none.
fn leader_refusal(node: NodeId, view: &View<'_>) -> Option<Refusal> {
    let status = view.status();
    match status.role {
        Role::Leader => None,
        Role::Follower | Role::Candidate | Role::PreCandidate => Some(match status.leader {
            Some(leader) if leader != node => Refusal::NotLeader { leader },
            Some(_) | None => Refusal::Unavailable,
        }),
    }
}

/// A command's length, in bytes.
fn len_of(command: &[u8]) -> u64 {
    u64::try_from(command.len()).unwrap_or(u64::MAX)
}
