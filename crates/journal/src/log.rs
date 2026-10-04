//! The logical log (PLAN.md §7.6): what the journal's frames mean once applied in
//! order. It holds the offset index, the persisted hard state and a bounded entry cache.
//!
//! Live appends ([`LogicalLog::append`]) and recovery replay (W11.3) apply frames
//! through the same [`LogicalLog::apply`], so the two can never disagree about what a
//! journal means (CD-10).
//!
//! - **Logical truncation.** A Raft batch whose first entry index is at or below the
//!   last index replaces the logical log from that index. The old bytes stay in the file,
//!   which is append-only. Replacing at or below the persisted commit index is refused.
//! - **Possible histories only** (§7.6 "an impossible index relationship", DUR-2). A
//!   persisted term or commit never goes back, a vote never changes within its term, and
//!   entry terms are at least 1, never decrease along the log and never exceed the term
//!   in force. A correct Raft writer never breaks these, so on the live path a refusal is
//!   a writer bug, and in recovery it is damage: refuse.
//! - **Hard stop** (DM-7). [`LogicalLog::append`] refuses a frame that would take the
//!   journal past `max_journal_bytes`, before changing anything. That refusal is the
//!   node's input to `failed`.
//! - **Offset index.** `LogIndex → (frame offset, term)`, about 16 bytes per entry,
//!   bounded by DM-7's admission ceiling.
//! - **Entry cache.** Recent entries up to a byte bound (`Limit::EntryCacheBytes`, §7.11),
//!   evicting the oldest index first. A miss reads the entry's frame back through the
//!   [`JournalRead`] port in bounded chunks. Eviction changes cost, never results.

use core::fmt;
use std::collections::BTreeMap;

use types::{
    ByteCount, Config, Entry, EntryIndex, EntryPayload, HardState, JournalRead, Limit, LogIndex,
    NodeId, ReadLen, StorageError, Term,
};

use crate::frame::{FrameError, FrameKind, HEADER_LEN, Header, decode, decode_header, encode};
use crate::record::{RaftBatch, RecordError};

