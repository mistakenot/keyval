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
//! - **Range:** a term of at least 1, and every term and index at most 2^53 − 1 (R-22).
//!   raft-rs treats term 0 as a local message and skips every term rule, so a forged
//!   term-0 vote request would be granted with term 0 and the library would `fatal!` on
//!   sending the answer (br-e1sd). A v1 peer never sends term 0: every message carries its
//!   sender's term, and a vote or pre-vote answer carries the request's.
//! - **Entries:** only on `MsgAppend`. Each entry is `EntryNormal`, without a context or
//!   `sync_log`, and holds at most one command ([`MAX_COMMAND_BYTES`]). Entries follow the
//!   message's index without a gap. Their terms run from the message's `log_term` up to its
//!   `term`, never decreasing, and are never 0.
//! - **Unknown fields:** refused at every depth, before anything is parsed.
//!   [`decode_message`] scans the wire format first: this build's field numbers and wire
//!   types only, no groups, every length within the bytes, and bounded repeated fields, on
//!   the message, its entries and a snapshot's nested messages alike. It then admits only
//!   the canonical bytes v1's encoder writes, so nothing the runtime would skip or store
//!   gets through (br-umjh, br-808c). A well-formed snapshot is refused by the snapshot
//!   rule above.
//!
//! Every rule above is stateless. One more needs the node's log ([`fits_log`], br-1jtp): a
//! heartbeat whose commit index is past this node's last index is refused, because raft-rs's
//! `commit_to` would `fatal!` on it. An honest leader never sends one: it sends
//! `min(matched, committed)`, and `matched` covers only entries this node synced before
//! acknowledging them (DUR-2); a heartbeat from an older term would be ignored anyway.
//! Two of raft-rs's other stateful `fatal!`s on peer input are kept out of reach by
//! stateless rules:
//! - **An append's previous entry** has index and term both 0, or both nonzero (br-0mio).
//!   raft-rs's `term()` answers 0 for any index past the log, so a `log_term` of 0 would
//!   "match" an index this node does not hold, and `maybe_append` would then drive
//!   `commit_to` or the unstable log past their bounds. A v1 leader sends
//!   `log_term = term(index)`, which is 0 only at index 0. With a matching nonzero previous
//!   entry, the append's commit is bounded by entries this node holds.
//! - **An appended entry conflicting with a committed one** cannot be reached:
//!   `handle_append_entries` answers at once when the message's index is below the commit
//!   index, and otherwise the contiguity rule puts every entry above it.
//!
//! A peer that forges otherwise plausible state is Byzantine, outside v1's fault model
//! (§2.2).

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
    /// A message term of 0, which only local messages carry.
    ZeroTerm,
    /// An append whose previous entry has index 0 with a nonzero term, or a nonzero index
    /// with term 0, which a v1 leader never sends.
    PrevEntry {
        /// The previous entry's index.
        index: u64,
        /// The previous entry's term.
        log_term: u64,
    },
    /// A heartbeat whose commit index is past this node's last index.
    CommitPastLog {
        /// The heartbeat's commit index.
        commit: u64,
        /// This node's last index.
        last: u64,
    },
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
    if message.term == 0 {
        return Err(Inadmissible::ZeroTerm);
    }
    if !carries_entries && !message.entries.is_empty() {
        return Err(Inadmissible::Entry { position: 0 });
    }
    if carries_entries && (message.index == 0) != (message.log_term == 0) {
        return Err(Inadmissible::PrevEntry {
            index: message.index,
            log_term: message.log_term,
        });
    }
    check_entries(&message)?;
    Ok(message)
}

/// Whether `message`, already admitted, fits this node's log, whose last index is `last`
/// (module documentation).
///
/// # Errors
///
/// [`Inadmissible::CommitPastLog`] for a heartbeat whose commit index is past `last`.
pub fn fits_log(message: &eraftpb::Message, last: u64) -> Result<(), Inadmissible> {
    if message.get_msg_type() == MessageType::MsgHeartbeat && message.commit > last {
        return Err(Inadmissible::CommitPastLog {
            commit: message.commit,
            last,
        });
    }
    Ok(())
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
