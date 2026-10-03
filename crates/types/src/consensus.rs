//! The Consensus port: the narrow, library-neutral boundary around the Raft library
//! (PLAN.md §7.2, §7.3, D-05). The engine calls it in-process; W13's `raft-rs`
//! adapter implements it.
//!
//! No library type crosses this port (CD-1): messages travel as encoded bytes, and
//! entries, hard state and status are core types. Replacing the library must not
//! change anything on this side of the port.

use crate::ids::NodeId;
use crate::log_index::{EntryIndex, LogIndex, Term};

/// A node's Raft role (O-1, `/v1/cluster`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Following a leader, or waiting for one.
    Follower,
    /// Asking for pre-votes before starting an election (pre-vote is on, §7.10).
    PreCandidate,
    /// Asking for votes in a new term.
    Candidate,
    /// The term's leader.
    Leader,
}

impl Role {
    /// The role as the API spells it (O-1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follower => "follower",
            Self::PreCandidate => "pre_candidate",
            Self::Candidate => "candidate",
            Self::Leader => "leader",
        }
    }
}

/// What a log entry carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryPayload {
    /// The leader's no-op on election: advances `applied_index` only (§7.5 apply step 1).
    Empty,
    /// One canonical, encoded command (§7.5), at most 66 KiB (§7.11).
    Command(Vec<u8>),
}

/// One Raft log entry.
///
/// Invariant: `index` is at least 1, by its type (index 0 holds no entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry's log index.
    pub index: EntryIndex,
    /// The term it was proposed in.
    pub term: Term,
    /// What it carries.
    pub payload: EntryPayload,
}

/// The Raft state a node must persist before sending anything that depends on it
/// (DUR-2, §7.4).
///
/// Invariant: `commit` never exceeds the node's last log index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardState {
    /// The current term.
    pub term: Term,
    /// The node voted for in `term`, if any.
    pub vote: Option<NodeId>,
    /// The commit index. It is volatile: persisted opportunistically, never on its own
    /// (§7.4 step 3).
    pub commit: LogIndex,
}

/// An encoded peer envelope addressed to a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundMessage {
    /// The destination node.
    pub to: NodeId,
    /// The encoded envelope (§8.2), bounded by the batch limits of §7.11.
    pub bytes: Vec<u8>,
}

/// One batch of work from the library (§7.4 step 2).
///
/// Invariants:
/// - never a snapshot (v1 never compacts; the adapter asserts it), at most one is
///   outstanding, and its messages are released only after its frame syncs (step 4);
/// - new entries always need a sync: a Ready that skipped the frame would release append
///   acknowledgements over unsynced entries (§7.4: never on unsynced state);
/// - a sync always has something to persist: entries, or a hard state.
///
/// [`Ready::new`] asserts both of the last two (CD-7); the fields are private.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    /// New entries to append, in index order.
    entries: Vec<Entry>,
    /// Hard state to persist, if it changed.
    hard_state: Option<HardState>,
    /// Whether anything here must be persisted before step 4.
    must_sync: bool,
    /// Outgoing messages, released in this order after the sync.
    messages: Vec<OutboundMessage>,
    /// Newly committed entries to apply, in index order (§7.4 step 5).
    committed: Vec<Entry>,
}

impl Ready {
    /// A Ready from the library's output. When `must_sync` is false no frame is written:
    /// the commit index alone is never worth a frame (§7.4 step 3).
    ///
    /// # Panics
    ///
    /// On a broken library promise (CD-7): new entries without `must_sync` (invariant
    /// `READY-ENTRIES-SYNC`), or `must_sync` with neither entries nor a hard state
    /// (`READY-SYNC-STATE`).
    #[must_use]
    pub fn new(
        entries: Vec<Entry>,
        hard_state: Option<HardState>,
        must_sync: bool,
        messages: Vec<OutboundMessage>,
        committed: Vec<Entry>,
    ) -> Self {
        crate::invariant!(
            must_sync || entries.is_empty(),
            "READY-ENTRIES-SYNC",
            "{} new entries but must_sync is false (first index {:?})",
            entries.len(),
            entries.first().map(|entry| entry.index)
        );
        crate::invariant!(
            !must_sync || !entries.is_empty() || hard_state.is_some(),
            "READY-SYNC-STATE",
            "must_sync with no entries and no hard state"
        );
        Self {
            entries,
            hard_state,
            must_sync,
            messages,
            committed,
        }
    }

