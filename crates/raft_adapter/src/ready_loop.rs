//! The Ready loop (PLAN.md §7.4, D-28): the critical persistence protocol.
//!
//! [`ReadyLoop`] drives a [`RaftNode`] one input at a time, with at most one Ready
//! outstanding, and turns its work into the core's [`Effect`]s, in their binding order:
//!
//! 1. **Input:** one tick, peer message or proposal ([`ReadyLoop::feed`]). While a sync is
//!    outstanding the library takes no input. The input is handed back as
//!    [`Fed::Deferred`], and the controller queues it within its bounds, merging ticks into
//!    its pending-tick counter (§7.11).
//! 2. **Take Ready.** A snapshot is an invariant failure ([`RaftNode`]).
//! 3. **Persist:** one frame ([`Effect::Append`]) and its [`Effect::Sync`]. A Ready that
//!    need not sync writes no frame; the commit index alone is never worth one.
//! 4. **Release:** after the frame's `SyncDone` ([`ReadyLoop::sync_done`]), the storage view
//!    is unsealed, then every message is sent, in recorded order. Nothing is sent while the
//!    sync is outstanding: no vote grant or append acknowledgement leaves on unsynced state
//!    (DUR-2).
//! 5. **Apply** the committed entries in index order, through [`Apply`].
//! 6. **Advance:** send the `LightReady`'s messages and apply its entries, then drain further
//!    ready work before the next input, so committed work never waits on an idle queue.
//! 7. **Report** applied progress, for the entries actually applied.
//!
//! The library's commit rule is authoritative: an entry is applied only once the library
//! hands it over as committed, and the loop never counts a majority itself.

use journal::JournalError;
use types::{
    Consensus, Effect, Entry, Incarnation, JournalRead, LogIndex, OutboundMessage, ProposeError,
    Status, StepError, SyncSeq, invariant,
};

use crate::node::RaftNode;
use crate::storage::ReadFailure;

/// Applies committed entries for the loop (§7.4 step 5): the state machine, the request
/// history and waiting client attempts, all the controller's (W14).
pub trait Apply {
    /// Applies `entry`, given the commit index `commit` known when it is applied.
    ///
    /// Entries arrive once each, in index order, and never beyond `commit`. The applier may
    /// push its own effects (such as a `Respond` for an attempt that completes) after the
    /// loop's effects so far, which keeps the order deterministic.
    fn apply(&mut self, entry: &Entry, commit: LogIndex, effects: &mut Vec<Effect>);
}

/// One input for the library (§7.4 step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaftInput {
    /// One logical clock step.
    Tick,
    /// A peer's encoded Raft message.
    Peer(Vec<u8>),
    /// One encoded command to propose.
    Propose(Vec<u8>),
}

/// What feeding one input did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fed {
    /// The library ticked.
    Ticked,
    /// The library stepped the message, or rejected it (counted, §8.2).
    Stepped(Result<(), StepError>),
    /// The library took the proposal, or refused it with nothing appended (§7.8, A-5).
    Proposed(Result<(), ProposeError>),
    /// A sync is outstanding, so the library took no input (§7.4 step 3). The input comes
    /// back unchanged for the controller to queue within its bounds (§7.11).
    Deferred(RaftInput),
}

/// What a `SyncDone` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Synced {
    /// It completed the outstanding sync, and the loop ran steps 4 to 7 and drained.
    Completed,
    /// It came from another incarnation and was ignored (§7.2).
    Stale,
}

/// Why the loop could not continue. Each one moves the node to `failed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopError {
    /// A Ready's frame would take the journal past its hard stop (DM-7). Nothing was
    /// written; the node moves to `failed`, and the loop stops.
    HardStop {
        /// The journal's length.
        end: u64,
        /// The frame's length.
        frame_len: u64,
        /// `max_journal_bytes`.
        limit: u64,
    },
    /// A journal read under the library failed (§7.11). The library got only the entries
    /// before it; nothing more was persisted or sent, and the loop stops.
    StorageFailed(ReadFailure),
    /// The loop stopped earlier and takes no more work.
    Stopped,
}

