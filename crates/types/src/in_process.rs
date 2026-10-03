//! In-process ports, called synchronously by the engine because the call is
//! deterministic given the adapter (PLAN.md §7.2): [`Entropy`], bounded journal reads
//! ([`JournalRead`]) and the data directory's lock and creation steps ([`DataDir`]).
//!
//! The types are core-owned. Adapters map their own errors (files, simulated disks)
//! onto [`StorageError`]; no filesystem type reaches the core (CD-1).

use core::fmt;

use crate::counts::ByteCount;

/// Which consumer draws from the entropy port. Each stream is independent, so one
/// consumer's draws never shift another's (§10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntropyStream {
    /// Raft's randomised election timeouts, at construction and on every reset (§7.3).
    ElectionTimeout,
}

/// Randomness for the core: seeded once at boot in production, derived from the
/// scenario seed, node and incarnation in simulation (§7.2).
pub trait Entropy {
    /// The next 64 random bits of `stream`.
    fn next_u64(&mut self, stream: EntropyStream) -> u64;
}

/// The length of one bounded journal read (§7.11 "read chunk").
///
/// Invariant: 1 to [`ReadLen::MAX`] bytes (256 KiB).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadLen(u32);

impl ReadLen {
    /// The longest read: 256 KiB (§7.11).
    pub const MAX: u32 = 256 * 1024;

    /// A read length, or `None` unless `len` is 1 to [`ReadLen::MAX`].
    #[must_use]
    pub const fn new(len: u32) -> Option<Self> {
        if len == 0 || len > Self::MAX {
            None
        } else {
            Some(Self(len))
        }
    }

    /// The length in bytes.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Why a storage call failed. Adapters log the underlying cause themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// Another process holds the data directory's exclusive lock (DUR-8).
    Locked,
    /// The data directory or journal does not exist (§7.7: a blank start is refused).
    Missing,
    /// A read returned fewer bytes than asked for.
    ShortRead {
        /// The bytes asked for.
        wanted: ReadLen,
        /// The bytes returned.
        got: u32,
    },
    /// Any other I/O failure. The node moves to `failed` (§7.11).
    Failed,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked => f.write_str("the data directory is locked by another process"),
            Self::Missing => f.write_str("the data directory or journal is missing"),
            Self::ShortRead { wanted, got } => write!(f, "read {got} of {} bytes", wanted.get()),
            Self::Failed => f.write_str("storage I/O failed"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Bounded, synchronous reads of the journal (§7.2): the Raft library's storage
/// callbacks are synchronous, so cache misses read through this port.
pub trait JournalRead {
    /// Reads exactly `len` bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// [`StorageError`] if the bytes cannot be read; a short read is
    /// [`StorageError::ShortRead`].
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError>;
}

/// The data directory's exclusive lock and creation steps (§7.7 step 0, DUR-7, DUR-8).
pub trait DataDir {
    /// Takes the exclusive lock, refusing if another process holds it.
    ///
    /// # Errors
    ///
    /// [`StorageError::Locked`] if the lock is held; [`StorageError::Failed`] otherwise.
    fn lock(&mut self) -> Result<(), StorageError>;

    /// Creates the empty journal file.
    ///
    /// # Errors
    ///
    /// [`StorageError::Failed`] if it cannot be created.
    fn create_journal(&mut self) -> Result<(), StorageError>;

    /// Syncs the directory, so a created file survives a crash (DUR-7).
    ///
    /// # Errors
    ///
    /// [`StorageError::Failed`] if the sync fails.
    fn sync_dir(&mut self) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_lengths_are_1_byte_to_256_kib() {
        for (len, ok) in [
            (0, false),
            (1, true),
            (ReadLen::MAX, true),
            (ReadLen::MAX + 1, false),
        ] {
            assert_eq!(
                ReadLen::new(len).is_some(),
                ok,
                "rule: one read chunk is 1..=256 KiB (§7.11); len={len}"
            );
        }
    }

    /// An in-memory journal: the read port needs no filesystem types (CD-1).
    struct Bytes(Vec<u8>);

    impl JournalRead for Bytes {
        fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
            let start = usize::try_from(offset.get()).map_err(|_| StorageError::Failed)?;
            let wanted = usize::try_from(len.get()).map_err(|_| StorageError::Failed)?;
            let available = self.0.get(start..).unwrap_or_default();
            if available.len() < wanted {
                let got = u32::try_from(available.len()).map_err(|_| StorageError::Failed)?;
                return Err(StorageError::ShortRead { wanted: len, got });
            }
            Ok(available.iter().take(wanted).copied().collect())
        }
    }

    /// A fixed entropy source: each stream is independent.
    struct Counter(u64);

    impl Entropy for Counter {
        fn next_u64(&mut self, stream: EntropyStream) -> u64 {
            match stream {
                EntropyStream::ElectionTimeout => {
                    self.0 += 1;
                    self.0
                }
            }
        }
    }

    #[test]
    fn in_process_ports_are_plain_traits() {
        let journal = Bytes(b"KVJ1frame".to_vec());
        assert_eq!(
            journal.read_at(ByteCount::new(4), ReadLen::new(5).unwrap()),
            Ok(b"frame".to_vec()),
            "rule: read_at returns exactly the bytes asked for (§7.2)"
        );
        assert_eq!(
            journal.read_at(ByteCount::new(4), ReadLen::new(6).unwrap()),
            Err(StorageError::ShortRead {
                wanted: ReadLen::new(6).unwrap(),
                got: 5
            }),
            "rule: a short read is a typed error, never silent"
        );
        let mut entropy = Counter(0);
        assert_eq!(
            [
                entropy.next_u64(EntropyStream::ElectionTimeout),
                entropy.next_u64(EntropyStream::ElectionTimeout)
            ],
            [1, 2],
            "rule: next_u64(stream) draws from the named stream (§7.2)"
        );
    }
}