    /// New entries to append, in index order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Hard state to persist, if it changed.
    #[must_use]
    pub const fn hard_state(&self) -> Option<HardState> {
        self.hard_state
    }

    /// Whether anything here must be persisted before step 4.
    #[must_use]
    pub const fn must_sync(&self) -> bool {
        self.must_sync
    }

    /// Outgoing messages, released in this order after the sync.
    #[must_use]
    pub fn messages(&self) -> &[OutboundMessage] {
        &self.messages
    }

    /// Newly committed entries to apply, in index order.
    #[must_use]
    pub fn committed(&self) -> &[Entry] {
        &self.committed
    }

    /// The Ready's parts, by value: new entries, hard state, messages and committed
    /// entries. Read [`must_sync`](Self::must_sync) first.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<Entry>,
        Option<HardState>,
        Vec<OutboundMessage>,
        Vec<Entry>,
    ) {
        (self.entries, self.hard_state, self.messages, self.committed)
    }
}

/// What advancing past a Ready returns (§7.4 step 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightReady {
    /// A newer commit index, if the library learned one.
    pub commit: Option<LogIndex>,
    /// Messages whose persistence obligations are already met.
    pub messages: Vec<OutboundMessage>,
    /// Further committed entries to apply, in index order.
    pub committed: Vec<Entry>,
}

/// One voter's replication progress, as the leader tracks it (O-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The voter.
    pub node: NodeId,
    /// The highest index known to be replicated on it.
    pub matched: LogIndex,
}

/// The library's view of this node (O-1, §7.3 "query role, term, leader and indices").
///
/// Invariants: `commit <= last`; `progress` is empty unless `role` is `Leader`, and then
/// holds at most one entry per configured voter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// This node's role.
    pub role: Role,
    /// Its current term.
    pub term: Term,
    /// The leader it believes in, if any.
    pub leader: Option<NodeId>,
    /// Its commit index.
    pub commit: LogIndex,
    /// The last index in its log.
    pub last: LogIndex,
    /// Replication progress per voter, when leading.
    pub progress: Vec<Progress>,
}

/// Why a proposal was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposeError {
    /// The library dropped it synchronously with nothing appended (`ProposalDropped`):
    /// the attempt was never submitted and is answered `busy` (§7.8, A-5).
    Dropped,
    /// This node is not the leader.
    NotLeader,
}

/// Why a peer message could not be stepped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepError {
    /// The bytes are not a valid message, or come from an unknown peer. The message is
    /// dropped and counted (§8.2).
    Rejected,
}

/// The Consensus port (§7.3). The engine drives it from its single serial controller,
/// feeding at most one input between Readys (§7.4).
pub trait Consensus {
    /// Proposes one encoded command (≤ 66 KiB).
    ///
    /// # Errors
    ///
    /// [`ProposeError`] if the library refused it with nothing appended.
    fn propose(&mut self, command: Vec<u8>) -> Result<(), ProposeError>;

    /// Steps one peer message, already checked by the envelope codec (§8.2).
    ///
    /// # Errors
    ///
    /// [`StepError::Rejected`] if the library rejects it.
    fn step(&mut self, message: Vec<u8>) -> Result<(), StepError>;

    /// Advances the library's logical clock by one tick.
    fn tick(&mut self);

    /// Takes the next Ready, if the library has work. At most one is outstanding.
    fn ready(&mut self) -> Option<Ready>;

    /// Reports the outstanding Ready persisted and its messages released, and returns
    /// the follow-up work (§7.4 step 6).
    fn advance(&mut self) -> LightReady;

    /// Reports that entries through `index` were applied (§7.4 step 7).
    fn applied(&mut self, index: LogIndex);

    /// The library's view of this node.
    fn status(&self) -> Status;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry at `index` in term 1.
    fn entry_at(index: u64) -> Entry {
        Entry {
            index: EntryIndex::new(index).unwrap(),
            term: Term::new(1).unwrap(),
            payload: EntryPayload::Empty,
        }
    }

    /// A hard state in term 1 with no vote and commit 0.
    fn hard_state() -> HardState {
        HardState {
            term: Term::new(1).unwrap(),
            vote: None,
            commit: LogIndex::ZERO,
        }
    }