/// Where the loop is between inputs.
#[derive(Debug)]
enum Phase {
    /// No Ready is outstanding: the library may take an input.
    Idle,
    /// A Ready's frame was appended, and its sync is outstanding.
    Syncing {
        /// The sync the frame waits for.
        seq: SyncSeq,
        /// The Ready's messages, released after the sync (step 4).
        messages: Vec<OutboundMessage>,
        /// The Ready's committed entries, applied after the release (step 5).
        committed: Vec<Entry>,
    },
    /// A hard stop or a failed read ended the loop.
    Stopped,
}

/// The serial Ready loop of one node (§7.4).
///
/// Invariants:
/// - at most one sync is outstanding, and the library takes no input while it is;
/// - no message is sent while a frame is waiting for its sync;
/// - entries are applied once each, in index order, never beyond the library's commit
///   index, and applied progress is reported only for entries applied;
/// - sync sequence numbers strictly increase within the incarnation.
#[derive(Debug)]
pub struct ReadyLoop<R: JournalRead> {
    /// The library, behind the Consensus port.
    node: RaftNode<R>,
    /// This life of the node: its syncs carry it, and completions from another are ignored.
    incarnation: Incarnation,
    /// The next sync's sequence number.
    next_sync: SyncSeq,
    /// Idle, syncing or stopped.
    phase: Phase,
    /// The newest commit index the library reported.
    commit: LogIndex,
    /// The last entry applied.
    applied: LogIndex,
    /// The library's view, as of the last time it could be read. Status lookups never touch
    /// the library (§7.4 step 1).
    status: Status,
}

impl<R: JournalRead> ReadyLoop<R> {
    /// The loop over `node` for `incarnation`, whose first sync uses `first_sync`: the
    /// controller's start-up syncs (§7.7) took the numbers before it.
    pub fn new(node: RaftNode<R>, incarnation: Incarnation, first_sync: SyncSeq) -> Self {
        let status = node.status();
        let applied = node.applied_index();
        Self {
            incarnation,
            next_sync: first_sync,
            phase: Phase::Idle,
            commit: status.commit,
            applied,
            status,
            node,
        }
    }

    /// The node.
    pub const fn node(&self) -> &RaftNode<R> {
        &self.node
    }

    /// The library's view of this node, as of the end of the last turn, or of the last Ready
    /// taken before a sync. Reading it never touches the library.
    pub const fn status(&self) -> &Status {
        &self.status
    }

    /// The last entry applied.
    pub const fn applied(&self) -> LogIndex {
        self.applied
    }

    /// The sync the outstanding Ready waits for, if any.
    pub const fn outstanding_sync(&self) -> Option<SyncSeq> {
        match &self.phase {
            Phase::Syncing { seq, .. } => Some(*seq),
            Phase::Idle | Phase::Stopped => None,
        }
    }

    /// Runs any ready work without feeding an input: at start-up, committed entries past the
    /// applied index arrive through the loop (§7.7 step 7). Does nothing while a sync is
    /// outstanding.
    ///
    /// # Errors
    ///
    /// [`LoopError`] at a hard stop, or once the loop has stopped.
    pub fn poll(
        &mut self,
        apply: &mut impl Apply,
        effects: &mut Vec<Effect>,
    ) -> Result<(), LoopError> {
        match &self.phase {
            Phase::Idle => self.drain(apply, effects),
            Phase::Syncing { .. } => Ok(()),
            Phase::Stopped => Err(LoopError::Stopped),
        }
    }

    /// Feeds one input to the library and runs the loop until it is idle again or waits for
    /// a sync (§7.4 steps 1 to 7).
    ///
    /// # Errors
    ///
    /// [`LoopError`] at a hard stop, or once the loop has stopped.
    pub fn feed(
        &mut self,
        input: RaftInput,
        apply: &mut impl Apply,
        effects: &mut Vec<Effect>,
    ) -> Result<Fed, LoopError> {
        match &self.phase {
            Phase::Idle => {}
            Phase::Syncing { .. } => return Ok(Fed::Deferred(input)),
            Phase::Stopped => return Err(LoopError::Stopped),
        }
        let fed = match input {
            RaftInput::Tick => {
                self.node.tick();
                Fed::Ticked
            }
            RaftInput::Peer(message) => Fed::Stepped(self.node.step(message)),
            RaftInput::Propose(command) => Fed::Proposed(self.node.propose(command)),
        };
        self.check_reads()?;
        self.drain(apply, effects)?;
        Ok(fed)
    }

