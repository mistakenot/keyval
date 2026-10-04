//! The doctor's examination of a journal (PLAN.md O-7, D-26, W23.1).
//!
//! [`examine`] reads a journal exactly as startup recovery does, through the same scan
//! ([`recovery::scan`](crate::recovery::scan), CD-10). It reports what recovery would do,
//! and what it read on the way, without modifying anything: it reads only through
//! [`JournalRead`].
//! - The scan covers the genesis record, every frame's checksums and sequence, index
//!   contiguity, term monotonicity and hard-state consistency (commit at most the last
//!   index), all refused by the logical log's own rules (§7.6).
//! - The verdict says what recovery would do: start, discard a torn tail first (where,
//!   how long, and what it looks like, e.g. "discard the 37-byte incomplete final frame
//!   at offset 4096"), or refuse, with why.
//! - Given the configuration and node, the genesis binding is verified too (DUR-8).
//!   Without it, the doctor checks everything else and says the binding was unchecked.
//!
//! The caller probes the data directory's lock first and refuses a live directory, so an
//! in-flight append is never read as damage (O-7).

use types::{ByteCount, Config, HardState, JournalRead, LogIndex, NodeId, Term};

use crate::log::{LogBounds, LogicalLog};
use crate::record::Genesis;
use crate::recovery::{RecoveryError, TornTail, scan};

/// Whether the genesis was verified against a configuration (DUR-8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A genesis record was read and checked against the given configuration and node. A
    /// mismatch is refused ([`RecoveryError::Mismatch`]), so any other verdict matched.
    Verified,
    /// Not checked: no configuration was given, or no genesis record was read (br-ljgh).
    Unchecked,
}

/// What startup recovery would do with the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Start: the journal ends with a whole frame.
    Clean,
    /// Start after discarding the torn tail: truncate, then sync (D-50).
    DiscardTail(TornTail),
    /// Refuse to start.
    Refuse(RecoveryError),
}

/// The doctor's report on one journal.
///
/// Invariant: a `Clean` or `DiscardTail` verdict comes with the genesis and the logical
/// log of the whole durable prefix; a refusal carries whatever was read before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Examination {
    /// The journal's length when it was read.
    journal_len: u64,
    /// The genesis, if one was read.
    genesis: Option<Genesis>,
    /// The logical log of the frames applied, if the genesis was read.
    log: Option<LogicalLog>,
    /// Whether the genesis binding was verified.
    binding: Binding,
    /// What recovery would do.
    verdict: Verdict,
}

impl Examination {
    /// The journal's length when it was read.
    #[must_use]
    pub const fn journal_len(&self) -> ByteCount {
        ByteCount::new(self.journal_len)
    }

    /// The genesis, if the first frame is one.
    #[must_use]
    pub const fn genesis(&self) -> Option<&Genesis> {
        self.genesis.as_ref()
    }

    /// Whether the genesis binding was verified.
    #[must_use]
    pub const fn binding(&self) -> Binding {
        self.binding
    }

    /// What startup recovery would do.
    #[must_use]
    pub const fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// The frames read and applied, the genesis included: the durable prefix, or the
    /// frames before a refusal.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        match (&self.log, &self.genesis) {
            (Some(log), _) => log.next_sequence() - 1,
            (None, Some(_)) => 1,
            (None, None) => 0,
        }
    }

    /// Where the frames read end: the durable prefix, or where the refusal was found.
    /// `None` when no logical log was built (no genesis read, or the genesis refused), so
    /// nothing reads as an empty log (br-ljgh).
    #[must_use]
    pub fn durable_len(&self) -> Option<ByteCount> {
        self.log.as_ref().map(|log| ByteCount::new(log.end()))
    }

    /// The last index of the logical log (0 when it holds no entry). `None` when no
    /// logical log was built (br-ljgh).
    #[must_use]
    pub fn last_index(&self) -> Option<LogIndex> {
        self.log.as_ref().map(LogicalLog::last_index)
    }

    /// The term of the last entry, if the log holds one.
    #[must_use]
    pub fn last_term(&self) -> Option<Term> {
        let log = self.log.as_ref()?;
        log.term(types::EntryIndex::new(log.last_index().get()).ok()?)
    }

    /// The persisted hard state, if the genesis was read.
    #[must_use]
    pub fn hard_state(&self) -> Option<HardState> {
        self.log.as_ref().map(LogicalLog::hard_state)
    }
}

/// Examines the journal of `journal_len` bytes as startup recovery would, modifying
/// nothing (O-7).
///
/// The frames are applied to a logical log within `bounds`. With `binding`, the genesis is
/// verified against that configuration and node, so the verdict is exactly [`recover`]'s.
///
/// [`recover`]: crate::recover
#[must_use]
pub fn examine(
    storage: &impl JournalRead,
    journal_len: ByteCount,
    binding: Option<(&Config, NodeId)>,
    bounds: LogBounds,
) -> Examination {
    let journal_len = journal_len.get();
    // Recovery checks the binding as soon as it has decoded the genesis record.
    let checked = |genesis: &Option<Genesis>| {
        if binding.is_some() && genesis.is_some() {
            Binding::Verified
        } else {
            Binding::Unchecked
        }
    };
    match scan(storage, journal_len, binding, bounds) {
        Ok(scanned) => Examination {
            journal_len,
            binding: checked(&Some(scanned.genesis.clone())),
            genesis: Some(scanned.genesis),
            log: Some(scanned.log),
            verdict: scanned.tail.map_or(Verdict::Clean, Verdict::DiscardTail),
        },
        Err(refusal) => {
            let refusal = *refusal;
            Examination {
                journal_len,
                binding: checked(&refusal.genesis),
                genesis: refusal.genesis,
                log: refusal.log,
                verdict: Verdict::Refuse(refusal.error),
            }
        }
    }
}
