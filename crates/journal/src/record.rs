//! Journal records (PLAN.md §7.6): the canonical payloads of genesis and Raft-batch
//! frames. All integers are little-endian; `golden/README.md` documents each layout
//! byte by byte.
//!
//! **Genesis payload:** format version (u16), instance ID (16 bytes), cluster ID (u8
//! length, then UTF-8), node ID (u64), voter count (u8), voter IDs ascending (u64 each),
//! semantic-config fingerprint (32 bytes).
//!
//! **Raft-batch payload:** entry count (u32). Then, per entry: index (u64), term (u64),
//! payload kind (u8: 0 empty, 1 command), data length (u32), data. Then a hard-state flag
//! (u8: 0 absent, 1 present), and, when present, term (u64), vote (u64, 0 for none) and
//! commit index (u64).
//!
//! A one-entry batch spends 26 bytes beyond the command: 4 for the count, 21 for the
//! entry, 1 for the flag. That is within `types::ENTRY_ENVELOPE_BOUND` (32 B), so the
//! journal margin formula's 64 B per entry holds (§7.10, D-68). A present hard state
//! adds 24 bytes per frame. That is protocol overhead, inside
//! `types::JOURNAL_PROTOCOL_ALLOWANCE`, even at one frame per entry for the most entries
//! a config allows.

use core::fmt;

use types::{
    ClusterId, ClusterIdError, Config, Entry, EntryIndex, EntryPayload, HardState, InstanceId,
    LogIndex, MAX_COMMAND_BYTES, NodeId, OutOfRange, SemanticFingerprint, Term,
};

use crate::frame::FORMAT_VERSION;

/// Why a payload is not a valid record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The payload ends inside `field`.
    Truncated {
        /// The field being read.
        field: &'static str,
    },
    /// Bytes remain after the record.
    TrailingBytes {
        /// How many.
        extra: usize,
    },
    /// A genesis written by another format version.
    UnsupportedFormat {
        /// The stored version.
        version: u16,
    },
    /// A cluster ID that is not UTF-8.
    ClusterIdNotUtf8,
    /// An invalid cluster ID.
    ClusterId(ClusterIdError),
    /// A node ID (the node, a voter or a vote) that is zero or not below 2^53.
    NodeId(OutOfRange),
    /// A voter count other than 1, 3 or 5 (D-45).
    VoterCount {
        /// The stored count.
        count: usize,
    },
    /// Voters not in strictly ascending order: the canonical voter set (§7.10).
    VotersNotCanonical,
    /// The node is not one of the voters.
    NodeNotAVoter {
        /// The node.
        node: NodeId,
    },
    /// An entry index that is zero or not below 2^53.
    EntryIndex(OutOfRange),
    /// A term not below 2^53.
    Term(OutOfRange),
    /// A commit index not below 2^53.
    Commit(OutOfRange),
    /// A batch's entries are not consecutive.
    EntryGap {
        /// The index that should have come next.
        expected: u64,
        /// The index found.
        found: u64,
    },
    /// An entry payload kind that names nothing.
    UnknownPayloadKind {
        /// The stored byte.
        code: u8,
    },
    /// An empty entry that carries data.
    EmptyEntryWithData {
        /// The stored data length.
        len: u32,
    },
    /// A command longer than `types::MAX_COMMAND_BYTES` (66 KiB, §7.11).
    CommandTooLarge {
        /// The command's length.
        len: u64,
    },
    /// A hard-state flag other than 0 or 1.
    HardStateFlag {
        /// The stored byte.
        value: u8,
    },
    /// A batch with neither entries nor a hard state: nothing to persist (§7.4).
    EmptyBatch,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { field } => write!(f, "record truncated in {field}"),
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the record"),
            Self::UnsupportedFormat { version } => {
                write!(
                    f,
                    "genesis format version {version} is not {FORMAT_VERSION}"
                )
            }
            Self::ClusterIdNotUtf8 => f.write_str("cluster ID is not UTF-8"),
            Self::ClusterId(error) => write!(f, "cluster ID: {error}"),
            Self::NodeId(error) => write!(f, "node ID: {error}"),
            Self::VoterCount { count } => write!(f, "{count} voters; a cluster has 1, 3 or 5"),
            Self::VotersNotCanonical => f.write_str("voters are not strictly ascending"),
            Self::NodeNotAVoter { node } => write!(f, "node {node} is not a voter"),
            Self::EntryIndex(error) => write!(f, "entry index: {error}"),
            Self::Term(error) => write!(f, "term: {error}"),
            Self::Commit(error) => write!(f, "commit index: {error}"),
            Self::EntryGap { expected, found } => {
                write!(f, "entry {found} follows where {expected} should")
            }
            Self::UnknownPayloadKind { code } => write!(f, "unknown entry payload kind {code}"),
            Self::EmptyEntryWithData { len } => write!(f, "an empty entry carries {len} bytes"),
            Self::CommandTooLarge { len } => {
                write!(f, "a {len}-byte command exceeds {MAX_COMMAND_BYTES}")
            }
            Self::HardStateFlag { value } => write!(f, "hard-state flag {value} is not 0 or 1"),
            Self::EmptyBatch => f.write_str("a batch with neither entries nor a hard state"),
        }
    }
}

