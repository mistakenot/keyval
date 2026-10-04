//! A bounded scan of a peer message's protobuf bytes, run before the protobuf runtime parses
//! anything (PLAN.md §7.11, CD-3, CD-9; W02 audit H-3; br-umjh, br-808c).
//!
//! The runtime allocates as it parses. It reserves a buffer for a declared length before it
//! checks that the bytes exist, builds a value for every element of a repeated field, keeps
//! unknown fields in a map, and skips a group without a trace. So a few bytes could make it
//! reserve megabytes, and a group could hide a field the H-3 check never sees. The scan reads
//! the wire format itself and allocates nothing. It passes only what the `eraftpb` schema
//! allows, at every depth, for `Message`, `Entry`, `Snapshot`, `SnapshotMetadata` and
//! `ConfState`:
//!
//! - **Fields:** known field numbers, each with its own wire type. There are no groups, no
//!   unknown fields and no other wire types.
//! - **Lengths:** every declared length lies within the bytes that remain, and every varint
//!   is at most ten bytes and fits 64 bits. The runtime therefore never reserves a buffer
//!   for bytes that are not there, and what it copies is at most the message itself.
//! - **Repeated fields:** at most [`MAX_ENTRIES`] entries, the most a v1 append carries,
//!   and at most [`MAX_CONF_STATE_IDS`] node IDs in a snapshot's `ConfState`. The runtime
//!   builds a whole value for each one, however few bytes encode it.
//!
//! What admission judges by value is left to admission, which refuses it by name after the
//! parse, in its own order. That covers terms, indices, contexts, command sizes, and a
//! well-formed snapshot (§7.3).

use types::ConfigParam;

/// The smallest `compute_size` of an entry admission accepts: a term and an index of at
/// least 1, a tag byte and a varint byte each.
const MIN_ENTRY_SIZE: u64 = 4;

/// The most entries a v1 append carries. raft-rs's `limit_size` sends one entry, then more
/// while their summed `compute_size` stays within `max_size_per_msg`, the replication batch
/// (`replication_batch_bytes`, at most 256 KiB, §7.11). So 256 KiB ÷ 4 + 1: 65,537.
pub const MAX_ENTRIES: u64 = ConfigParam::ReplicationBatchBytes.maximum() / MIN_ENTRY_SIZE + 1;

/// The most node IDs a snapshot's `ConfState` may list across its four ID fields: the most
/// voters a v1 configuration has (1, 3 or 5, D-45). v1 has no learners or joint
/// configurations, and admission refuses every snapshot anyway; this only bounds what the
/// runtime builds before it does.
pub const MAX_CONF_STATE_IDS: u64 = 5;

/// The varint wire type.
const VARINT: u64 = 0;
/// The length-delimited wire type.
const LEN: u64 = 2;

/// Bytes the scan does not pass (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

/// A message type of the `eraftpb` schema a peer message may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Schema {
    /// `eraftpb.Message`.
    Message,
    /// `eraftpb.Entry`.
    Entry,
    /// `eraftpb.Snapshot`.
    Snapshot,
    /// `eraftpb.SnapshotMetadata`.
    SnapshotMetadata,
    /// `eraftpb.ConfState`.
    ConfState,
}

/// How the scan treats a field.
#[derive(Clone, Copy)]
enum Field {
    /// An integer, enum or bool: a varint.
    Scalar,
    /// Bytes: length-delimited, not looked into.
    Bytes,
    /// A nested message: length-delimited, scanned with its own schema.
    Nested(Schema),
    /// One of `ConfState`'s repeated node IDs: a varint, or a packed run of them.
    Id,
}

impl Schema {
    /// Field `number` of this message type, if it has one (`eraftpb.proto`).
    const fn field(self, number: u64) -> Option<Field> {
        match self {
            Self::Message => match number {
                1..=6 | 8 | 10 | 11 | 13..=16 => Some(Field::Scalar),
                7 => Some(Field::Nested(Self::Entry)),
                9 => Some(Field::Nested(Self::Snapshot)),
                12 => Some(Field::Bytes),
                _ => None,
            },
            Self::Entry => match number {
                1..=3 | 5 => Some(Field::Scalar),
                4 | 6 => Some(Field::Bytes),
                _ => None,
            },
            Self::Snapshot => match number {
                1 => Some(Field::Bytes),
                2 => Some(Field::Nested(Self::SnapshotMetadata)),
                _ => None,
            },
            Self::SnapshotMetadata => match number {
                1 => Some(Field::Nested(Self::ConfState)),
                2 | 3 => Some(Field::Scalar),
                _ => None,
            },
            Self::ConfState => match number {
                1..=4 => Some(Field::Id),
                5 => Some(Field::Scalar),
                _ => None,
            },
        }
    }
}

