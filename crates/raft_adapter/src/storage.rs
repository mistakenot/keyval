//! The library's storage view: the journal's logical log (PLAN.md §7.4 step 4, §7.7 step 6).

use std::cell::{Ref, RefCell};

use journal::LogicalLog;
use protobuf::Message as _;
use raft::eraftpb::{self, ConfState};
use raft::{GetEntriesContext, RaftState, StorageError};
use types::{EntryIndex, JournalRead, NodeId};

use crate::convert::{entry_to_library, hard_state_to_library};

/// `raft::Storage` over the journal's [`LogicalLog`]: the offset index, the bounded entry cache,
/// and bounded reads through the in-process [`JournalRead`] port on a cache miss.
///
/// - **`initial_state`:** the recovered hard state, plus a `ConfState` from the static voter
///   set. Membership never comes from the log in v1 (§7.7 step 6).
/// - **Never compacts:** the first index is always 1, so the library never asks for a
///   snapshot.
///
/// Invariant: the voter set is sorted and holds no duplicates.
#[derive(Debug)]
pub struct JournalStorage<R> {
    /// The logical log. Library reads take `&self`, but a cache miss fills the cache, so the
    /// log sits in a cell. The adapter is single-threaded (§7.14).
    log: RefCell<LogicalLog>,
    /// Bounded journal reads, for cache misses.
    reader: R,
    /// The configured voters, sorted.
    voters: Vec<u64>,
}

impl<R> JournalStorage<R> {
    /// The storage view of the recovered `log`, read through `reader`, for `voters`.
    pub fn new(log: LogicalLog, reader: R, voters: impl IntoIterator<Item = NodeId>) -> Self {
        let mut voters: Vec<u64> = voters.into_iter().map(NodeId::get).collect();
        voters.sort_unstable();
        voters.dedup();
        Self {
            log: RefCell::new(log),
            reader,
            voters,
        }
    }

    /// The logical log.
    pub fn log(&self) -> Ref<'_, LogicalLog> {
        self.log.borrow()
    }

    /// The logical log, to append to.
    pub fn log_mut(&mut self) -> &mut LogicalLog {
        self.log.get_mut()
    }
}

/// The library's "not in storage" error.
const fn unavailable() -> raft::Error {
    raft::Error::Store(StorageError::Unavailable)
}

impl<R: JournalRead> raft::Storage for JournalStorage<R> {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(RaftState {
            hard_state: hard_state_to_library(self.log.borrow().hard_state()),
            conf_state: ConfState::from((self.voters.clone(), Vec::<u64>::new())),
        })
    }

    /// Entries `[low, high)`, at least one, then as many more as fit `max_size` bytes (the
    /// library's own size rule). A read that fails is `Unavailable`; the library treats that as
    /// fatal, and the node fails (§7.11).
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> raft::Result<Vec<eraftpb::Entry>> {
        let max_size = max_size.into();
        let mut log = self.log.borrow_mut();
        let mut entries = Vec::new();
        let mut size = 0_u64;
        for raw in low..high {
            let index = EntryIndex::new(raw).map_err(|_| unavailable())?;
            let entry = log.entry(index, &self.reader).map_err(|_| unavailable())?;
            let library = entry_to_library(&entry);
            size = size.saturating_add(u64::from(library.compute_size()));
            if !entries.is_empty() && max_size.is_some_and(|max| size > max) {
                break;
            }
            entries.push(library);
        }
        Ok(entries)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        if idx == 0 {
            return Ok(0);
        }
        let index = EntryIndex::new(idx).map_err(|_| unavailable())?;
        self.log
            .borrow()
            .term(index)
            .map(types::Term::get)
            .ok_or_else(unavailable)
    }

    fn first_index(&self) -> raft::Result<u64> {
        Ok(1)
    }

    fn last_index(&self) -> raft::Result<u64> {
        Ok(self.log.borrow().last_index().get())
    }

    /// v1 never compacts, so no follower ever needs a snapshot; were one asked for, the library
    /// would wait and ask again.
    fn snapshot(&self, _request_index: u64, _to: u64) -> raft::Result<eraftpb::Snapshot> {
        Err(raft::Error::Store(
            StorageError::SnapshotTemporarilyUnavailable,
        ))
    }
}