    #[test]
    fn a_ready_is_valid_with_entries_with_hard_state_or_with_neither_unsynced() {
        let cases = [
            ("entries, synced", vec![entry_at(1)], None, true),
            (
                "a term or vote change, synced",
                Vec::new(),
                Some(hard_state()),
                true,
            ),
            (
                "a commit-only change, not synced",
                Vec::new(),
                Some(hard_state()),
                false,
            ),
            ("nothing to persist, not synced", Vec::new(), None, false),
        ];
        for (case, entries, state, must_sync) in cases {
            let ready = Ready::new(entries.clone(), state, must_sync, Vec::new(), Vec::new());
            assert_eq!(
                (ready.entries(), ready.hard_state(), ready.must_sync()),
                (entries.as_slice(), state, must_sync),
                "rule: a valid Ready keeps what it was given (§7.4); case={case}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "invariant READY-ENTRIES-SYNC violated")]
    fn new_entries_without_a_sync_are_an_invariant_failure() {
        // The core would write no frame and release append acks over unsynced entries.
        let _ = Ready::new(vec![entry_at(1)], None, false, Vec::new(), Vec::new());
    }

    #[test]
    #[should_panic(expected = "invariant READY-SYNC-STATE violated")]
    fn a_sync_with_nothing_to_persist_is_an_invariant_failure() {
        let _ = Ready::new(Vec::new(), None, true, Vec::new(), Vec::new());
    }

    #[test]
    fn roles_are_spelt_as_the_api() {
        let roles = [
            Role::Leader,
            Role::Follower,
            Role::Candidate,
            Role::PreCandidate,
        ];
        let names: Vec<&str> = roles.iter().map(|role| role.as_str()).collect();
        assert_eq!(
            names,
            ["leader", "follower", "candidate", "pre_candidate"],
            "rule: the API's Role enum (O-1, W03.1 reading 3)"
        );
    }

    /// A single-voter stand-in: the port is implementable with core types only (CD-1).
    struct Solo {
        log: Vec<Entry>,
        outstanding: bool,
    }

    impl Consensus for Solo {
        fn propose(&mut self, command: Vec<u8>) -> Result<(), ProposeError> {
            let index = EntryIndex::new(u64::try_from(self.log.len()).unwrap() + 1).unwrap();
            self.log.push(Entry {
                index,
                term: Term::new(1).unwrap(),
                payload: EntryPayload::Command(command),
            });
            Ok(())
        }

        fn step(&mut self, _message: Vec<u8>) -> Result<(), StepError> {
            Err(StepError::Rejected)
        }

        fn tick(&mut self) {}

        fn ready(&mut self) -> Option<Ready> {
            if self.outstanding || self.log.is_empty() {
                return None;
            }
            self.outstanding = true;
            Some(Ready::new(
                self.log.clone(),
                None,
                true,
                Vec::new(),
                self.log.clone(),
            ))
        }

        fn advance(&mut self) -> LightReady {
            self.outstanding = false;
            self.log.clear();
            LightReady {
                commit: None,
                messages: Vec::new(),
                committed: Vec::new(),
            }
        }

        fn applied(&mut self, _index: LogIndex) {}

        fn status(&self) -> Status {
            Status {
                role: Role::Leader,
                term: Term::new(1).unwrap(),
                leader: NodeId::new(1).ok(),
                commit: LogIndex::ZERO,
                last: LogIndex::ZERO,
                progress: Vec::new(),
            }
        }
    }

    #[test]
    fn one_ready_is_outstanding_at_a_time() {
        let mut solo = Solo {
            log: Vec::new(),
            outstanding: false,
        };
        assert_eq!(solo.ready(), None, "rule: no work, no Ready");
        solo.propose(b"put".to_vec()).unwrap();
        let ready = solo.ready().unwrap();
        assert_eq!(
            ready.entries().len(),
            1,
            "rule: the proposal is in the Ready (§7.4 step 2)"
        );
        assert!(
            ready.must_sync(),
            "rule: new entries must be persisted (§7.4 step 3)"
        );
        assert_eq!(
            solo.ready(),
            None,
            "rule: at most one outstanding Ready (§7.4)"
        );
        let light = solo.advance();
        assert!(
            light.committed.is_empty(),
            "rule: advance returns only follow-up work"
        );
        assert_eq!(
            solo.status().role.as_str(),
            "leader",
            "rule: status reports the role (§7.3)"
        );
    }
}