/// Why a frame cannot be applied to the logical log, or an entry cannot be read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// The frame's sequence is not the next one.
    Sequence {
        /// The next sequence.
        expected: u64,
        /// The frame's sequence.
        found: u64,
    },
    /// The frame does not start where the journal ends.
    Offset {
        /// The journal's end.
        expected: u64,
        /// The frame's offset.
        found: u64,
    },
    /// The batch's first entry leaves a gap after the last index.
    Gap {
        /// The last index plus one.
        expected: u64,
        /// The batch's first index.
        found: u64,
    },
    /// The batch would replace entries at or below the persisted commit index.
    TruncatesCommitted {
        /// The batch's first index.
        first: u64,
        /// The persisted commit index.
        commit: u64,
    },
    /// The batch's hard state commits beyond the last index.
    CommitBeyondLog {
        /// The hard state's commit index.
        commit: u64,
        /// The last index after the batch.
        last: u64,
    },
    /// An entry with term 0: only the dummy entry at index 0 has term 0.
    ZeroTerm {
        /// The entry's index.
        index: u64,
    },
    /// An entry's term is below the term of the entry before it.
    TermDecreases {
        /// The entry's index.
        index: u64,
        /// The entry's term.
        term: Term,
        /// The term of the entry before it.
        previous: Term,
    },
    /// An entry's term is above the term in force after the batch.
    TermAhead {
        /// The entry's index.
        index: u64,
        /// The entry's term.
        term: Term,
        /// The batch's hard-state term, or the persisted term without one.
        in_force: Term,
    },
    /// The hard state's term is below the persisted term.
    TermRegression {
        /// The persisted term.
        persisted: Term,
        /// The batch's term.
        found: Term,
    },
    /// The hard state's commit index is below the persisted commit index.
    CommitRegression {
        /// The persisted commit index.
        persisted: u64,
        /// The batch's commit index.
        found: u64,
    },
    /// The hard state changes a vote already persisted in the same term.
    VoteChanged {
        /// The term.
        term: Term,
        /// The persisted vote.
        persisted: NodeId,
        /// The batch's vote.
        found: Option<NodeId>,
    },
    /// The batch's hard state votes for a node outside the genesis voters (br-biok).
    VoteForNonVoter {
        /// The vote.
        vote: NodeId,
    },
    /// The frame would take the journal past its hard stop (DM-7): the node stops
    /// appending and moves to `failed`.
    HardStop {
        /// The journal's length.
        end: u64,
        /// The frame's length.
        frame_len: u64,
        /// `max_journal_bytes`.
        limit: u64,
    },
    /// The index is not in the logical log.
    NotInLog {
        /// The index asked for.
        index: u64,
    },
    /// The frame at the offset is not a Raft batch.
    NotRaftBatch {
        /// The frame's offset.
        offset: u64,
    },
    /// The frame read back does not hold the entry the index promised.
    EntryMismatch {
        /// The index asked for.
        index: u64,
    },
    /// The frame is too large to encode.
    Frame(FrameError),
    /// The frame read back is not a valid Raft batch.
    Record(RecordError),
    /// The byte-storage port failed.
    Storage(StorageError),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence { expected, found } => {
                write!(f, "frame sequence {found} where {expected} was next")
            }
            Self::Offset { expected, found } => {
                write!(
                    f,
                    "frame at offset {found} where the journal ends at {expected}"
                )
            }
            Self::Gap { expected, found } => {
                write!(
                    f,
                    "batch starts at index {found}; the next index is {expected}"
                )
            }
            Self::TruncatesCommitted { first, commit } => write!(
                f,
                "batch would replace entries from {first}, at or below commit index {commit}"
            ),
            Self::CommitBeyondLog { commit, last } => {
                write!(f, "commit index {commit} is beyond the last index {last}")
            }
            Self::ZeroTerm { index } => write!(f, "entry {index} has term 0"),
            Self::TermDecreases {
                index,
                term,
                previous,
            } => write!(
                f,
                "entry {index} has term {}, below the previous entry's term {}",
                term.get(),
                previous.get()
            ),
            Self::TermAhead {
                index,
                term,
                in_force,
            } => write!(
                f,
                "entry {index} has term {}, above the term in force {}",
                term.get(),
                in_force.get()
            ),
            Self::TermRegression { persisted, found } => write!(
                f,
                "hard-state term {} is below the persisted term {}",
                found.get(),
                persisted.get()
            ),
            Self::CommitRegression { persisted, found } => write!(
                f,
                "hard-state commit {found} is below the persisted commit {persisted}"
            ),
            Self::VoteForNonVoter { vote } => write!(
                f,
                "a vote for node {}, outside the genesis voters",
                vote.get()
            ),
            Self::VoteChanged {
                term,
                persisted,
                found,
            } => write!(
                f,
                "in term {} the vote for {} would become {:?}",
                term.get(),
                persisted.get(),
                found.map(NodeId::get)
            ),
            Self::HardStop {
                end,
                frame_len,
                limit,
            } => write!(
                f,
                "a {frame_len}-byte frame at {end} would pass the journal hard stop of \
                 {limit} bytes (DM-7)"
            ),
            Self::NotInLog { index } => write!(f, "index {index} is not in the logical log"),
            Self::NotRaftBatch { offset } => write!(f, "the frame at {offset} is not a Raft batch"),
            Self::EntryMismatch { index } => {
                write!(f, "the frame read back does not hold entry {index}")
            }
            Self::Frame(error) => write!(f, "frame: {error}"),
            Self::Record(error) => write!(f, "record: {error}"),
            Self::Storage(error) => write!(f, "storage: {error}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// Where one entry of the logical log lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    /// The offset of the frame that holds the entry.
    frame_offset: u64,
    /// The entry's term.
    term: Term,
}

/// The bounded entry cache: recent entries, evicted oldest-index first.
///
/// Invariant: the cached entries' weights sum to `bytes`, at most `limit`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EntryCache {
    /// The cached entries, by index.
    entries: BTreeMap<u64, Entry>,
    /// The cached weight.
    bytes: u64,
    /// The byte bound.
    limit: u64,
}

impl EntryCache {
    /// An entry's weight: its fixed fields plus its command bytes.
    fn weight(entry: &Entry) -> u64 {
        let data = match &entry.payload {
            EntryPayload::Empty => 0,
            EntryPayload::Command(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        };
        data.saturating_add(32)
    }

    /// Caches `entry`, evicting the oldest entries until the bound holds. An entry
    /// heavier than the whole bound is not cached.
    fn insert(&mut self, entry: Entry) {
        let weight = Self::weight(&entry);
        if weight > self.limit {
            return;
        }
        if let Some(old) = self.entries.insert(entry.index.get(), entry) {
            self.bytes = self.bytes.saturating_sub(Self::weight(&old));
        }
        self.bytes = self.bytes.saturating_add(weight);
        while self.bytes > self.limit {
            let Some((_, evicted)) = self.entries.pop_first() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(Self::weight(&evicted));
        }
    }

    /// Drops every entry at or after `index`.
    fn remove_from(&mut self, index: u64) {
        for (_, dropped) in self.entries.split_off(&index) {
            self.bytes = self.bytes.saturating_sub(Self::weight(&dropped));
        }
    }

    /// The cached entry at `index`, if any.
    fn get(&self, index: u64) -> Option<&Entry> {
        self.entries.get(&index)
    }
}

/// The logical log's bounds (§7.11, CD-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogBounds {
    /// The entry cache's byte bound (`Limit::EntryCacheBytes`).
    cache_bytes: ByteCount,
    /// The journal's hard stop (DM-7, `max_journal_bytes`).
    max_journal_bytes: ByteCount,
}