#[cfg(test)]
mod tests {
    use raft::Storage as _;
    use types::{Entry, EntryPayload, HardState, LogIndex, Term};

    use super::*;
    use crate::test_support::{Journal, empty_log, entry, persist_batch};

    #[test]
    fn initial_state_is_the_recovered_hard_state_and_the_static_voters() {
        let journal = Journal::default();
        let mut log = empty_log();
        let state = HardState {
            term: Term::new(3).unwrap(),
            vote: NodeId::new(2).ok(),
            commit: LogIndex::new(1).unwrap(),
        };
        persist_batch(&mut log, &journal, vec![entry(1, 3, b"cmd")], Some(state));
        let voters = [3, 1, 2].map(|n| NodeId::new(n).unwrap());
        let storage = JournalStorage::new(log, journal, voters);
        let initial = storage.initial_state().unwrap();
        assert_eq!(
            initial.hard_state,
            hard_state_to_library(state),
            "rule: initial_state reports the recovered hard state (§7.7 step 6)"
        );
        assert_eq!(
            initial.conf_state.voters,
            vec![1, 2, 3],
            "rule: membership is the static, sorted voter set, never the log"
        );
        assert!(
            initial.conf_state.learners.is_empty(),
            "rule: v1 has no learners"
        );
    }

    #[test]
    fn reads_serve_the_logical_log_through_the_cache_or_the_journal() {
        let journal = Journal::default();
        let mut log = empty_log();
        let entries: Vec<Entry> = (1..=4)
            .map(|i| entry(i, 1 + i / 3, b"0123456789"))
            .collect();
        // A batch's hard state must cover its entries' terms (TermAhead otherwise).
        let state = HardState {
            term: Term::new(2).unwrap(),
            vote: None,
            commit: LogIndex::ZERO,
        };
        persist_batch(&mut log, &journal, entries.clone(), Some(state));
        let storage = JournalStorage::new(log, journal, [NodeId::new(1).unwrap()]);
        assert_eq!(
            (
                storage.first_index().unwrap(),
                storage.last_index().unwrap()
            ),
            (1, 4),
            "rule: v1 never compacts, so the log starts at 1"
        );
        assert_eq!(
            (1..=4)
                .map(|i| storage.term(i).unwrap())
                .collect::<Vec<_>>(),
            vec![1, 1, 2, 2],
            "rule: terms come from the offset index"
        );
        assert_eq!(
            storage.term(0).unwrap(),
            0,
            "rule: the dummy index 0 has term 0"
        );
        assert!(
            storage.term(5).is_err(),
            "rule: past the last index is unavailable"
        );
        let all = storage
            .entries(1, 5, None, GetEntriesContext::empty(false))
            .unwrap();
        assert_eq!(
            all,
            entries.iter().map(entry_to_library).collect::<Vec<_>>(),
            "rule: entries come back exactly as persisted"
        );
        let one_size = u64::from(all[0].compute_size());
        for (max, expected) in [(0, 1), (one_size, 1), (2 * one_size, 2), (u64::MAX, 4)] {
            let got = storage
                .entries(1, 5, max, GetEntriesContext::empty(false))
                .unwrap();
            assert_eq!(
                got.len(),
                expected,
                "rule: at least one entry, then as many as fit max_size; max={max}"
            );
        }
        assert!(
            storage.snapshot(1, 2).is_err(),
            "rule: v1 never compacts, so there is no snapshot"
        );
        assert_eq!(
            entries[0].payload,
            EntryPayload::Command(b"0123456789".to_vec()),
            "fixture sanity"
        );
    }
}