/// The repeated values the runtime would build, counted against their bounds.
#[derive(Default)]
struct Counts {
    /// Entries.
    entries: u64,
    /// `ConfState` node IDs.
    ids: u64,
}

impl Counts {
    /// Counts a nested message of `schema`: an entry counts against [`MAX_ENTRIES`].
    const fn nested(&mut self, schema: Schema) -> Result<(), Malformed> {
        match schema {
            Schema::Entry => Self::bump(&mut self.entries, MAX_ENTRIES),
            Schema::Message | Schema::Snapshot | Schema::SnapshotMetadata | Schema::ConfState => {
                Ok(())
            }
        }
    }

    /// Counts one `ConfState` node ID against [`MAX_CONF_STATE_IDS`].
    const fn id(&mut self) -> Result<(), Malformed> {
        Self::bump(&mut self.ids, MAX_CONF_STATE_IDS)
    }

    /// Adds one to `count`, which may not pass `bound`.
    const fn bump(count: &mut u64, bound: u64) -> Result<(), Malformed> {
        *count += 1;
        if *count > bound {
            return Err(Malformed);
        }
        Ok(())
    }
}

/// The unread rest of the bytes.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// The next varint: at most ten bytes, fitting 64 bits.
    fn varint(&mut self) -> Result<u64, Malformed> {
        let mut value = 0_u64;
        for (at, byte) in self.0.iter().take(10).enumerate() {
            let bits = u64::from(byte & 0x7f);
            if at == 9 && bits > 1 {
                return Err(Malformed);
            }
            value |= bits << (7 * at);
            if byte & 0x80 == 0 {
                self.0 = self.0.get(at + 1..).unwrap_or_default();
                return Ok(value);
            }
        }
        Err(Malformed)
    }

    /// The next length-delimited value, which must lie within the bytes that remain.
    fn delimited(&mut self) -> Result<&'a [u8], Malformed> {
        let len = usize::try_from(self.varint()?).map_err(|_| Malformed)?;
        let value = self.0.get(..len).ok_or(Malformed)?;
        self.0 = self.0.get(len..).unwrap_or_default();
        Ok(value)
    }
}

/// Scans a peer message's bytes before they are parsed (module documentation).
///
/// # Errors
///
/// [`Malformed`] for anything the scan does not pass.
pub fn scan(bytes: &[u8]) -> Result<(), Malformed> {
    scan_as(bytes, Schema::Message, &mut Counts::default())
}

/// Scans `bytes` as a message of `schema`. The schema has no cycles, so the recursion is
/// at most four deep.
fn scan_as(bytes: &[u8], schema: Schema, counts: &mut Counts) -> Result<(), Malformed> {
    let mut reader = Reader(bytes);
    while !reader.0.is_empty() {
        let key = reader.varint()?;
        let (number, wire) = (key >> 3, key & 0b111);
        match (schema.field(number), wire) {
            (Some(Field::Scalar), VARINT) => {
                reader.varint()?;
            }
            (Some(Field::Bytes), LEN) => {
                reader.delimited()?;
            }
            (Some(Field::Nested(inner)), LEN) => {
                counts.nested(inner)?;
                scan_as(reader.delimited()?, inner, counts)?;
            }
            (Some(Field::Id), VARINT) => {
                counts.id()?;
                reader.varint()?;
            }
            (Some(Field::Id), LEN) => {
                let mut packed = Reader(reader.delimited()?);
                while !packed.0.is_empty() {
                    counts.id()?;
                    packed.varint()?;
                }
            }
            (Some(Field::Scalar | Field::Bytes | Field::Nested(_) | Field::Id) | None, _) => {
                return Err(Malformed);
            }
        }
    }
    Ok(())
}
