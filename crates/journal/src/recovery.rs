//! Fail-closed recovery (PLAN.md §7.6 "Fail closed", §7.7 steps 0–4; D-50, D-52, D-60,
//! D-69).
//!
//! [`recover`] reads a journal through the [`JournalRead`] port in bounded chunks, and
//! only reads: every refusal names its cause and modifies nothing.
//! 1. The genesis frame is read and verified against the configuration: node, voters and
//!    semantic fingerprint (§7.7 step 2). An ordinary start refuses an empty journal, or
//!    one holding no complete genesis and nothing after it (D-52, anti-amnesia). Those
//!    two refusals, and only they, let `init` (re-)run ([`RecoveryError::permits_init`]).
//! 2. Frames are read in order and applied to a [`LogicalLog`], exactly as live appends
//!    are (CD-10). The first frame that fails validation (bad magic, a checksum, an
//!    impossible length, the wrong kind or sequence) ends the durable prefix at the last
//!    valid frame N. A frame whose header is intact (its header checksum holds) but holds
//!    a field this build never writes (a version, kind, reserved byte, length or sequence
//!    0) is not crash damage: it refuses, whether it is the genesis or a later frame
//!    (DUR-9, br-o69w). A checksum-valid frame of the right sequence is refused if its
//!    batch is invalid, or cannot apply: an impossible index or term relationship
//!    (DUR-6), or a vote for a node outside the genesis voters, which the log refuses on
//!    live appends too (CD-10).
//! 3. The **resync scan** then searches the entire remainder, at every offset, for a
//!    complete, checksum-valid frame with a sequence greater than N (D-60). Its memory
//!    is bounded by one frame; how far it looks is not.
//!    - If one exists, the damage is inside the durable prefix: corruption, refuse.
//!    - If none does, the remainder is a **torn tail**. [`Recovered::truncate_to`] says
//!      where it starts; the core truncates there and syncs before its next append
//!      (D-50).
//!    - When the first invalid frame's header is intact (its header checksum holds), its
//!      length is trustworthy and the scan skips that frame, so client bytes inside a
//!      torn payload cannot pass for a later frame (D-69).
//!    - A legitimate torn tail is at most one maximum frame, because at most one Ready's
//!      frame is ever unsynced (§7.4). A longer trailing region is outside the crash
//!      model: refuse.

use core::fmt;

use types::{
    ByteCount, Config, InstanceId, JournalRead, MAX_FRAME_PAYLOAD_BYTES, NodeId, ReadLen,
    StorageError,
};

use crate::frame::{
    FrameError, FrameKind, HEADER_LEN, Header, MAGIC, TRAILER_LEN, decode, decode_header,
};
use crate::log::{JournalError, LogBounds, LogicalLog, read_exact};
use crate::record::{Genesis, GenesisMismatch, RaftBatch, RecordError};

/// The longest frame in bytes: a full payload plus the header and trailer. A torn tail
/// is never longer.
pub const MAX_FRAME_LEN: u64 = MAX_FRAME_PAYLOAD_BYTES + HEADER_LEN as u64 + TRAILER_LEN as u64;

/// A recovered journal: its verified genesis, its logical log, and where its torn tail
/// starts, if it has one.
///
/// Invariant: the logical log ends at or before the journal's length when it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The verified genesis.
    genesis: Genesis,
    /// The logical log rebuilt from the durable prefix.
    log: LogicalLog,
    /// The journal's length when it was read.
    journal_len: u64,
}

impl Recovered {
    /// The verified genesis. It carries the cluster instance ID for peer envelopes
    /// (§7.7 step 2).
    #[must_use]
    pub const fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// The logical log rebuilt from the durable prefix (§7.7 step 4).
    #[must_use]
    pub const fn log(&self) -> &LogicalLog {
        &self.log
    }

    /// The length to truncate a torn tail to, if there is one. The core emits
    /// `Effect::Truncate` with it and a `Sync` before its next append (DUR-6, D-50). A
    /// crash before that sync recovers to the same answer.
    #[must_use]
    pub const fn truncate_to(&self) -> Option<ByteCount> {
        if self.log.end() < self.journal_len {
            Some(ByteCount::new(self.log.end()))
        } else {
            None
        }
    }

