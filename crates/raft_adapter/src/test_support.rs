//! Test-only helpers shared by this crate's tests:
//! - seeded entropy;
//! - an in-memory journal, empty or starting with a real genesis frame;
//! - the committed fixture configs.
#![cfg(test)]

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use journal::{FrameKind, Genesis, LogBounds, LogicalLog, RaftBatch};
use types::{
    ByteCount, Config, Entropy, EntropyStream, Entry, EntryIndex, EntryPayload, HardState,
    InstanceId, JournalRead, NodeId, ReadLen, StorageError, Term,
};

/// A seeded `SplitMix64` entropy port: these tests' only randomness (§7.14).
#[derive(Debug)]
pub struct Seeded(u64);

impl Seeded {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }
}

impl Entropy for Seeded {
    fn next_u64(&mut self, stream: EntropyStream) -> u64 {
        match stream {
            EntropyStream::ElectionTimeout => {
                self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            }
        }
    }
}

/// An in-memory journal, shared between a test that appends frames and the storage view
/// that reads them back. A test can make every later read fail, as an I/O error would.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    bytes: Rc<RefCell<Vec<u8>>>,
    failing: Rc<Cell<bool>>,
}

impl Journal {
    pub fn push(&self, frame: &[u8]) {
        self.bytes.borrow_mut().extend_from_slice(frame);
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.borrow().is_empty()
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }

    /// Makes every later read fail with `StorageError::Failed`.
    pub fn fail_reads(&self) {
        self.failing.set(true);
    }
}

impl JournalRead for Journal {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        if self.failing.get() {
            return Err(StorageError::Failed);
        }
        let bytes = self.bytes.borrow();
        let start = usize::try_from(offset.get()).map_err(|_| StorageError::Failed)?;
        let wanted = usize::try_from(len.get()).map_err(|_| StorageError::Failed)?;
        let available = bytes.get(start..).unwrap_or_default();
        if available.len() < wanted {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(available.len()).unwrap_or(u32::MAX),
            });
        }
        Ok(available[..wanted].to_vec())
    }
}

/// An empty logical log after a zero-byte genesis. Its 16-byte entry cache sends most reads
/// to the journal. It records the voters of every committed config fixture ([`CONFIGS`]),
/// because the journal refuses a persisted vote for a node outside its genesis voters
/// (br-biok), and these tests run clusters from any fixture.
pub fn empty_log() -> LogicalLog {
    let voters: Vec<NodeId> = CONFIGS
        .iter()
        .flat_map(|(name, _)| config(name).voters().collect::<Vec<_>>())
        .collect();
    LogicalLog::after_genesis(
        0,
        &voters,
        LogBounds::new(ByteCount::new(16), ByteCount::new(1 << 30)),
    )
}

/// The bounds of these tests' logs: a 16-byte entry cache, and a hard stop far away.
pub const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(16), ByteCount::new(1 << 30));

/// A journal holding only node `node`'s genesis frame under `config` (§7.7 step 0), and the
/// empty logical log that follows it: what `journal::recover` reads back.
pub fn journal_with_genesis(config: &Config, node: NodeId) -> (Journal, LogicalLog) {
    let instance = InstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
    let genesis = Genesis::new(config, node, instance).unwrap();
    let frame = journal::encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap();
    let journal = Journal::default();
    journal.push(&frame);
    let voters: Vec<NodeId> = config.voters().collect();
    let log = LogicalLog::after_genesis(u64::try_from(frame.len()).unwrap(), &voters, BOUNDS);
    (journal, log)
}

pub fn entry(index: u64, term: u64, command: &[u8]) -> Entry {
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: EntryPayload::Command(command.to_vec()),
    }
}

/// Appends one frame holding `entries` and `hard_state` to `log` and `journal`.
pub fn persist_batch(
    log: &mut LogicalLog,
    journal: &Journal,
    entries: Vec<Entry>,
    hard_state: Option<HardState>,
) {
    let frame = log
        .append(RaftBatch::new(entries, hard_state).unwrap())
        .unwrap();
    journal.push(&frame);
}

/// Every committed config: the fixture corpus W08 commits in `config/fixtures/` (D-78), and
/// `config/local.toml`.
pub const CONFIGS: [(&str, &str); 6] = [
    (
        "fastest-timing",
        include_str!("../../../config/fixtures/fastest-timing.toml"),
    ),
    (
        "five-voters-explicit-limits",
        include_str!("../../../config/fixtures/five-voters-explicit-limits.toml"),
    ),
    (
        "one-voter",
        include_str!("../../../config/fixtures/one-voter.toml"),
    ),
    (
        "small-limits",
        include_str!("../../../config/fixtures/small-limits.toml"),
    ),
    (
        "three-voters",
        include_str!("../../../config/fixtures/three-voters.toml"),
    ),
    ("local", include_str!("../../../config/local.toml")),
];

/// The committed config called `name`, parsed.
pub fn config(name: &str) -> Config {
    let (_, text) = CONFIGS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap();
    Config::parse(text, Path::new("config/fixtures")).unwrap()
}