impl LogBounds {
    /// Bounds of an entry cache of `cache_bytes` and a hard stop of `max_journal_bytes`.
    #[must_use]
    pub const fn new(cache_bytes: ByteCount, max_journal_bytes: ByteCount) -> Self {
        Self {
            cache_bytes,
            max_journal_bytes,
        }
    }

    /// The bounds `config` sets: the fixed entry-cache bound and `max_journal_bytes`.
    #[must_use]
    pub const fn of(config: &Config) -> Self {
        Self::new(
            ByteCount::new(config.limit(Limit::EntryCacheBytes)),
            config.max_journal_bytes(),
        )
    }

    /// The entry cache's byte bound.
    #[must_use]
    pub const fn cache_bytes(self) -> ByteCount {
        self.cache_bytes
    }

    /// The journal's hard stop.
    #[must_use]
    pub const fn max_journal_bytes(self) -> ByteCount {
        self.max_journal_bytes
    }
}

/// The logical log of one journal.
///
/// Invariants:
/// - `positions[i]` describes entry `i + 1`, so the log holds entries 1 to
///   `positions.len()`, with terms of at least 1 that never decrease and never exceed
///   the persisted term;
/// - the persisted commit index is at most that last index;
/// - a persisted vote names one of the genesis voters;
/// - `end` is the journal's length in bytes and `next_sequence` the next frame's
///   sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalLog {
    /// Each entry's frame offset and term, entry 1 first.
    positions: Vec<Position>,
    /// The latest persisted hard state.
    hard_state: HardState,
    /// The journal's length in bytes.
    end: u64,
    /// The next frame's sequence.
    next_sequence: u64,
    /// Recent entries.
    cache: EntryCache,
    /// The hard stop: no append takes `end` past it (DM-7).
    max_journal_bytes: u64,
    /// The voters the genesis records, which a persisted vote must name: the live path
    /// and recovery refuse the same votes (CD-10, br-biok).
    voters: Vec<NodeId>,
}

impl LogicalLog {
    /// The empty logical log that follows a genesis frame of `genesis_len` bytes at
    /// sequence 1, recording `voters`, within `bounds`.
    #[must_use]
    pub fn after_genesis(genesis_len: u64, voters: &[NodeId], bounds: LogBounds) -> Self {
        Self {
            positions: Vec::new(),
            hard_state: HardState {
                term: Term::ZERO,
                vote: None,
                commit: LogIndex::ZERO,
            },
            end: genesis_len,
            next_sequence: 2,
            cache: EntryCache {
                entries: BTreeMap::new(),
                bytes: 0,
                limit: bounds.cache_bytes.get(),
            },
            max_journal_bytes: bounds.max_journal_bytes.get(),
            voters: voters.to_vec(),
        }
    }

    /// Encodes `batch` as the next frame and applies it. Returns the frame's bytes, for
    /// an `Effect::Append`.
    ///
    /// # Errors
    ///
    /// Nothing changes on any error.
    /// - [`JournalError::HardStop`] if the frame would take the journal past
    ///   `max_journal_bytes`: the node stops appending and moves to `failed` (DM-7).
    /// - [`JournalError::Frame`] if the batch exceeds a frame.
    /// - Any [`LogicalLog::apply`] error. The live path treats it as an invariant
    ///   failure: a correct Raft library never writes an impossible history (CD-7).
    pub fn append(&mut self, batch: RaftBatch) -> Result<Vec<u8>, JournalError> {
        let frame = encode(FrameKind::RaftBatch, self.next_sequence, &batch.encode())
            .map_err(JournalError::Frame)?;
        let header = decode_header(&frame).map_err(JournalError::Frame)?;
        let frame_len = u64::try_from(frame.len()).unwrap_or(u64::MAX);
        if self.end.saturating_add(frame_len) > self.max_journal_bytes {
            return Err(JournalError::HardStop {
                end: self.end,
                frame_len,
                limit: self.max_journal_bytes,
            });
        }
        self.apply(self.end, header, batch)?;
        Ok(frame)
    }