    /// The genesis and the logical log, by value.
    #[must_use]
    pub fn into_parts(self) -> (Genesis, LogicalLog) {
        (self.genesis, self.log)
    }
}

/// What a torn tail looks like where it starts (§7.6, D-50, D-69).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailShape {
    /// A frame cut short: its header is intact and promises `frame_len` bytes, fewer of
    /// which reached the disk.
    Incomplete {
        /// The frame's full length, from its intact header.
        frame_len: u64,
    },
    /// A full-length frame whose checksum fails.
    ChecksumFailed,
    /// Bytes without an intact header: a cut header, zeros or garbage.
    Unreadable,
    /// A complete, valid frame whose sequence is at or below the last one applied: a
    /// stale copy.
    Stale {
        /// Its sequence.
        sequence: u64,
    },
}

impl fmt::Display for TailShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { frame_len } => {
                write!(f, "incomplete final frame (of {frame_len} bytes)")
            }
            Self::ChecksumFailed => f.write_str("final frame failing its checksum"),
            Self::Unreadable => f.write_str("unreadable tail (no intact frame header)"),
            Self::Stale { sequence } => write!(f, "stale copy of frame {sequence}"),
        }
    }
}

/// The torn tail startup recovery discards: it truncates the journal to `start` and
/// syncs before the next append (DUR-6, D-50).
///
/// Invariant: `len` is at most one maximum frame ([`MAX_FRAME_LEN`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TornTail {
    /// Where it starts: the end of the durable prefix.
    start: u64,
    /// Its length in bytes.
    len: u64,
    /// What it looks like.
    shape: TailShape,
}

impl TornTail {
    /// Where the tail starts: the length recovery truncates the journal to.
    #[must_use]
    pub const fn start(&self) -> ByteCount {
        ByteCount::new(self.start)
    }

    /// The tail's length in bytes.
    #[must_use]
    pub const fn len(&self) -> ByteCount {
        ByteCount::new(self.len)
    }

    /// What the tail looks like.
    #[must_use]
    pub const fn shape(&self) -> TailShape {
        self.shape
    }
}

impl fmt::Display for TornTail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "discard the {}-byte {} at offset {}",
            self.len, self.shape, self.start
        )
    }
}

/// Why a journal cannot be recovered: the node does not start, and nothing has been
/// modified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryError {
    /// The journal is empty. An ordinary start refuses a blank node (D-52); `init` may
    /// run.
    Empty,
    /// The journal holds no complete genesis frame and nothing after it: a crash
    /// interrupted `init`, which may re-run (§7.7 step 0). An ordinary start refuses.
    IncompleteGenesis,
    /// The journal holds no valid genesis, yet more bytes than the genesis frame an
    /// interrupted `init` can leave: content, never re-initialised (§7.7 step 0).
    NotAnInterruptedInit {
        /// The journal's length.
        len: u64,
        /// The length of a genesis frame for this configuration and node. Without a
        /// configuration (the doctor), the longest any configuration gives (br-riea).
        genesis_len: u64,
    },
    /// A frame whose header is intact (its header checksum holds) but holds a field this
    /// build never writes. A torn append is a prefix of a frame this build wrote, so this
    /// is damage outside the crash model, never a torn tail (DUR-9, br-o69w).
    ImpossibleHeader {
        /// The frame's offset.
        at: u64,
        /// The impossible field.
        error: FrameError,
    },
    /// The first frame is valid but is not the genesis at sequence 1.
    NotGenesis {
        /// Its kind.
        kind: FrameKind,
        /// Its sequence.
        sequence: u64,
    },
    /// The genesis frame's payload is invalid.
    GenesisRecord(RecordError),
    /// The genesis conflicts with the configuration (DUR-8).
    Mismatch(GenesisMismatch),
    /// A checksum-valid Raft-batch frame, next in sequence, whose batch is invalid.
    Record {
        /// The frame's offset.
        at: u64,
        /// What is wrong with the batch.
        error: RecordError,
    },
    /// A batch that cannot apply: an impossible index or term relationship, such as
    /// replacing entries at or below the persisted commit, or a term or commit regression
    /// (DUR-6, DUR-2).
    Apply {
        /// The frame's offset.
        at: u64,
        /// The rule it breaks.
        error: JournalError,
    },
    /// Damage inside the durable prefix: beyond the first invalid frame lies a valid
    /// frame with a later sequence (D-60).
    Corruption {
        /// Where the first invalid frame starts.
        damaged_at: u64,
        /// Where the later valid frame starts.
        valid_at: u64,
        /// The later frame's sequence.
        sequence: u64,
    },
    /// The region after the durable prefix is longer than one maximum frame: outside the
    /// crash model (§7.6).
    TailTooLong {
        /// Where the region starts.
        start: u64,
        /// Its length in bytes.
        len: u64,
    },
    /// The byte-storage port failed.
    Storage(StorageError),
}

