//! The simulated disk (PLAN.md §10.2, W12.2): one node's data directory and journal as
//! byte-addressed **volatile** and **durable** images, and the crash model (DUR-9).
//!
//! A [`SimDisk`] is the node's directory; [`SimDisk::open`] gives one process (one
//! incarnation) a [`DiskHandle`], the simulation's [`ByteStorage`] adapter, which passes
//! the same contract suite as the real-file adapter (W12.1, P3).
//!
//! **What a crash keeps** ([`SimDisk::crash`]). Between two syncs a correct core issues
//! at most a truncation and then appends (D-50, §7.4), so the unsynced work is folded
//! into at most two ordered parts: a truncation to `keep` bytes, then the appended
//! region. The seed chooses where the crash cut that order:
//! - before the truncation: the durable image stays as it was;
//! - after it: the durable image cut to `keep`, then a prefix of the appended region,
//!   none, some or all of it, optionally followed by zero or garbage bytes up to the
//!   region's full extent (§10.2: never only "all unsynced bytes vanish").
//!
//! Synced bytes are never damaged (§9.2). A created file whose directory entry was never
//! synced is lost entirely, whatever its contents (DUR-7). A crash ends every handle:
//! a dead process does no more I/O, and the lock is free for the next incarnation.
//!
//! A sync is durable the moment [`ByteStorage::sync`] returns; its completion is an
//! input the scheduler delivers later, or never, or to the next incarnation as a stale
//! completion that the core ignores (B-17). Reads are instantaneous and deterministic.

use std::cell::RefCell;
use std::rc::Rc;

use types::{ByteCount, ByteStorage, DataDir, JournalRead, ReadLen, StorageError};

use crate::entropy::SplitMix64;

/// The directory and journal state behind a [`SimDisk`].
///
/// Invariants:
/// - `volatile[..keep] == durable[..keep]`, and `keep <= durable.len()`;
/// - without a journal (`volatile` is `None`) the entry is not durable and both images
///   are empty.
#[derive(Debug, Default)]
struct State {
    /// The journal as the running process sees it, if its file exists.
    volatile: Option<Vec<u8>>,
    /// The journal's durable bytes.
    durable: Vec<u8>,
    /// How much of `durable` the unsynced work kept: below `durable.len()` after an
    /// unsynced truncation.
    keep: usize,
    /// Whether the journal's directory entry is durable.
    entry_durable: bool,
    /// The handle holding the lock.
    holder: Option<u64>,
    /// Handles opened so far.
    opened: u64,
    /// Crashes so far: a handle from an earlier generation is dead.
    generation: u64,
}

/// The tail a crash leaves after the surviving prefix of the appended region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Nothing: the journal ends after the prefix.
    None,
    /// Zero bytes up to the region's full extent.
    Zeros,
    /// Garbage bytes up to the region's full extent.
    Garbage,
}

/// What a crash kept, for the trace and the tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Crash {
    /// No journal file existed.
    NoJournal,
    /// The journal's directory entry was never synced, so the file is gone (DUR-7).
    FileLost,
    /// The journal survived.
    Kept {
        /// Whether an unsynced truncation survived, if there was one.
        truncation: Option<bool>,
        /// How many bytes of the appended region survived.
        prefix: ByteCount,
        /// The region's full length.
        region: ByteCount,
        /// What follows the surviving prefix.
        fill: Fill,
        /// The journal's length afterwards.
        len: ByteCount,
    },
}

/// One node's simulated data directory. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct SimDisk(Rc<RefCell<State>>);

/// A byte count as a [`ByteCount`].
fn count(len: usize) -> ByteCount {
    ByteCount::new(u64::try_from(len).unwrap_or(u64::MAX))
}

/// A seed-chosen index below `bound` (which is at least 1).
const fn below(rng: &mut SplitMix64, bound: u64) -> u64 {
    rng.next_u64() % bound
}

/// How much of an unsynced region of `len` bytes survives a crash: none, all, or some
/// (1 to `len - 1`), a quarter, a quarter and a half of the time.
fn surviving_prefix(rng: &mut SplitMix64, len: usize) -> usize {
    match below(rng, 4) {
        0 => 0,
        1 => len,
        _ if len > 1 => {
            let some = below(rng, u64::try_from(len - 1).unwrap_or(u64::MAX));
            1 + usize::try_from(some).unwrap_or(0)
        }
        _ => len,
    }
}

impl SimDisk {
    /// A new, empty data directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A new process's handle on this directory.
    #[must_use]
    pub fn open(&self) -> DiskHandle {
        let mut state = self.0.borrow_mut();
        state.opened += 1;
        DiskHandle {
            state: Rc::clone(&self.0),
            id: state.opened,
            generation: state.generation,
        }
    }