    /// Applies the Raft-batch frame with `header` at `offset`, whose payload is `batch`:
    /// the live path and recovery replay alike (CD-10). The frame's length comes from
    /// its decoded header, never from the caller.
    ///
    /// # Errors
    ///
    /// The [`JournalError`] for the first rule broken. On error, nothing changes.
    pub fn apply(
        &mut self,
        offset: u64,
        header: Header,
        batch: RaftBatch,
    ) -> Result<(), JournalError> {
        if header.kind() != FrameKind::RaftBatch {
            return Err(JournalError::NotRaftBatch { offset });
        }
        let sequence = header.sequence();
        if sequence != self.next_sequence {
            return Err(JournalError::Sequence {
                expected: self.next_sequence,
                found: sequence,
            });
        }
        if offset != self.end {
            return Err(JournalError::Offset {
                expected: self.end,
                found: offset,
            });
        }
        let last = self.last_index().get();
        let (entries, hard_state) = batch.into_parts();
        let kept = match entries.first() {
            None => self.positions.len(),
            Some(first) => {
                let first = first.index.get();
                if first > last.saturating_add(1) {
                    return Err(JournalError::Gap {
                        expected: last.saturating_add(1),
                        found: first,
                    });
                }
                let commit = self.hard_state.commit.get();
                if first <= commit {
                    return Err(JournalError::TruncatesCommitted { first, commit });
                }
                usize::try_from(first - 1).unwrap_or(usize::MAX)
            }
        };
        if let Some(state) = hard_state {
            self.check_hard_state(state)?;
        }
        let in_force = hard_state.map_or(self.hard_state.term, |state| state.term);
        self.check_terms(kept, &entries, in_force)?;
        let new_last = u64::try_from(kept + entries.len()).unwrap_or(u64::MAX);
        if let Some(state) = hard_state
            && state.commit.get() > new_last
        {
            return Err(JournalError::CommitBeyondLog {
                commit: state.commit.get(),
                last: new_last,
            });
        }
        self.positions.truncate(kept);
        self.cache
            .remove_from(u64::try_from(kept).unwrap_or(u64::MAX) + 1);
        for entry in entries {
            self.positions.push(Position {
                frame_offset: offset,
                term: entry.term,
            });
            self.cache.insert(entry);
        }
        if let Some(state) = hard_state {
            self.hard_state = state;
        }
        let frame_len = u64::try_from(header.frame_len()).unwrap_or(u64::MAX);
        self.end = offset.saturating_add(frame_len);
        self.next_sequence = sequence.saturating_add(1);
        types::invariant!(
            self.positions
                .last()
                .is_none_or(|last| last.term <= self.hard_state.term),
            "JOURNAL-TERM-IN-FORCE",
            "the last entry's term exceeds the persisted term {} after the frame at {offset}",
            self.hard_state.term.get()
        );
        Ok(())
    }

    /// Refuses a hard state that would lower the persisted term or commit, or change the
    /// vote persisted in its term (DUR-2, §7.6).
    fn check_hard_state(&self, state: HardState) -> Result<(), JournalError> {
        if let Some(vote) = state.vote
            && !self.voters.contains(&vote)
        {
            return Err(JournalError::VoteForNonVoter { vote });
        }
        let persisted = self.hard_state;
        if state.term.get() < persisted.term.get() {
            return Err(JournalError::TermRegression {
                persisted: persisted.term,
                found: state.term,
            });
        }
        if state.commit.get() < persisted.commit.get() {
            return Err(JournalError::CommitRegression {
                persisted: persisted.commit.get(),
                found: state.commit.get(),
            });
        }
        if state.term.get() == persisted.term.get()
            && let Some(vote) = persisted.vote
        {
            let same = state.vote.is_some_and(|found| found.get() == vote.get());
            if !same {
                return Err(JournalError::VoteChanged {
                    term: persisted.term,
                    persisted: vote,
                    found: state.vote,
                });
            }
        }
        Ok(())
    }

