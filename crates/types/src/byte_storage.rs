//! The adapter side of the byte-storage port (PLAN.md §7.2, W12).
//!
//! The core emits storage effects ([`Effect::Append`], [`Effect::Truncate`],
//! [`Effect::Sync`], [`Effect::CreateJournal`], [`Effect::SyncDir`]) and reads through
//! [`JournalRead`]. A composition root executes the effects with a [`ByteStorage`]
//! adapter: real files in production, the simulated disk in simulation. Both adapters
//! implement the same trait, and one contract suite (`crates/types/tests/storage_contract`)
//! holds them to the same semantics, so what simulation proves holds for production (P3,
//! T-2).
//!
//! [`execute`] is the one translation from an effect to adapter calls and back to a
//! completion input, shared by every root, so adapters cannot disagree about which
//! incarnation a completion carries or how a failure is reported.

use crate::counts::ByteCount;
use crate::effects::{Effect, Incarnation, Input, StorageOp};
use crate::in_process::{DataDir, JournalRead, StorageError};

/// A node's byte storage: one data directory holding one append-only journal file.
///
/// Contract (the suite in `crates/types/tests/storage_contract` checks every adapter):
/// - Reads ([`JournalRead::read_at`]) and [`ByteStorage::journal_len`] see every append
///   and truncate so far, synced or not. A read past the end is
///   [`StorageError::ShortRead`], never fewer bytes without an error.
/// - Without a journal file, [`ByteStorage::journal_len`] and reads are
///   [`StorageError::Missing`], and appends fail.
/// - [`ByteStorage::create_journal`] creates an empty journal and refuses an existing one:
///   it never discards bytes. The file's directory entry is durable only after a later
///   [`ByteStorage::sync_dir`] (DUR-7).
/// - [`ByteStorage::append`] adds bytes at the end. [`ByteStorage::truncate`] shortens the
///   journal and refuses to lengthen it; later appends continue at the new end.
/// - [`ByteStorage::sync`] returns once every earlier append and truncate is durable.
///   A truncation is relied on only after its own sync (§7.2).
/// - [`DataDir::lock`] refuses while another handle on the same directory holds the lock.
///   Dropping a handle ends its process's hold, cleanly: everything synced is still
///   there for the next handle.
pub trait ByteStorage: JournalRead + DataDir {
    /// The journal's length: every append and truncate so far, synced or not.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] without a journal file; [`StorageError::Failed`] if the
    /// length cannot be read.
    fn journal_len(&self) -> Result<ByteCount, StorageError>;

    /// Creates the empty journal file (§7.7 step 0). Its directory entry stays volatile
    /// until a later [`ByteStorage::sync_dir`].
    ///
    /// # Errors
    ///
    /// [`StorageError::Failed`] if a journal already exists or the file cannot be created.
    fn create_journal(&mut self) -> Result<(), StorageError>;

    /// Syncs the data directory, making every earlier file creation durable (DUR-7).
    ///
    /// # Errors
    ///
    /// [`StorageError::Failed`] if the directory cannot be synced.
    fn sync_dir(&mut self) -> Result<(), StorageError>;

    /// Appends `bytes` at the journal's end.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] without a journal; [`StorageError::Failed`] otherwise.
    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError>;

    /// Truncates the journal to `len` bytes, at most its current length.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] without a journal; [`StorageError::Failed`] if `len` is
    /// past the end or the file cannot be truncated. On error the journal is unchanged.
    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError>;

    /// Makes every earlier append and truncate durable.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] without a journal; [`StorageError::Failed`] otherwise.
    fn sync(&mut self) -> Result<(), StorageError>;
}

/// What executing one effect produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Executed {
    /// Not a storage effect: the root routes it to another adapter.
    NotStorage,
    /// A storage effect without a completion (`Append`, `Truncate`, `CreateJournal`)
    /// succeeded.
    Done,
    /// The completion to feed back to the core: [`Input::SyncDone`] for a sync, or
    /// [`Input::StorageFailed`] for any storage effect that failed (§7.11).
    Completion(Input),
}

/// Executes `effect` with `storage` for a node in `incarnation` (§7.2).
///
/// A sync's completion echoes the incarnation the sync itself carries; a failure of an
/// effect without one carries `incarnation`. Delivering the completion (now, later, or
/// never, after a crash) is the root's business.
pub fn execute(
    storage: &mut impl ByteStorage,
    effect: &Effect,
    incarnation: Incarnation,
) -> Executed {
    let failed = |op: StorageOp, incarnation: Incarnation| {
        Executed::Completion(Input::StorageFailed { op, incarnation })
    };
    match effect {
        Effect::Append { frame } => storage.append(frame).map_or_else(
            |_| failed(StorageOp::Append, incarnation),
            |()| Executed::Done,
        ),
        Effect::Truncate { len } => storage.truncate(*len).map_or_else(
            |_| failed(StorageOp::Truncate, incarnation),
            |()| Executed::Done,
        ),
        Effect::CreateJournal => storage.create_journal().map_or_else(
            |_| failed(StorageOp::CreateJournal, incarnation),
            |()| Executed::Done,
        ),
        Effect::Sync {
            seq,
            incarnation: requested,
        } => storage.sync().map_or_else(
            |_| failed(StorageOp::Sync(*seq), *requested),
            |()| {
                Executed::Completion(Input::SyncDone {
                    seq: *seq,
                    incarnation: *requested,
                })
            },
        ),
        Effect::SyncDir {
            seq,
            incarnation: requested,
        } => storage.sync_dir().map_or_else(
            |_| failed(StorageOp::SyncDir(*seq), *requested),
            |()| {
                Executed::Completion(Input::SyncDone {
                    seq: *seq,
                    incarnation: *requested,
                })
            },
        ),
        Effect::Send { .. } | Effect::Respond { .. } | Effect::Log(_) | Effect::Observe(_) => {
            Executed::NotStorage
        }
    }
}