    /// The journal as it is now, synced or not: for inspection, never for the core.
    #[must_use]
    pub fn journal(&self) -> Option<Vec<u8>> {
        self.0.borrow().volatile.clone()
    }

    /// The journal's durable bytes, if its directory entry is durable.
    #[must_use]
    pub fn durable(&self) -> Option<Vec<u8>> {
        let state = self.0.borrow();
        state.entry_durable.then(|| state.durable.clone())
    }

    /// The node crashes: every handle dies, the lock is freed, and the journal becomes
    /// what the crash model keeps, chosen by `rng` (the run's disk stream).
    pub fn crash(&self, rng: &mut SplitMix64) -> Crash {
        let mut state = self.0.borrow_mut();
        state.generation += 1;
        state.holder = None;
        let Some(volatile) = state.volatile.take() else {
            return Crash::NoJournal;
        };
        if !state.entry_durable {
            state.durable.clear();
            state.keep = 0;
            return Crash::FileLost;
        }
        let keep = state.keep;
        let region = volatile.get(keep..).unwrap_or_default().to_vec();
        let truncated = keep < state.durable.len();
        let truncation_survived = !truncated || below(rng, 2) == 0;
        let (mut kept, prefix, fill) = if truncation_survived {
            let prefix = surviving_prefix(rng, region.len());
            let fill = if prefix < region.len() {
                match below(rng, 3) {
                    0 => Fill::None,
                    1 => Fill::Zeros,
                    _ => Fill::Garbage,
                }
            } else {
                Fill::None
            };
            let mut kept = state.durable.get(..keep).unwrap_or_default().to_vec();
            kept.extend_from_slice(region.get(..prefix).unwrap_or_default());
            (kept, prefix, fill)
        } else {
            (state.durable.clone(), 0, Fill::None)
        };
        let tail = region.len() - prefix;
        match fill {
            Fill::None => {}
            Fill::Zeros => kept.resize(kept.len() + tail, 0),
            Fill::Garbage => kept.extend((0..tail).map(|_| rng.next_u64().to_le_bytes()[0])),
        }
        let len = count(kept.len());
        state.durable.clone_from(&kept);
        state.keep = kept.len();
        state.volatile = Some(kept);
        Crash::Kept {
            truncation: truncated.then_some(truncation_survived),
            prefix: count(prefix),
            region: count(region.len()),
            fill,
            len,
        }
    }
}

/// One process's handle on a [`SimDisk`]: the simulation's byte-storage adapter.
/// Dropping it is a clean exit (its lock is released, nothing is lost); a crash of the
/// disk kills it, and every later call fails.
#[derive(Debug)]
pub struct DiskHandle {
    /// The directory.
    state: Rc<RefCell<State>>,
    /// This handle's identity, for the lock.
    id: u64,
    /// The crash generation it was opened in.
    generation: u64,
}

impl DiskHandle {
    /// Whether this handle's process is still alive.
    const fn alive(&self, state: &State) -> Result<(), StorageError> {
        if state.generation == self.generation {
            Ok(())
        } else {
            Err(StorageError::Failed)
        }
    }
}

impl JournalRead for DiskHandle {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let state = self.state.borrow();
        self.alive(&state)?;
        let journal = state.volatile.as_ref().ok_or(StorageError::Missing)?;
        let start = usize::try_from(offset.get())
            .unwrap_or(usize::MAX)
            .min(journal.len());
        let end = start.saturating_add(len.get() as usize).min(journal.len());
        let read = journal.get(start..end).unwrap_or_default().to_vec();
        if read.len() < len.get() as usize {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(read.len()).unwrap_or(u32::MAX),
            });
        }
        Ok(read)
    }
}

impl DataDir for DiskHandle {
    fn lock(&mut self) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        if let Some(holder) = state.holder
            && holder != self.id
        {
            return Err(StorageError::Locked);
        }
        state.holder = Some(self.id);
        Ok(())
    }
}

impl ByteStorage for DiskHandle {
    fn journal_len(&self) -> Result<ByteCount, StorageError> {
        let state = self.state.borrow();
        self.alive(&state)?;
        state
            .volatile
            .as_ref()
            .map(|journal| count(journal.len()))
            .ok_or(StorageError::Missing)
    }

