//! [`RaftNode`]: the Consensus port over the vendored `RawNode` (PLAN.md §7.2, §7.3, §7.4).

use journal::{JournalError, LogicalLog, RaftBatch};
use raft::{RawNode, StateRole};
use types::{
    Consensus, ConsensusSettings, Entropy, Entry, HardState, JournalRead, LightReady, LogIndex,
    NodeId, OutboundMessage, Progress, ProposeError, Ready, Role, Status, StepError, Term,
    invariant,
};

use crate::config::library_config;
use crate::convert::{decode_message, entries_from_library, hard_state_from_library, outbound};
use crate::entropy::EntropySource;
use crate::storage::JournalStorage;

/// Why a node could not be constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstructError {
    /// The applied index is past the recovered commit index (§7.7 steps 5 and 6).
    AppliedPastCommit {
        /// The applied index asked for.
        applied: LogIndex,
        /// The recovered commit index.
        commit: LogIndex,
    },
    /// The library refused the settings. The parser mirrors the library's own validation,
    /// so this never happens for a parsed config (D-78); `reason` is the library's text.
    Refused {
        /// The library's reason.
        reason: String,
    },
}

/// The Ready taken but not yet advanced past, and the frame it still owes.
#[derive(Debug)]
struct Outstanding {
    /// The library's Ready, kept for `advance`.
    ready: raft::Ready,
    /// Its entries and hard state, until [`RaftNode::persist`] writes them; `None` once
    /// written, or when the Ready needs no sync.
    batch: Option<RaftBatch>,
}

/// The Consensus port over the vendored `RawNode`, with the journal's logical log as the
/// library's storage view (§7.3).
///
/// Invariants: at most one Ready is outstanding; it is advanced past only after its frame
/// was persisted and synced (§7.4 steps 3 and 4); no library type crosses the port (CD-1).
///
/// [`ReadyLoop`](crate::ReadyLoop) drives it through the §7.4 steps; the methods here are
/// the steps' building blocks.
pub struct RaftNode<R: JournalRead> {
    /// This node.
    node: NodeId,
    /// The library.
    raw: RawNode<JournalStorage<R>>,
    /// The outstanding Ready, if any.
    outstanding: Option<Outstanding>,
}

impl<R: JournalRead> std::fmt::Debug for RaftNode<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaftNode")
            .field("node", &self.node)
            .field("outstanding", &self.outstanding.is_some())
            .finish_non_exhaustive()
    }
}