    /// Handles a `SyncDone`. If it completes the outstanding sync, the Ready's entries
    /// become visible to the library, its messages are released and its committed entries
    /// applied, and the loop drains (§7.4 steps 4 to 7). One from another incarnation is
    /// ignored (§7.2).
    ///
    /// # Errors
    ///
    /// [`LoopError`] at a hard stop while draining, or once the loop has stopped.
    ///
    /// # Panics
    ///
    /// `RA-SYNC-UNEXPECTED` if it is this incarnation's, but not for the outstanding sync.
    pub fn sync_done(
        &mut self,
        seq: SyncSeq,
        incarnation: Incarnation,
        apply: &mut impl Apply,
        effects: &mut Vec<Effect>,
    ) -> Result<Synced, LoopError> {
        if incarnation != self.incarnation {
            return Ok(Synced::Stale);
        }
        match &self.phase {
            Phase::Idle | Phase::Syncing { .. } => {}
            Phase::Stopped => return Err(LoopError::Stopped),
        }
        invariant!(
            self.outstanding_sync() == Some(seq),
            "RA-SYNC-UNEXPECTED",
            "node {}: SyncDone for sync {} in incarnation {}, but the outstanding sync is {:?}",
            self.node.id(),
            seq.get(),
            incarnation.get(),
            self.outstanding_sync().map(SyncSeq::get)
        );
        let Phase::Syncing {
            messages,
            committed,
            ..
        } = std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return Ok(Synced::Completed);
        };
        self.node.synced();
        self.finish(messages, &committed, apply, effects)?;
        self.drain(apply, effects)?;
        Ok(Synced::Completed)
    }

    /// Takes Readys until the library has none, or one waits for its sync (steps 2 and 3).
    fn drain(
        &mut self,
        apply: &mut impl Apply,
        effects: &mut Vec<Effect>,
    ) -> Result<(), LoopError> {
        loop {
            let ready = self.node.ready();
            self.check_reads()?;
            let Some(ready) = ready else {
                self.status = self.node.status();
                return Ok(());
            };
            if let Some(state) = ready.hard_state() {
                self.commit = self.commit.max(state.commit);
            }
            // The view is still unsealed: read the status now, for lookups during the sync.
            self.status = self.node.status();
            let (_, _, messages, committed) = ready.into_parts();
            match self.node.persist() {
                Ok(Some(frame)) => {
                    let seq = self.next_sync;
                    let next = seq.checked_next();
                    invariant!(
                        next.is_some(),
                        "RA-SYNC-SEQ",
                        "node {}: sync sequence {} has no successor",
                        self.node.id(),
                        seq.get()
                    );
                    self.next_sync = next.unwrap_or(seq);
                    effects.push(Effect::Append { frame });
                    effects.push(Effect::Sync {
                        seq,
                        incarnation: self.incarnation,
                    });
                    self.phase = Phase::Syncing {
                        seq,
                        messages,
                        committed,
                    };
                    return Ok(());
                }
                Ok(None) => self.finish(messages, &committed, apply, effects)?,
                Err(error) => return Err(self.stop(&error)),
            }
        }
    }

    /// Steps 4 to 7 for a Ready whose frame, if any, has synced.
    fn finish(
        &mut self,
        messages: Vec<OutboundMessage>,
        committed: &[Entry],
        apply: &mut impl Apply,
        effects: &mut Vec<Effect>,
    ) -> Result<(), LoopError> {
        let reported = self.applied;
        send(messages, effects);
        self.apply_all(committed, apply, effects);
        let light = self.node.advance();
        self.check_reads()?;
        if let Some(commit) = light.commit {
            self.commit = self.commit.max(commit);
        }
        send(light.messages, effects);
        self.apply_all(&light.committed, apply, effects);
        if self.applied > reported {
            self.node.applied(self.applied);
        }
        self.check_reads()
    }

    /// Stops the loop if a journal read under the library failed. The library got only the
    /// entries before it, and nothing more is persisted, sent or applied (§7.11).
    fn check_reads(&mut self) -> Result<(), LoopError> {
        match self.node.storage_failure() {
            None => Ok(()),
            Some(failure) => {
                self.phase = Phase::Stopped;
                Err(LoopError::StorageFailed(failure))
            }
        }
    }

    /// Applies `entries` in index order (step 5).
    ///
    /// # Panics
    ///
    /// `RA-APPLY-ORDER` if an entry is not the one after the applied index, and
    /// `RA-APPLY-COMMITTED` if it lies beyond the commit index.
    fn apply_all(&mut self, entries: &[Entry], apply: &mut impl Apply, effects: &mut Vec<Effect>) {
        for entry in entries {
            let index = entry.index.index();
            invariant!(
                self.applied.checked_next() == Some(index),
                "RA-APPLY-ORDER",
                "node {}: entry {index} handed over for apply after applied index {}; \
                 committed entries apply once each, in index order",
                self.node.id(),
                self.applied
            );
            invariant!(
                index <= self.commit,
                "RA-APPLY-COMMITTED",
                "node {}: entry {index} handed over for apply beyond commit index {}; only \
                 the library's commit rule commits",
                self.node.id(),
                self.commit
            );
            apply.apply(entry, self.commit, effects);
            self.applied = index;
        }
    }

    /// The loop's answer to a refused frame: a hard stop stops it (DM-7); anything else is a
    /// broken invariant.
    ///
    /// # Panics
    ///
    /// `RA-FRAME-BOUND` if the Ready's batch overflows one frame: one input persists at most
    /// one append batch or one proposal, plus hard state (§7.4). `RA-HISTORY` if the library
    /// wrote an impossible history.
    fn stop(&mut self, error: &JournalError) -> LoopError {
        let (fits, possible) = match error {
            JournalError::HardStop { .. } => (true, true),
            JournalError::Frame(_) => (false, true),
            JournalError::Sequence { .. }
            | JournalError::Offset { .. }
            | JournalError::Gap { .. }
            | JournalError::TruncatesCommitted { .. }
            | JournalError::CommitBeyondLog { .. }
            | JournalError::ZeroTerm { .. }
            | JournalError::TermDecreases { .. }
            | JournalError::TermAhead { .. }
            | JournalError::TermRegression { .. }
            | JournalError::CommitRegression { .. }
            | JournalError::VoteChanged { .. }
            | JournalError::VoteForNonVoter { .. }
            | JournalError::NotInLog { .. }
            | JournalError::NotRaftBatch { .. }
            | JournalError::EntryMismatch { .. }
            | JournalError::Record(_)
            | JournalError::Storage(_) => (true, false),
        };
        invariant!(
            fits,
            "RA-FRAME-BOUND",
            "node {}: a Ready's batch overflows one frame ({error}); one input persists at \
             most one append batch or one proposal, plus hard state (§7.4)",
            self.node.id()
        );
        invariant!(
            possible,
            "RA-HISTORY",
            "node {}: the library wrote a history the journal refuses: {error}",
            self.node.id()
        );
        self.phase = Phase::Stopped;
        let JournalError::HardStop {
            end,
            frame_len,
            limit,
        } = error
        else {
            return LoopError::Stopped;
        };
        LoopError::HardStop {
            end: *end,
            frame_len: *frame_len,
            limit: *limit,
        }
    }
}

/// Sends `messages` in their recorded order (steps 4 and 6).
fn send(messages: Vec<OutboundMessage>, effects: &mut Vec<Effect>) {
    effects.extend(messages.into_iter().map(|message| Effect::Send {
        to: message.to,
        bytes: message.bytes,
    }));
}