    fn create_journal(&mut self) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        if state.volatile.is_some() {
            return Err(StorageError::Failed);
        }
        state.volatile = Some(Vec::new());
        state.durable.clear();
        state.keep = 0;
        state.entry_durable = false;
        Ok(())
    }

    fn sync_dir(&mut self) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        state.entry_durable = state.volatile.is_some();
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        state
            .volatile
            .as_mut()
            .ok_or(StorageError::Missing)?
            .extend_from_slice(bytes);
        Ok(())
    }

    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        let journal = state.volatile.as_mut().ok_or(StorageError::Missing)?;
        let len = usize::try_from(len.get()).unwrap_or(usize::MAX);
        if len > journal.len() {
            return Err(StorageError::Failed);
        }
        journal.truncate(len);
        state.keep = state.keep.min(len);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        let mut state = self.state.borrow_mut();
        self.alive(&state)?;
        let journal = state.volatile.clone().ok_or(StorageError::Missing)?;
        state.keep = journal.len();
        state.durable = journal;
        Ok(())
    }
}

impl Drop for DiskHandle {
    /// A clean exit releases the handle's lock. A crashed handle holds none: the crash
    /// freed it, a dead handle cannot take it again, and handle identities are never
    /// reused.
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.holder == Some(self.id) {
            state.holder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    // check: SIM-DISK

    use types::{Effect, Executed, Incarnation, Input, SyncSeq, execute};

    use super::*;

    /// `len` bytes of a pattern seeded by `seed`.
    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|at| seed.wrapping_add(u8::try_from(at % 251).unwrap()))
            .collect()
    }

    /// A disk whose journal holds `synced` durably, its entry synced too, and a handle.
    fn synced(synced: &[u8]) -> (SimDisk, DiskHandle) {
        let disk = SimDisk::new();
        let mut handle = disk.open();
        handle.create_journal().unwrap();
        handle.sync_dir().unwrap();
        handle.append(synced).unwrap();
        handle.sync().unwrap();
        (disk, handle)
    }

    #[test]
    fn an_unsynced_append_survives_as_a_prefix_with_an_optional_fill() {
        let head = pattern(1, 100);
        let tail = pattern(7, 64);
        let mut prefixes = [false; 3];
        let mut fills = [false; 3];
        for seed in 0..400 {
            let (disk, mut handle) = synced(&head);
            handle.append(&tail).unwrap();
            let crash = disk.crash(&mut SplitMix64::new(seed));
            let journal = disk.journal().unwrap();
            let Crash::Kept {
                truncation: None,
                prefix,
                region,
                fill,
                len,
            } = crash
            else {
                panic!(
                    "rule: an append after a synced entry keeps the journal; seed={seed} {crash:?}"
                );
            };
            let prefix = usize::try_from(prefix.get()).unwrap();
            assert_eq!(region, count(64), "seed={seed}");
            assert_eq!(
                &journal[..100],
                head.as_slice(),
                "rule: synced bytes are never damaged (§9.2); seed={seed}"
            );
            assert_eq!(
                &journal[100..100 + prefix],
                &tail[..prefix],
                "rule: the surviving unsynced bytes are a prefix of the append (DUR-9); seed={seed}"
            );
            let rest = &journal[100 + prefix..];
            match fill {
                Fill::None => assert!(rest.is_empty(), "seed={seed}"),
                Fill::Zeros => assert_eq!(rest, vec![0; 64 - prefix].as_slice(), "seed={seed}"),
                Fill::Garbage => assert_eq!(rest.len(), 64 - prefix, "seed={seed}"),
            }
            assert_eq!(len, count(journal.len()), "seed={seed}");
            prefixes[match prefix {
                0 => 0,
                64 => 2,
                _ => 1,
            }] = true;
            fills[match fill {
                Fill::None => 0,
                Fill::Zeros => 1,
                Fill::Garbage => 2,
            }] = true;
        }
        assert_eq!(
            prefixes, [true; 3],
            "rule: seeds choose none, some and all of the unsynced bytes, never only none (§10.2)"
        );
        assert_eq!(
            fills, [true; 3],
            "rule: seeds choose no fill, zeros or garbage up to the append's extent (§10.2)"
        );
    }

    #[test]
    fn a_file_whose_entry_was_never_synced_is_lost() {
        let disk = SimDisk::new();
        let mut handle = disk.open();
        handle.create_journal().unwrap();
        handle.append(b"genesis").unwrap();
        handle.sync().unwrap();
        assert_eq!(
            disk.crash(&mut SplitMix64::new(1)),
            Crash::FileLost,
            "rule: a created file is volatile until its directory is synced (DUR-7)"
        );
        assert_eq!(
            disk.open().journal_len(),
            Err(StorageError::Missing),
            "rule: after the crash the journal does not exist"
        );
        let mut handle = disk.open();
        handle.create_journal().unwrap();
        handle.sync_dir().unwrap();
        disk.crash(&mut SplitMix64::new(1));
        assert_eq!(
            disk.open().journal_len(),
            Ok(count(0)),
            "rule: once the directory is synced the file survives a crash"
        );
    }

    #[test]
    fn an_unsynced_truncation_may_be_lost_and_a_synced_one_never_is() {
        let head = pattern(3, 100);
        let mut outcomes = [false; 2];
        for seed in 0..64 {
            let (disk, mut handle) = synced(&head);
            handle.truncate(count(40)).unwrap();
            let crash = disk.crash(&mut SplitMix64::new(seed));
            let journal = disk.journal().unwrap();
            match crash {
                Crash::Kept {
                    truncation: Some(true),
                    ..
                } => {
                    assert_eq!(journal, head[..40].to_vec(), "seed={seed}");
                    outcomes[0] = true;
                }
                Crash::Kept {
                    truncation: Some(false),
                    ..
                } => {
                    assert_eq!(
                        journal, head,
                        "rule: a lost truncation keeps every synced byte; seed={seed}"
                    );
                    outcomes[1] = true;
                }
                Crash::Kept {
                    truncation: None, ..
                }
                | Crash::NoJournal
                | Crash::FileLost => {
                    panic!("rule: an unsynced truncation is a crash choice; seed={seed} {crash:?}")
                }
            }
        }
        assert_eq!(
            outcomes, [true; 2],
            "rule: a crash between a truncation and its sync may keep either length (§10.2)"
        );
        let (disk, mut handle) = synced(&head);
        handle.truncate(count(40)).unwrap();
        handle.sync().unwrap();
        disk.crash(&mut SplitMix64::new(0));
        assert_eq!(
            disk.journal(),
            Some(head[..40].to_vec()),
            "rule: a synced truncation always survives"
        );
    }

    #[test]
    fn a_crash_ends_every_handle_and_frees_the_lock() {
        let (disk, mut old) = synced(b"frame");
        old.lock().unwrap();
        disk.crash(&mut SplitMix64::new(5));
        assert_eq!(
            (old.append(b"zombie"), old.journal_len(), old.lock()),
            (
                Err(StorageError::Failed),
                Err(StorageError::Failed),
                Err(StorageError::Failed)
            ),
            "rule: a crashed process does no more I/O"
        );
        let mut next = disk.open();
        assert_eq!(next.lock(), Ok(()), "rule: the crash freed the lock");
        drop(old);
        assert_eq!(
            disk.open().lock(),
            Err(StorageError::Locked),
            "rule: a dead handle's drop never releases the next incarnation's lock"
        );
        assert_eq!(disk.journal(), Some(b"frame".to_vec()));
    }

    #[test]
    fn a_lost_sync_completion_is_stale_for_the_next_incarnation() {
        let (disk, mut handle) = synced(b"genesis");
        let first = Incarnation::new(1);
        handle.append(b"frame").unwrap();
        let completion = execute(
            &mut handle,
            &Effect::Sync {
                seq: SyncSeq::new(9),
                incarnation: first,
            },
            first,
        );
        let Executed::Completion(done) = completion else {
            panic!("rule: a sync completes; got {completion:?}");
        };
        disk.crash(&mut SplitMix64::new(2));
        assert_eq!(
            disk.journal(),
            Some(b"genesisframe".to_vec()),
            "rule: the sync was durable although its completion was lost (§10.2)"
        );
        assert!(
            done.is_stale_completion(Incarnation::new(2)),
            "rule: delivered to the next incarnation, the completion is stale (B-17): {done:?}"
        );
        assert_eq!(
            done,
            Input::SyncDone {
                seq: SyncSeq::new(9),
                incarnation: first,
            },
            "rule: the completion names its sync and the incarnation that asked"
        );
    }

    #[test]
    fn the_same_seed_keeps_the_same_bytes() {
        let run = |seed| {
            let (disk, mut handle) = synced(&pattern(9, 50));
            handle.truncate(count(30)).unwrap();
            handle.append(&pattern(4, 200)).unwrap();
            let crash = disk.crash(&mut SplitMix64::new(seed));
            (crash, disk.journal())
        };
        for seed in 0..32 {
            assert_eq!(
                run(seed),
                run(seed),
                "rule: a crash is a function of the seed (T-3); seed={seed}"
            );
        }
        let distinct: std::collections::BTreeSet<_> = (0..32).map(|seed| run(seed).1).collect();
        assert!(
            distinct.len() > 4,
            "rule: different seeds explore different crash outcomes; {} distinct",
            distinct.len()
        );
    }
}