impl<R: JournalRead> RaftNode<R> {
    /// The node `settings` describe, among `voters`, over its recovered `log` (read back
    /// through `reader`), drawing its election timeouts from `entropy`. Entries through
    /// `applied` were already replayed into the state machine (§7.7 steps 5 and 6).
    ///
    /// # Errors
    ///
    /// [`ConstructError`] if `applied` is past the recovered commit index, or if the
    /// library refuses the settings, which never happens for a parsed config (D-78).
    pub fn new<E: Entropy + Send + 'static>(
        settings: &ConsensusSettings,
        voters: impl IntoIterator<Item = NodeId>,
        log: LogicalLog,
        reader: R,
        entropy: E,
        applied: LogIndex,
    ) -> Result<Self, ConstructError> {
        let commit = log.hard_state().commit;
        if applied > commit {
            return Err(ConstructError::AppliedPastCommit { applied, commit });
        }
        let storage = JournalStorage::new(settings.node_id(), log, reader, voters);
        // An explicit logger with a discarding drain: no background thread, no output (§7.3).
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let raw = RawNode::new(
            &library_config(settings, applied),
            storage,
            EntropySource::new(entropy),
            &logger,
        )
        .map_err(|err| ConstructError::Refused {
            reason: err.to_string(),
        })?;
        Ok(Self {
            node: settings.node_id(),
            raw,
            outstanding: None,
        })
    }

    /// This node.
    pub const fn id(&self) -> NodeId {
        self.node
    }

    /// The library's storage view: the logical log.
    pub fn storage(&self) -> &JournalStorage<R> {
        self.raw.store()
    }

    /// The index the library holds as applied: the replayed index at construction, then
    /// whatever [`applied`](Consensus::applied) last reported.
    pub fn applied_index(&self) -> LogIndex {
        log_index(self.raw.raft.raft_log.applied)
    }

    /// Writes the outstanding Ready's entries and hard state as the journal's next frame,
    /// returning the frame's bytes for an `Effect::Append`, or `None` if the Ready needs no
    /// sync (the commit index alone is never worth a frame, §7.4 step 3) or was already
    /// written. A written frame seals the storage view until [`synced`](Self::synced).
    ///
    /// # Errors
    ///
    /// The journal's refusal, such as its hard stop (DM-7); nothing is written.
    ///
    /// # Panics
    ///
    /// `RA-PERSIST-WITHOUT-READY` if no Ready is outstanding.
    pub fn persist(&mut self) -> Result<Option<Vec<u8>>, JournalError> {
        invariant!(
            self.outstanding.is_some(),
            "RA-PERSIST-WITHOUT-READY",
            "node {}: persist with no outstanding Ready (§7.4 step 3)",
            self.node
        );
        let Some(batch) = self
            .outstanding
            .as_mut()
            .and_then(|outstanding| outstanding.batch.take())
        else {
            return Ok(None);
        };
        match self.raw.mut_store().log_mut().append(batch.clone()) {
            Ok(frame) => {
                self.raw.mut_store().seal();
                Ok(Some(frame))
            }
            Err(err) => {
                // Nothing was written, so the frame is still owed.
                if let Some(outstanding) = self.outstanding.as_mut() {
                    outstanding.batch = Some(batch);
                }
                Err(err)
            }
        }
    }

    /// Reports that the outstanding Ready's frame is durable (its `SyncDone` arrived): its
    /// entries become visible to the library's storage view (§7.4 step 4).
    ///
    /// # Panics
    ///
    /// `RA-SYNCED-WITHOUT-FRAME` if no written frame is waiting for its sync.
    pub fn synced(&mut self) {
        invariant!(
            self.raw.store().is_sealed(),
            "RA-SYNCED-WITHOUT-FRAME",
            "node {}: a sync was reported with no written frame waiting for it",
            self.node
        );
        self.raw.mut_store().expose();
    }

    /// Asserts that the library may take `input`: no Ready is outstanding. raft-rs forbids any
    /// input between `ready()` and `advance` (`raw_node.rs`), and the loop feeds none until the
    /// Ready is done (§7.4 step 3).
    ///
    /// # Panics
    ///
    /// `RA-INPUT-DURING-READY` if a Ready is outstanding.
    fn check_no_ready(&self, input: &str) {
        invariant!(
            self.outstanding.is_none(),
            "RA-INPUT-DURING-READY",
            "node {}: {input} fed while a Ready is outstanding; the library takes no input until \
             the Ready is advanced (§7.4 step 3)",
            self.node
        );
    }

    /// Library entries as core entries.
    ///
    /// # Panics
    ///
    /// `RA-ENTRY` if the library produced one v1 cannot hold: index 0, a term out of range or a
    /// configuration change (membership is static, §7.7 step 6).
    fn core_entries(&self, entries: &[raft::eraftpb::Entry], what: &str) -> Vec<Entry> {
        let converted = entries_from_library(entries);
        invariant!(
            converted.is_some(),
            "RA-ENTRY",
            "node {}: the library produced {what} v1 cannot hold: {:?}",
            self.node,
            entries
                .iter()
                .map(|entry| (entry.index, entry.term, entry.get_entry_type()))
                .collect::<Vec<_>>()
        );
        converted.unwrap_or_default()
    }

    /// Library messages as outbound core messages, in their order.
    ///
    /// # Panics
    ///
    /// `RA-MESSAGE` if one has no valid destination or does not encode.
    fn core_messages(&self, messages: &[raft::eraftpb::Message]) -> Vec<OutboundMessage> {
        let converted: Option<Vec<OutboundMessage>> = messages.iter().map(outbound).collect();
        invariant!(
            converted.is_some(),
            "RA-MESSAGE",
            "node {}: the library produced a message with no valid destination or encoding: {:?}",
            self.node,
            messages
                .iter()
                .map(|message| message.to)
                .collect::<Vec<_>>()
        );
        converted.unwrap_or_default()
    }

    /// A library hard state as a core one.
    ///
    /// # Panics
    ///
    /// `RA-HARD-STATE` if its term or commit index is out of range.
    fn core_hard_state(&self, state: &raft::eraftpb::HardState) -> HardState {
        let converted = hard_state_from_library(state);
        invariant!(
            converted.is_some(),
            "RA-HARD-STATE",
            "node {}: the library produced an out-of-range hard state: term {} commit {}",
            self.node,
            state.term,
            state.commit
        );
        converted.unwrap_or(HardState {
            term: Term::ZERO,
            vote: None,
            commit: LogIndex::ZERO,
        })
    }
}

/// A library index as a core one; library indices stay within 2^53 − 1 because entries do.
fn log_index(raw: u64) -> LogIndex {
    LogIndex::new(raw).unwrap_or(LogIndex::ZERO)
}

impl<R: JournalRead> Consensus for RaftNode<R> {
    /// # Panics
    ///
    /// `RA-INPUT-DURING-READY` if a Ready is outstanding.
    fn propose(&mut self, command: Vec<u8>) -> Result<(), ProposeError> {
        self.check_no_ready("a proposal");
        match self.raw.raft.state {
            StateRole::Leader => self
                .raw
                .propose(Vec::new(), command)
                .map_err(|_| ProposeError::Dropped),
            StateRole::Follower | StateRole::Candidate | StateRole::PreCandidate => {
                Err(ProposeError::NotLeader)
            }
        }
    }

