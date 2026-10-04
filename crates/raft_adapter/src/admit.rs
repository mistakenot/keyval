//! Admission of peer Raft messages (PLAN.md §7.3, §7.8, §8.2; W02 audit S-1 and H-3).
//!
//! raft-rs acts on any message it is given. It grants a vote to a node outside the voter
//! set, appends a `MsgPropose` from the wire without the core's admission (DM-7, §7.8),
//! restores a snapshot v1 never takes, and panics (`fatal!`) on some malformed input. So the
//! adapter steps only what v1 peers actually exchange, and refuses everything else before the
//! library sees it:
//!
//! - **Message types:** `MsgAppend`, `MsgHeartbeat`, `MsgRequestVote` and
//!   `MsgRequestPreVote`, and their responses. Leader transfer (`MsgTimeoutNow`) and
//!   linearizable reads (`MsgReadIndex`, R-04) are not in v1, and the other types are local.
//! - **Addressing:** to this node, from another voter.
//! - **Snapshots:** none, and no snapshot request. v1 never compacts (§7.3).
//! - **Fields v1 never sets:** no message context (it carries only read-index and
//!   leader-transfer data) and no election priority.
//! - **Range:** every term and index at most 2^53 − 1 (R-22).
//! - **Entries:** only on `MsgAppend`. Each entry is `EntryNormal`, without a context or
//!   `sync_log`, and holds at most one command ([`MAX_COMMAND_BYTES`]). Entries follow the
//!   message's index without a gap. Their terms run from the message's `log_term` up to its
//!   `term`, never decreasing, and are never 0.
//! - **Unknown fields:** refused at every depth. [`decode_message`] refuses them on the
//!   message and its entries, and the only other nested message, the snapshot, is refused
//!   outright.

use raft::eraftpb::{self, EntryType, MessageType};
use types::{MAX_COMMAND_BYTES, MAX_SAFE_INTEGER, NodeId};

use crate::convert::decode_message;

/// Why a peer message was refused before it reached the library. Each variant names the
/// rule it broke (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inadmissible {
    /// The bytes are not a Raft message, or carry a field this build does not know.
    Undecodable,
    /// A message type v1 peers never exchange; `code` is its protobuf number.
    MessageType {
        /// The message type's protobuf number.
        code: i32,
    },
    /// Addressed to another node.
    Destination {
        /// The node it was addressed to.
        to: u64,
    },
    /// Not from another voter.
    Sender {
        /// The node it claims to come from.
        from: u64,
    },
    /// It carries a snapshot or a snapshot request.
    Snapshot,
    /// It sets a context or an election priority, which v1 never uses.
    UnusedField,
    /// A term or index above 2^53 − 1.
    OutOfRange,
    /// The entry at `position` is not one v1 replicates: entries on a message type that
    /// carries none, a configuration change, a context, `sync_log`, or more than one
    /// command's bytes.
    Entry {
        /// The entry's position in the message, from 0.
        position: usize,
    },
    /// The entry at `position` does not follow its predecessor: a gap in the indices, a
    /// term of 0, a term below the previous one, or a term past the message's.
    EntryOrder {
        /// The entry's position in the message, from 0.
        position: usize,
    },
}

/// The peer message in `bytes`, decoded, if node `node` among `voters` admits it (module
/// documentation).
///
/// # Errors
///
/// [`Inadmissible`] names the first rule the message breaks.
pub fn admit(
    bytes: &[u8],
    node: NodeId,
    voters: &[NodeId],
) -> Result<eraftpb::Message, Inadmissible> {
    let message = decode_message(bytes).ok_or(Inadmissible::Undecodable)?;
    let kind = message.get_msg_type();
    let carries_entries = exchanged(kind).ok_or_else(|| Inadmissible::MessageType {
        code: protobuf::ProtobufEnum::value(&kind),
    })?;
    if message.to != node.get() {
        return Err(Inadmissible::Destination { to: message.to });
    }
    if message.from == node.get() || !voters.iter().any(|voter| voter.get() == message.from) {
        return Err(Inadmissible::Sender { from: message.from });
    }
    if message.has_snapshot() || message.request_snapshot != 0 {
        return Err(Inadmissible::Snapshot);
    }
    if !message.context.is_empty() || message.priority != 0 || message.deprecated_priority != 0 {
        return Err(Inadmissible::UnusedField);
    }
    let numbers = [
        message.term,
        message.log_term,
        message.index,
        message.commit,
        message.commit_term,
        message.reject_hint,
    ];
    if numbers.iter().any(|&number| number > MAX_SAFE_INTEGER) {
        return Err(Inadmissible::OutOfRange);
    }
    if !carries_entries && !message.entries.is_empty() {
        return Err(Inadmissible::Entry { position: 0 });
    }
    check_entries(&message)?;
    Ok(message)
}

/// Whether v1 peers exchange messages of `kind`: `Some(true)` if they may carry entries,
/// `Some(false)` if they never do, and `None` if v1 peers never send it.
const fn exchanged(kind: MessageType) -> Option<bool> {
    match kind {
        MessageType::MsgAppend => Some(true),
        MessageType::MsgAppendResponse
        | MessageType::MsgHeartbeat
        | MessageType::MsgHeartbeatResponse
        | MessageType::MsgRequestVote
        | MessageType::MsgRequestVoteResponse
        | MessageType::MsgRequestPreVote
        | MessageType::MsgRequestPreVoteResponse => Some(false),
        MessageType::MsgHup
        | MessageType::MsgBeat
        | MessageType::MsgPropose
        | MessageType::MsgSnapshot
        | MessageType::MsgUnreachable
        | MessageType::MsgSnapStatus
        | MessageType::MsgCheckQuorum
        | MessageType::MsgTransferLeader
        | MessageType::MsgTimeoutNow
        | MessageType::MsgReadIndex
        | MessageType::MsgReadIndexResp => None,
    }
}

/// Checks `message`'s entries: each is one v1 replicates, and each follows its predecessor
/// (module documentation).
fn check_entries(message: &eraftpb::Message) -> Result<(), Inadmissible> {
    let mut previous_term = message.log_term;
    for (position, entry) in message.entries.iter().enumerate() {
        let normal = match entry.get_entry_type() {
            EntryType::EntryNormal => true,
            EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => false,
        };
        let data_len = u64::try_from(entry.data.len()).unwrap_or(u64::MAX);
        if !normal || !entry.context.is_empty() || entry.sync_log || data_len > MAX_COMMAND_BYTES {
            return Err(Inadmissible::Entry { position });
        }
        if entry.index > MAX_SAFE_INTEGER || entry.term > MAX_SAFE_INTEGER {
            return Err(Inadmissible::OutOfRange);
        }
        let expected = u64::try_from(position)
            .ok()
            .and_then(|offset| message.index.checked_add(offset))
            .and_then(|index| index.checked_add(1));
        if expected != Some(entry.index)
            || entry.term == 0
            || entry.term < previous_term
            || entry.term > message.term
        {
            return Err(Inadmissible::EntryOrder { position });
        }
        previous_term = entry.term;
    }
    Ok(())
}