impl RecoveryError {
    /// Whether `init` may (re-)run on this journal: only when it is empty or holds no
    /// complete genesis and nothing after it (§7.7 step 0). Any other content is never
    /// re-initialised.
    #[must_use]
    pub const fn permits_init(&self) -> bool {
        match self {
            Self::Empty | Self::IncompleteGenesis => true,
            Self::NotAnInterruptedInit { .. }
            | Self::ImpossibleHeader { .. }
            | Self::NotGenesis { .. }
            | Self::GenesisRecord(_)
            | Self::Mismatch(_)
            | Self::Record { .. }
            | Self::Apply { .. }
            | Self::Corruption { .. }
            | Self::TailTooLong { .. }
            | Self::Storage(_) => false,
        }
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the journal is empty: a blank node never starts (D-52)"),
            Self::IncompleteGenesis => f.write_str(
                "the journal holds no complete genesis and nothing after it: only init may \
                 re-run",
            ),
            Self::NotAnInterruptedInit { len, genesis_len } => write!(
                f,
                "the journal holds no valid genesis but {len} bytes, more than the \
                 {genesis_len}-byte genesis frame an interrupted init can leave: it is never \
                 re-initialised"
            ),
            Self::ImpossibleHeader { at, error } => write!(
                f,
                "frame at offset {at} has an intact header (its checksum holds) with {error}: \
                 this build never writes such a header, so it is damage, not a torn tail"
            ),
            Self::NotGenesis { kind, sequence } => write!(
                f,
                "the first frame is {kind:?} at sequence {sequence}, not the genesis at 1"
            ),
            Self::GenesisRecord(error) => write!(f, "genesis frame: {error}"),
            Self::Mismatch(error) => write!(f, "{error}"),
            Self::Record { at, error } => write!(f, "frame at offset {at}: {error}"),
            Self::Apply { at, error } => write!(f, "frame at offset {at}: {error}"),
            Self::Corruption {
                damaged_at,
                valid_at,
                sequence,
            } => write!(
                f,
                "corruption: the frame at offset {damaged_at} is invalid, but a valid frame \
                 with sequence {sequence} follows at offset {valid_at}"
            ),
            Self::TailTooLong { start, len } => write!(
                f,
                "{len} bytes after the durable prefix at offset {start} exceed one maximum \
                 frame ({MAX_FRAME_LEN} bytes)"
            ),
            Self::Storage(error) => write!(f, "journal read: {error}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// What starts at an offset.
enum Slot {
    /// A complete, checksum-valid frame.
    Valid {
        /// Its header.
        header: Header,
        /// Its payload.
        payload: Vec<u8>,
    },
    /// An intact header (its checksum holds, so its length is trustworthy, D-69) whose
    /// frame is cut short or fails its checksum.
    Torn(Header),
    /// An intact header holding a field this build never writes ([`FrameError::header_intact`]).
    Impossible(FrameError),
    /// No intact header.
    Damaged,
}

/// A byte length as a journal offset.
fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// What starts at `offset` in a journal of `journal_len` bytes. Reads at most one frame.
fn read_slot(
    storage: &impl JournalRead,
    offset: u64,
    journal_len: u64,
) -> Result<Slot, RecoveryError> {
    let available = journal_len.saturating_sub(offset);
    if available < len_u64(HEADER_LEN) {
        return Ok(Slot::Damaged);
    }
    let header_bytes = read_exact(storage, offset, HEADER_LEN).map_err(RecoveryError::Storage)?;
    let header = match decode_header(&header_bytes) {
        Ok(header) => header,
        Err(error) if error.header_intact() => return Ok(Slot::Impossible(error)),
        Err(_) => return Ok(Slot::Damaged),
    };
    if available < len_u64(header.frame_len()) {
        return Ok(Slot::Torn(header));
    }
    let bytes = read_exact(storage, offset, header.frame_len()).map_err(RecoveryError::Storage)?;
    let slot = decode(&bytes).map_or(Slot::Torn(header), |frame| Slot::Valid {
        header,
        payload: frame.payload().to_vec(),
    });
    Ok(slot)
}

/// The first complete, valid frame with a sequence above `last` that starts in
/// `from..journal_len`, as its offset and sequence. Reads the region in chunks of
/// `ReadLen::MAX` bytes, checking every offset where the magic occurs.
fn find_later_frame(
    storage: &impl JournalRead,
    from: u64,
    journal_len: u64,
    last: u64,
) -> Result<Option<(u64, u64)>, RecoveryError> {
    let chunk_len = u64::from(ReadLen::MAX);
    // Consecutive chunks overlap by the magic's length less one byte, so every offset
    // starts a full magic-sized window in exactly one chunk.
    let overlap = len_u64(MAGIC.len()) - 1;
    let mut chunk_start = from;
    while chunk_start < journal_len {
        let len = chunk_len.min(journal_len - chunk_start);
        let chunk = read_exact(
            storage,
            chunk_start,
            usize::try_from(len).unwrap_or(usize::MAX),
        )
        .map_err(RecoveryError::Storage)?;
        for (at, window) in chunk.windows(MAGIC.len()).enumerate() {
            if window != MAGIC.as_slice() {
                continue;
            }
            let candidate = chunk_start + len_u64(at);
            if let Slot::Valid { header, .. } = read_slot(storage, candidate, journal_len)?
                && header.sequence() > last
            {
                return Ok(Some((candidate, header.sequence())));
            }
        }
        if chunk_start + len >= journal_len {
            break;
        }
        chunk_start += len - overlap;
    }
    Ok(None)
}

/// Judges the remainder of the journal after the durable prefix, whose last valid frame
/// has sequence `last` and whose first invalid frame, `slot`, starts at `damaged_at`:
/// a torn tail of the returned shape (`Ok`), or a refusal.
fn judge_remainder(
    storage: &impl JournalRead,
    damaged_at: u64,
    slot: &Slot,
    journal_len: u64,
    last: u64,
) -> Result<TailShape, RecoveryError> {
    let (resume_at, shape) = match slot {
        Slot::Valid { header, .. } => {
            if header.sequence() > last {
                return Err(RecoveryError::Corruption {
                    damaged_at,
                    valid_at: damaged_at,
                    sequence: header.sequence(),
                });
            }
            (
                damaged_at.saturating_add(len_u64(header.frame_len())),
                TailShape::Stale {
                    sequence: header.sequence(),
                },
            )
        }
        Slot::Torn(header) => {
            let frame_len = len_u64(header.frame_len());
            let shape = if journal_len - damaged_at < frame_len {
                TailShape::Incomplete { frame_len }
            } else {
                TailShape::ChecksumFailed
            };
            (damaged_at.saturating_add(frame_len), shape)
        }
        Slot::Impossible(error) => {
            return Err(RecoveryError::ImpossibleHeader {
                at: damaged_at,
                error: *error,
            });
        }
        Slot::Damaged => (damaged_at.saturating_add(1), TailShape::Unreadable),
    };
    if let Some((valid_at, sequence)) = find_later_frame(storage, resume_at, journal_len, last)? {
        return Err(RecoveryError::Corruption {
            damaged_at,
            valid_at,
            sequence,
        });
    }
    let len = journal_len - damaged_at;
    if len > MAX_FRAME_LEN {
        return Err(RecoveryError::TailTooLong {
            start: damaged_at,
            len,
        });
    }
    Ok(shape)
}

/// The shortest genesis frame any valid configuration gives: a 1-byte cluster ID and one
/// voter. A genesis frame is 92 bytes plus its cluster ID plus 8 per voter: the frame header
/// (24) and trailer (8), the format version (2), the instance ID (16), the cluster ID's and
/// the voter count's length bytes (1 each), the node ID (8) and the fingerprint (32).
pub const MIN_GENESIS_FRAME_LEN: u64 = 92 + 1 + 8;

/// The longest genesis frame any valid configuration gives: a 255-byte cluster ID
/// (`ClusterId::MAX_BYTES`) and five voters (§7.10, D-45). Without the configuration, a
/// journal longer than this holds content, never an interrupted `init` (br-riea).
pub const MAX_GENESIS_FRAME_LEN: u64 = 92 + 255 + 8 * 5;

/// The length of the genesis frame `init` writes for `node` under `config`, if the
/// configuration lists the node. The instance ID's bytes do not change it.
fn genesis_frame_len(config: &Config, node: NodeId) -> Option<u64> {
    let genesis = Genesis::new(config, node, InstanceId::from_bytes([0; 16]))?;
    Some(len_u64(HEADER_LEN + genesis.encode().len() + TRAILER_LEN))
}

/// What reading a journal produced, shared by [`recover`] and the doctor's
/// [`examine`](crate::examine): the genesis, the logical log rebuilt from the durable
/// prefix, and the torn tail after it, if any.
#[derive(Debug)]
pub struct Scanned {
    /// The genesis.
    pub genesis: Genesis,
    /// The logical log of the durable prefix.
    pub log: LogicalLog,
    /// The torn tail recovery discards, if there is one.
    pub tail: Option<TornTail>,
}

/// A refusal, with what was read before it: for the doctor's report.
#[derive(Debug)]
pub struct Refusal {
    /// Why the journal cannot be recovered.
    pub error: RecoveryError,
    /// The genesis, if one was read.
    pub genesis: Option<Genesis>,
    /// The logical log of the frames applied before the refusal, if the genesis was read.
    pub log: Option<LogicalLog>,
}

/// The refusal `error`, before any genesis was read.
fn refuse(error: RecoveryError) -> Box<Refusal> {
    Box::new(Refusal {
        error,
        genesis: None,
        log: None,
    })
}

/// Reads and applies the frame at the log's end: `Ok(None)` once it is applied, or the
/// torn tail's shape when the durable prefix ends there.
fn next_frame(
    storage: &impl JournalRead,
    log: &mut LogicalLog,
    journal_len: u64,
) -> Result<Option<TailShape>, RecoveryError> {
    let at = log.end();
    let slot = read_slot(storage, at, journal_len)?;
    let last = log.next_sequence() - 1;
    let Slot::Valid { header, payload } = slot else {
        return judge_remainder(storage, at, &slot, journal_len, last).map(Some);
    };
    if header.kind() != FrameKind::RaftBatch || header.sequence() != log.next_sequence() {
        let slot = Slot::Valid { header, payload };
        return judge_remainder(storage, at, &slot, journal_len, last).map(Some);
    }
    let batch = RaftBatch::decode(&payload).map_err(|error| RecoveryError::Record { at, error })?;
    log.apply(at, header, batch)
        .map_err(|error| RecoveryError::Apply { at, error })?;
    Ok(None)
}

/// Reads the journal of `journal_len` bytes as startup recovery does (§7.6, §7.7 steps
/// 2-4), into a logical log within `bounds`. Only reads.
///
/// With `binding`, the genesis is verified against that configuration and node (DUR-8),
/// and a journal with no valid genesis is told apart from an interrupted `init` by the
/// genesis frame's length for them. Without one (the doctor examining a directory without
/// its configuration, O-7), the binding is unchecked, and only a journal longer than the
/// longest genesis frame any configuration gives is told apart (br-riea).
///
/// # Errors
///
/// A [`Refusal`]: the [`RecoveryError`] naming why the node must not start, with what was
/// read before it.
pub fn scan(
    storage: &impl JournalRead,
    journal_len: u64,
    binding: Option<(&Config, NodeId)>,
    bounds: LogBounds,
) -> Result<Scanned, Box<Refusal>> {
    if journal_len == 0 {
        return Err(refuse(RecoveryError::Empty));
    }
    let first = read_slot(storage, 0, journal_len).map_err(refuse)?;
    let Slot::Valid { header, payload } = first else {
        judge_remainder(storage, 0, &first, journal_len, 0).map_err(refuse)?;
        // An interrupted init leaves at most one genesis frame's worth of bytes: a torn
        // prefix, perhaps filled out with zeros or garbage to the append's full length.
        // Without the configuration, the longest genesis frame stands in (br-riea).
        let genesis_len = match binding {
            Some((config, node)) => genesis_frame_len(config, node),
            None => Some(MAX_GENESIS_FRAME_LEN),
        };
        if let Some(genesis_len) = genesis_len
            && journal_len > genesis_len
        {
            return Err(refuse(RecoveryError::NotAnInterruptedInit {
                len: journal_len,
                genesis_len,
            }));
        }
        return Err(refuse(RecoveryError::IncompleteGenesis));
    };
    if header.kind() != FrameKind::Genesis || header.sequence() != 1 {
        return Err(refuse(RecoveryError::NotGenesis {
            kind: header.kind(),
            sequence: header.sequence(),
        }));
    }
    let genesis =
        Genesis::decode(&payload).map_err(|error| refuse(RecoveryError::GenesisRecord(error)))?;
    if let Some((config, node)) = binding
        && let Err(mismatch) = genesis.verify(config, node)
    {
        return Err(Box::new(Refusal {
            error: RecoveryError::Mismatch(mismatch),
            genesis: Some(genesis),
            log: None,
        }));
    }
    let mut log = LogicalLog::after_genesis(len_u64(header.frame_len()), genesis.voters(), bounds);
    let mut tail = None;
    while log.end() < journal_len {
        match next_frame(storage, &mut log, journal_len) {
            Ok(None) => {}
            Ok(Some(shape)) => {
                tail = Some(TornTail {
                    start: log.end(),
                    len: journal_len - log.end(),
                    shape,
                });
                break;
            }
            Err(error) => {
                return Err(Box::new(Refusal {
                    error,
                    genesis: Some(genesis),
                    log: Some(log),
                }));
            }
        }
    }
    types::invariant!(
        log.end() <= journal_len,
        "JOURNAL-RECOVERY-PREFIX",
        "the durable prefix ends at {} beyond the journal's {journal_len} bytes",
        log.end()
    );
    Ok(Scanned { genesis, log, tail })
}

/// Recovers the journal of `journal_len` bytes for `node` under `config`, into a logical
/// log within `bounds` (in production, [`LogBounds::of`] the same config). Only reads.
///
/// The caller has already taken the data-directory lock and refused a missing journal
/// (§7.7 step 1). A journal at or past its hard stop still recovers: the recovered log
/// keeps the stop, so the first append that would pass it is refused and the node
/// re-enters `failed` (DM-7).
///
/// # Errors
///
/// The [`RecoveryError`] naming why the node must not start.
pub fn recover(
    storage: &impl JournalRead,
    journal_len: ByteCount,
    config: &Config,
    node: NodeId,
    bounds: LogBounds,
) -> Result<Recovered, RecoveryError> {
    let journal_len = journal_len.get();
    let scanned = scan(storage, journal_len, Some((config, node)), bounds)
        .map_err(|refusal| refusal.error)?;
    Ok(Recovered {
        genesis: scanned.genesis,
        log: scanned.log,
        journal_len,
    })
}