    /// # Panics
    ///
    /// `RA-INPUT-DURING-READY` if a Ready is outstanding.
    fn step(&mut self, message: Vec<u8>) -> Result<(), StepError> {
        self.check_no_ready("a peer message");
        let message = decode_message(&message).ok_or(StepError::Rejected)?;
        self.raw.step(message).map_err(|_| StepError::Rejected)
    }

    /// # Panics
    ///
    /// `RA-INPUT-DURING-READY` if a Ready is outstanding.
    fn tick(&mut self) {
        self.check_no_ready("a tick");
        // Whether the tick changed anything shows up in the next Ready.
        let _changed = self.raw.tick();
    }

    /// # Panics
    ///
    /// `RA-ONE-READY` if a Ready is already outstanding (§7.4: one at a time);
    /// `RA-NO-SNAPSHOT` if the library hands over a snapshot (v1 never compacts, §7.4 step 2).
    fn ready(&mut self) -> Option<Ready> {
        invariant!(
            self.outstanding.is_none(),
            "RA-ONE-READY",
            "node {}: a second Ready was asked for before the first was advanced (§7.4)",
            self.node
        );
        if !self.raw.has_ready() {
            return None;
        }
        let mut ready = self.raw.ready();
        invariant!(
            ready.snapshot().get_metadata().index == 0,
            "RA-NO-SNAPSHOT",
            "node {}: the library handed over a snapshot at index {} (v1 never compacts, §7.4 \
             step 2)",
            self.node,
            ready.snapshot().get_metadata().index
        );
        let entries = self.core_entries(&ready.take_entries(), "a new entry");
        let hard_state = ready.hs().map(|state| self.core_hard_state(state));
        let must_sync = ready.must_sync();
        // v1 holds every message until the sync, ordinary ones first, as recorded (§7.4 step 4).
        let mut messages = ready.take_messages();
        messages.extend(ready.take_persisted_messages());
        let messages = self.core_messages(&messages);
        let committed = self.core_entries(&ready.take_committed_entries(), "a committed entry");
        let batch = if must_sync {
            let batch = RaftBatch::new(entries.clone(), hard_state);
            invariant!(
                batch.is_ok(),
                "RA-BATCH",
                "node {}: a Ready that must sync is no journal batch: {:?}",
                self.node,
                batch.as_ref().err()
            );
            batch.ok()
        } else {
            None
        };
        self.outstanding = Some(Outstanding { ready, batch });
        Some(Ready::new(
            entries, hard_state, must_sync, messages, committed,
        ))
    }

    /// # Panics
    ///
    /// `RA-ADVANCE-WITHOUT-READY` if no Ready is outstanding; `RA-ADVANCE-UNPERSISTED` if its
    /// frame was not persisted first, and `RA-ADVANCE-UNSYNCED` if it was not synced
    /// (§7.4 steps 3 and 4).
    fn advance(&mut self) -> LightReady {
        let outstanding = self.outstanding.take();
        invariant!(
            outstanding.is_some(),
            "RA-ADVANCE-WITHOUT-READY",
            "node {}: advance with no outstanding Ready",
            self.node
        );
        let Some(Outstanding { ready, batch }) = outstanding else {
            return LightReady {
                commit: None,
                messages: Vec::new(),
                committed: Vec::new(),
            };
        };
        invariant!(
            batch.is_none(),
            "RA-ADVANCE-UNPERSISTED",
            "node {}: advance before the Ready's frame was persisted (§7.4 steps 3 and 4)",
            self.node
        );
        invariant!(
            !self.raw.store().is_sealed(),
            "RA-ADVANCE-UNSYNCED",
            "node {}: advance before the Ready's frame was synced (§7.4 step 4)",
            self.node
        );
        // advance_append, not advance: applied progress is reported only for entries actually
        // applied, through `applied` (§7.4 step 7).
        let mut light = self.raw.advance_append(ready);
        LightReady {
            commit: light.commit_index().map(log_index),
            messages: self.core_messages(&light.take_messages()),
            committed: self.core_entries(&light.take_committed_entries(), "a committed entry"),
        }
    }

    fn applied(&mut self, index: LogIndex) {
        self.raw.advance_apply_to(index.get());
    }

    fn status(&self) -> Status {
        let status = self.raw.status();
        let role = match status.ss.raft_state {
            StateRole::Follower => Role::Follower,
            StateRole::PreCandidate => Role::PreCandidate,
            StateRole::Candidate => Role::Candidate,
            StateRole::Leader => Role::Leader,
        };
        let mut progress: Vec<Progress> = status
            .progress
            .map(|tracker| {
                tracker
                    .iter()
                    .filter_map(|(id, pr)| {
                        Some(Progress {
                            node: NodeId::new(*id).ok()?,
                            matched: log_index(pr.matched),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        progress.sort_by_key(|entry| entry.node);
        Status {
            role,
            term: Term::new(status.hs.term).unwrap_or(Term::ZERO),
            leader: NodeId::new(status.ss.leader_id).ok(),
            commit: log_index(status.hs.commit),
            last: log_index(self.raw.raft.raft_log.last_index()),
            progress,
        }
    }
}
