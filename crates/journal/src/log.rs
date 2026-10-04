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
//! - **Offset index.** `LogIndex → (frame offset, term)`, about 16 bytes per entry,
//!   bounded by DM-7's admission ceiling.
//! - **Entry cache.** Recent entries up to a byte bound (`Limit::EntryCacheBytes`, §7.11),
//!   evicting the oldest index first. A miss reads the entry's frame back through the
//!   [`JournalRead`] port in bounded chunks. Eviction changes cost, never results.

use core::fmt;
use std::collections::BTreeMap;

use types::{
    ByteCount, Entry, EntryIndex, EntryPayload, HardState, JournalRead, LogIndex, ReadLen,
    StorageError, Term,
};

use crate::frame::{FrameError, FrameKind, HEADER_LEN, decode, decode_header, encode};
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
    /// The index is not in the logical log.
    NotInLog {
        /// The index asked for.
        index: u64,
    },
    /// The frame at the index's offset is not a Raft batch.
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

/// The logical log of one journal.
///
/// Invariants:
/// - `positions[i]` describes entry `i + 1`, so the log holds entries 1 to
///   `positions.len()`;
/// - the persisted commit index is at most that last index;
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
}

impl LogicalLog {
    /// The empty logical log that follows a genesis frame of `genesis_len` bytes at
    /// sequence 1, with an entry cache of `cache_bytes` (`Limit::EntryCacheBytes`).
    #[must_use]
    pub const fn after_genesis(genesis_len: u64, cache_bytes: u64) -> Self {
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
                limit: cache_bytes,
            },
        }
    }

    /// Encodes `batch` as the next frame and applies it. Returns the frame's bytes, for
    /// an `Effect::Append`.
    ///
    /// # Errors
    ///
    /// Any [`LogicalLog::apply`] error, or [`JournalError::Frame`] if the batch exceeds
    /// a frame. The live path treats an error as an invariant failure: the Raft library
    /// never truncates committed entries (CD-7).
    pub fn append(&mut self, batch: RaftBatch) -> Result<Vec<u8>, JournalError> {
        let frame = encode(FrameKind::RaftBatch, self.next_sequence, &batch.encode())
            .map_err(JournalError::Frame)?;
        let len = u64::try_from(frame.len()).unwrap_or(u64::MAX);
        self.apply(self.end, self.next_sequence, len, batch)?;
        Ok(frame)
    }

    /// Applies the Raft-batch frame of `frame_len` bytes at `offset` with `sequence`:
    /// the live path and recovery replay alike (CD-10).
    ///
    /// # Errors
    ///
    /// The [`JournalError`] for the first rule broken. On error, nothing changes.
    pub fn apply(
        &mut self,
        offset: u64,
        sequence: u64,
        frame_len: u64,
        batch: RaftBatch,
    ) -> Result<(), JournalError> {
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
        self.end = offset.saturating_add(frame_len);
        self.next_sequence = sequence.saturating_add(1);
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
fn read_exact(
    storage: &impl JournalRead,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, JournalError> {
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let remaining = u32::try_from(len - bytes.len()).unwrap_or(u32::MAX);
        let chunk = ReadLen::new(remaining.min(ReadLen::MAX))
            .ok_or(JournalError::Storage(StorageError::Failed))?;
        let at = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let read = storage
            .read_at(ByteCount::new(at), chunk)
            .map_err(JournalError::Storage)?;
        if read.len() != chunk.get() as usize {
            return Err(JournalError::Storage(StorageError::ShortRead {
                wanted: chunk,
                got: u32::try_from(read.len()).unwrap_or(u32::MAX),
            }));
        }
        bytes.extend_from_slice(&read);
    }
    Ok(bytes)
}

/// The payload of the Raft-batch frame at `offset`, read and validated.
fn read_frame(storage: &impl JournalRead, offset: u64) -> Result<Vec<u8>, JournalError> {
    let header = read_exact(storage, offset, HEADER_LEN)?;
    let frame_len = decode_header(&header)
        .map_err(JournalError::Frame)?
        .frame_len();
    let bytes = read_exact(storage, offset, frame_len)?;
    let frame = decode(&bytes).map_err(JournalError::Frame)?;
    if frame.header().kind() != FrameKind::RaftBatch {
        return Err(JournalError::NotRaftBatch { offset });
    }
    Ok(frame.payload().to_vec())
}