impl std::error::Error for RecordError {}

/// Reads a payload front to back.
struct Reader<'a> {
    /// The unread bytes.
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// The next `len` bytes.
    const fn take(&mut self, len: usize, field: &'static str) -> Result<&'a [u8], RecordError> {
        if self.rest.len() < len {
            return Err(RecordError::Truncated { field });
        }
        let (taken, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(taken)
    }

    /// The next `N` bytes as an array.
    fn array<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N], RecordError> {
        self.take(N, field)?
            .try_into()
            .map_err(|_| RecordError::Truncated { field })
    }

    /// The next byte.
    fn u8(&mut self, field: &'static str) -> Result<u8, RecordError> {
        self.array::<1>(field).map(|[byte]| byte)
    }

    /// The next little-endian `u16`.
    fn u16(&mut self, field: &'static str) -> Result<u16, RecordError> {
        self.array(field).map(u16::from_le_bytes)
    }

    /// The next little-endian `u32`.
    fn u32(&mut self, field: &'static str) -> Result<u32, RecordError> {
        self.array(field).map(u32::from_le_bytes)
    }

    /// The next little-endian `u64`.
    fn u64(&mut self, field: &'static str) -> Result<u64, RecordError> {
        self.array(field).map(u64::from_le_bytes)
    }

    /// Refuses unread bytes.
    const fn finish(self) -> Result<(), RecordError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(RecordError::TrailingBytes {
                extra: self.rest.len(),
            })
        }
    }
}

/// The genesis record: the first frame of every journal, binding the data directory to
/// its cluster instance, node, voter set and semantic configuration (DUR-8, §7.7 step 0).
///
/// Invariants: the voters are 1, 3 or 5 distinct IDs in ascending order (the canonical
/// voter set), and the node is one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Genesis {
    /// The cluster instance ID stamped at `init` (D-63).
    instance_id: InstanceId,
    /// The display cluster ID.
    cluster_id: ClusterId,
    /// This data directory's node.
    node_id: NodeId,
    /// The voter set, ascending.
    voters: Vec<NodeId>,
    /// The semantic-config fingerprint (DUR-8).
    fingerprint: SemanticFingerprint,
}

impl Genesis {
    /// The genesis for `node` under `config`, stamped with `instance_id`, or `None` if
    /// `node` is not one of the config's voters.
    #[must_use]
    pub fn new(config: &Config, node: NodeId, instance_id: InstanceId) -> Option<Self> {
        config.node(node)?;
        Some(Self {
            instance_id,
            cluster_id: config.cluster_id().clone(),
            node_id: node,
            voters: config.voters().collect(),
            fingerprint: config.semantic_fingerprint(),
        })
    }