    /// Refuses entry terms of 0, terms that decrease from the kept entry before the batch
    /// or within it, and terms above `in_force`.
    fn check_terms(
        &self,
        kept: usize,
        entries: &[Entry],
        in_force: Term,
    ) -> Result<(), JournalError> {
        let mut previous = kept
            .checked_sub(1)
            .and_then(|at| self.positions.get(at))
            .map(|position| position.term);
        for entry in entries {
            let index = entry.index.get();
            if entry.term == Term::ZERO {
                return Err(JournalError::ZeroTerm { index });
            }
            if let Some(previous) = previous
                && entry.term < previous
            {
                return Err(JournalError::TermDecreases {
                    index,
                    term: entry.term,
                    previous,
                });
            }
            if entry.term > in_force {
                return Err(JournalError::TermAhead {
                    index,
                    term: entry.term,
                    in_force,
                });
            }
            previous = Some(entry.term);
        }
        Ok(())
    }

    /// The last index of the logical log (0 when empty).
    #[must_use]
    pub fn last_index(&self) -> LogIndex {
        LogIndex::new(u64::try_from(self.positions.len()).unwrap_or(u64::MAX))
            .unwrap_or(LogIndex::ZERO)
    }

    /// The term of the entry at `index`, if it is in the log.
    #[must_use]
    pub fn term(&self, index: EntryIndex) -> Option<Term> {
        self.position(index).map(|position| position.term)
    }

    /// The latest persisted hard state.
    #[must_use]
    pub const fn hard_state(&self) -> HardState {
        self.hard_state
    }

    /// The journal's length in bytes.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// The next frame's sequence.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// The bytes the entry cache holds now.
    #[must_use]
    pub const fn cached_bytes(&self) -> u64 {
        self.cache.bytes
    }

    /// The entry at `index`: from the cache, or read back through `storage` in bounded
    /// chunks and then cached.
    ///
    /// # Errors
    ///
    /// [`JournalError::NotInLog`] past the last index; [`JournalError::Storage`],
    /// [`JournalError::Frame`] or [`JournalError::Record`] if the read fails or the bytes
    /// are damaged; [`JournalError::EntryMismatch`] if the frame does not hold the entry.
    pub fn entry(
        &mut self,
        index: EntryIndex,
        storage: &impl JournalRead,
    ) -> Result<Entry, JournalError> {
        let position = self
            .position(index)
            .ok_or_else(|| JournalError::NotInLog { index: index.get() })?;
        if let Some(entry) = self.cache.get(index.get()) {
            return Ok(entry.clone());
        }
        let frame = read_frame(storage, position.frame_offset)?;
        let batch = RaftBatch::decode(&frame).map_err(JournalError::Record)?;
        let entry = batch
            .entries()
            .iter()
            .find(|entry| entry.index == index && entry.term == position.term)
            .cloned()
            .ok_or_else(|| JournalError::EntryMismatch { index: index.get() })?;
        self.cache.insert(entry.clone());
        Ok(entry)
    }

    /// Where the entry at `index` lives, if it is in the log.
    fn position(&self, index: EntryIndex) -> Option<Position> {
        let at = usize::try_from(index.get() - 1).ok()?;
        self.positions.get(at).copied()
    }
}

/// Reads exactly `len` bytes at `offset` in chunks of at most `ReadLen::MAX` (§7.11).
/// Shared with the recovery scan.
pub fn read_exact(
    storage: &impl JournalRead,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, StorageError> {
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let remaining = u32::try_from(len - bytes.len()).unwrap_or(u32::MAX);
        let chunk = ReadLen::new(remaining.min(ReadLen::MAX)).ok_or(StorageError::Failed)?;
        let at = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let read = storage.read_at(ByteCount::new(at), chunk)?;
        if read.len() != chunk.get() as usize {
            return Err(StorageError::ShortRead {
                wanted: chunk,
                got: u32::try_from(read.len()).unwrap_or(u32::MAX),
            });
        }
        bytes.extend_from_slice(&read);
    }
    Ok(bytes)
}

/// The payload of the Raft-batch frame at `offset`, read and validated.
fn read_frame(storage: &impl JournalRead, offset: u64) -> Result<Vec<u8>, JournalError> {
    let header = read_exact(storage, offset, HEADER_LEN).map_err(JournalError::Storage)?;
    let frame_len = decode_header(&header)
        .map_err(JournalError::Frame)?
        .frame_len();
    let bytes = read_exact(storage, offset, frame_len).map_err(JournalError::Storage)?;
    let frame = decode(&bytes).map_err(JournalError::Frame)?;
    if frame.header().kind() != FrameKind::RaftBatch {
        return Err(JournalError::NotRaftBatch { offset });
    }
    Ok(frame.payload().to_vec())
}