    /// A genesis from its parts, such as one read back from storage.
    ///
    /// # Errors
    ///
    /// [`RecordError::VoterCount`], [`RecordError::VotersNotCanonical`] or
    /// [`RecordError::NodeNotAVoter`] if the invariants do not hold.
    pub fn from_parts(
        instance_id: InstanceId,
        cluster_id: ClusterId,
        node_id: NodeId,
        voters: Vec<NodeId>,
        fingerprint: SemanticFingerprint,
    ) -> Result<Self, RecordError> {
        if voters.len() != 1 && voters.len() != 3 && voters.len() != 5 {
            return Err(RecordError::VoterCount {
                count: voters.len(),
            });
        }
        if voters
            .iter()
            .zip(voters.iter().skip(1))
            .any(|(earlier, later)| earlier >= later)
        {
            return Err(RecordError::VotersNotCanonical);
        }
        if !voters.contains(&node_id) {
            return Err(RecordError::NodeNotAVoter { node: node_id });
        }
        Ok(Self {
            instance_id,
            cluster_id,
            node_id,
            voters,
            fingerprint,
        })
    }

    /// The cluster instance ID.
    #[must_use]
    pub const fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// The display cluster ID.
    #[must_use]
    pub const fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    /// This data directory's node.
    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// The voter set, ascending.
    #[must_use]
    pub fn voters(&self) -> &[NodeId] {
        &self.voters
    }

    /// The semantic-config fingerprint.
    #[must_use]
    pub const fn fingerprint(&self) -> SemanticFingerprint {
        self.fingerprint
    }

    /// The canonical payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let cluster = self.cluster_id.as_str().as_bytes();
        let mut out = Vec::with_capacity(60 + cluster.len() + 8 * self.voters.len());
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.instance_id.to_bytes());
        out.push(u8::try_from(cluster.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(cluster);
        out.extend_from_slice(&self.node_id.get().to_le_bytes());
        out.push(u8::try_from(self.voters.len()).unwrap_or(u8::MAX));
        for voter in &self.voters {
            out.extend_from_slice(&voter.get().to_le_bytes());
        }
        out.extend_from_slice(&self.fingerprint.to_bytes());
        out
    }

    /// Decodes and validates a genesis payload.
    ///
    /// # Errors
    ///
    /// The first [`RecordError`] found, in field order.
    pub fn decode(payload: &[u8]) -> Result<Self, RecordError> {
        let mut reader = Reader { rest: payload };
        let version = reader.u16("format version")?;
        if version != FORMAT_VERSION {
            return Err(RecordError::UnsupportedFormat { version });
        }
        let instance_id = InstanceId::from_bytes(reader.array("instance ID")?);
        let cluster_len = usize::from(reader.u8("cluster ID length")?);
        let cluster_bytes = reader.take(cluster_len, "cluster ID")?;
        let cluster_text = core::str::from_utf8(cluster_bytes)
            .map_err(|_| RecordError::ClusterIdNotUtf8)?
            .to_owned();
        let cluster_id = ClusterId::parse(cluster_text).map_err(RecordError::ClusterId)?;
        let node_id = NodeId::new(reader.u64("node ID")?).map_err(RecordError::NodeId)?;
        let count = usize::from(reader.u8("voter count")?);
        if count != 1 && count != 3 && count != 5 {
            return Err(RecordError::VoterCount { count });
        }
        let mut voters = Vec::with_capacity(count);
        for _ in 0..count {
            voters.push(NodeId::new(reader.u64("voter ID")?).map_err(RecordError::NodeId)?);
        }
        let fingerprint = SemanticFingerprint::from_bytes(reader.array("fingerprint")?);
        reader.finish()?;
        Self::from_parts(instance_id, cluster_id, node_id, voters, fingerprint)
    }
}

/// One persisted Ready (§7.4 step 3): new entries, then the hard state if it changed.
///
/// Invariants: entries have consecutive indices; each command is at most
/// `types::MAX_COMMAND_BYTES`; there are entries or a hard state, never neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaftBatch {
    /// New entries, in index order.
    entries: Vec<Entry>,
    /// The hard state, if it changed.
    hard_state: Option<HardState>,
}

impl RaftBatch {
    /// A batch of `entries` and `hard_state`.
    ///
    /// # Errors
    ///
    /// [`RecordError::EntryGap`] if the entries are not consecutive,
    /// [`RecordError::CommandTooLarge`] for an oversized command, or
    /// [`RecordError::EmptyBatch`] if there is nothing to persist.
    pub fn new(entries: Vec<Entry>, hard_state: Option<HardState>) -> Result<Self, RecordError> {
        if entries.is_empty() && hard_state.is_none() {
            return Err(RecordError::EmptyBatch);
        }
        for (earlier, later) in entries.iter().zip(entries.iter().skip(1)) {
            let expected = earlier.index.get().saturating_add(1);
            if later.index.get() != expected {
                return Err(RecordError::EntryGap {
                    expected,
                    found: later.index.get(),
                });
            }
        }
        for entry in &entries {
            match &entry.payload {
                EntryPayload::Empty => {}
                EntryPayload::Command(bytes) => {
                    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    if len > MAX_COMMAND_BYTES {
                        return Err(RecordError::CommandTooLarge { len });
                    }
                }
            }
        }
        Ok(Self {
            entries,
            hard_state,
        })
    }

    /// New entries, in index order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The hard state, if it changed.
    #[must_use]
    pub const fn hard_state(&self) -> Option<HardState> {
        self.hard_state
    }

    /// The batch's parts, by value.
    #[must_use]
    pub fn into_parts(self) -> (Vec<Entry>, Option<HardState>) {
        (self.entries, self.hard_state)
    }

    /// The canonical payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(
            &u32::try_from(self.entries.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        for entry in &self.entries {
            out.extend_from_slice(&entry.index.get().to_le_bytes());
            out.extend_from_slice(&entry.term.get().to_le_bytes());
            let data: &[u8] = match &entry.payload {
                EntryPayload::Empty => {
                    out.push(0);
                    &[]
                }
                EntryPayload::Command(bytes) => {
                    out.push(1);
                    bytes
                }
            };
            out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
            out.extend_from_slice(data);
        }
        match self.hard_state {
            None => out.push(0),
            Some(state) => {
                out.push(1);
                out.extend_from_slice(&state.term.get().to_le_bytes());
                out.extend_from_slice(&state.vote.map_or(0, NodeId::get).to_le_bytes());
                out.extend_from_slice(&state.commit.get().to_le_bytes());
            }
        }
        out
    }

    /// Decodes and validates a Raft-batch payload.
    ///
    /// # Errors
    ///
    /// The first [`RecordError`] found, in field order, then the [`RaftBatch::new`]
    /// rules.
    pub fn decode(payload: &[u8]) -> Result<Self, RecordError> {
        let mut reader = Reader { rest: payload };
        let count = reader.u32("entry count")?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let index =
                EntryIndex::new(reader.u64("entry index")?).map_err(RecordError::EntryIndex)?;
            let term = Term::new(reader.u64("entry term")?).map_err(RecordError::Term)?;
            let code = reader.u8("entry payload kind")?;
            let len = reader.u32("entry data length")?;
            let payload = match code {
                0 if len == 0 => EntryPayload::Empty,
                0 => return Err(RecordError::EmptyEntryWithData { len }),
                1 => {
                    if u64::from(len) > MAX_COMMAND_BYTES {
                        return Err(RecordError::CommandTooLarge {
                            len: u64::from(len),
                        });
                    }
                    let data =
                        reader.take(usize::try_from(len).unwrap_or(usize::MAX), "entry data")?;
                    EntryPayload::Command(data.to_vec())
                }
                _ => return Err(RecordError::UnknownPayloadKind { code }),
            };
            entries.push(Entry {
                index,
                term,
                payload,
            });
        }
        let hard_state = match reader.u8("hard-state flag")? {
            0 => None,
            1 => {
                let term = Term::new(reader.u64("hard-state term")?).map_err(RecordError::Term)?;
                let vote = match reader.u64("hard-state vote")? {
                    0 => None,
                    raw => Some(NodeId::new(raw).map_err(RecordError::NodeId)?),
                };
                let commit =
                    LogIndex::new(reader.u64("hard-state commit")?).map_err(RecordError::Commit)?;
                Some(HardState { term, vote, commit })
            }
            value => return Err(RecordError::HardStateFlag { value }),
        };
        reader.finish()?;
        Self::new(entries, hard_state)
    }
}
