//! Conversions between the library's protobuf types and the core's port types. This module is
//! the only place the two meet, so no library type leaves the crate (CD-1).

use protobuf::Message as _;
use raft::eraftpb::{self, EntryType};
use types::{Entry, EntryIndex, EntryPayload, HardState, LogIndex, NodeId, OutboundMessage, Term};

/// A library entry as a core entry, or `None` if it is not one v1 can hold: index 0, a term
/// outside 1 to 2^53 − 1, or a configuration change. Membership is static, so the library never
/// makes one (§7.7 step 6).
pub fn entry_from_library(entry: &eraftpb::Entry) -> Option<Entry> {
    let normal = match entry.get_entry_type() {
        EntryType::EntryNormal => true,
        EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => false,
    };
    let payload = if entry.data.is_empty() {
        EntryPayload::Empty
    } else {
        EntryPayload::Command(entry.data.to_vec())
    };
    if !normal {
        return None;
    }
    Some(Entry {
        index: EntryIndex::new(entry.index).ok()?,
        term: Term::new(entry.term).ok()?,
        payload,
    })
}

/// A core entry as a library entry.
pub fn entry_to_library(entry: &Entry) -> eraftpb::Entry {
    let mut library = eraftpb::Entry::default();
    library.set_entry_type(EntryType::EntryNormal);
    library.index = entry.index.get();
    library.term = entry.term.get();
    library.data = match &entry.payload {
        EntryPayload::Empty => Vec::new(),
        EntryPayload::Command(bytes) => bytes.clone(),
    }
    .into();
    library
}

/// Library entries as core entries, or `None` if any is not one v1 can hold.
pub fn entries_from_library(entries: &[eraftpb::Entry]) -> Option<Vec<Entry>> {
    entries.iter().map(entry_from_library).collect()
}

/// A library hard state as a core one, or `None` if its term or commit index is out of range.
/// A vote of 0 means none.
pub fn hard_state_from_library(state: &eraftpb::HardState) -> Option<HardState> {
    Some(HardState {
        term: Term::new(state.term).ok()?,
        vote: NodeId::new(state.vote).ok(),
        commit: LogIndex::new(state.commit).ok()?,
    })
}

/// A core hard state as a library one.
pub fn hard_state_to_library(state: HardState) -> eraftpb::HardState {
    eraftpb::HardState {
        term: state.term.get(),
        vote: state.vote.map_or(0, NodeId::get),
        commit: state.commit.get(),
        ..eraftpb::HardState::default()
    }
}

/// A library message as an outbound core message: its destination and its canonical
/// protobuf bytes. `None` if it has no valid destination or cannot be encoded.
pub fn outbound(message: &eraftpb::Message) -> Option<OutboundMessage> {
    Some(OutboundMessage {
        to: NodeId::new(message.to).ok()?,
        bytes: message.write_to_bytes().ok()?,
    })
}

/// A peer message's bytes as a library message, or `None` if they do not decode, or carry
/// fields this build does not know on the message or its entries. The protobuf runtime would
/// keep an unknown field in a hash map and write it back on re-encoding, so the bytes would
/// depend on the sender's build; it is refused instead (W02 audit H-3). The snapshot, the only
/// other nested message, is refused whole by [`admit`](crate::admit::admit), which every peer
/// message passes before the library sees it (br-xx08).
pub fn decode_message(bytes: &[u8]) -> Option<eraftpb::Message> {
    let message = eraftpb::Message::parse_from_bytes(bytes).ok()?;
    let known = message.get_unknown_fields().iter().next().is_none()
        && message
            .get_entries()
            .iter()
            .all(|entry| entry.get_unknown_fields().iter().next().is_none());
    known.then_some(message)
}

#[cfg(test)]
mod tests {
    use raft::eraftpb::MessageType;

    use super::*;

    fn index(raw: u64) -> EntryIndex {
        EntryIndex::new(raw).unwrap()
    }

    #[test]
    fn entries_and_hard_states_round_trip() {
        for payload in [EntryPayload::Empty, EntryPayload::Command(vec![1, 2, 3])] {
            let entry = Entry {
                index: index(4),
                term: Term::new(2).unwrap(),
                payload,
            };
            assert_eq!(
                entry_from_library(&entry_to_library(&entry)),
                Some(entry.clone()),
                "rule: a core entry survives the library round trip (CD-1); entry={entry:?}"
            );
        }
        for vote in [None, NodeId::new(3).ok()] {
            let state = HardState {
                term: Term::new(5).unwrap(),
                vote,
                commit: LogIndex::new(9).unwrap(),
            };
            assert_eq!(
                hard_state_from_library(&hard_state_to_library(state)),
                Some(state),
                "rule: a hard state survives the round trip; vote 0 means none"
            );
        }
    }

    #[test]
    fn configuration_changes_and_index_zero_are_not_v1_entries() {
        let mut change = entry_to_library(&Entry {
            index: index(1),
            term: Term::new(1).unwrap(),
            payload: EntryPayload::Empty,
        });
        change.set_entry_type(EntryType::EntryConfChange);
        assert_eq!(
            entry_from_library(&change),
            None,
            "rule: membership is static in v1 (§7.7 step 6)"
        );
        assert_eq!(
            entry_from_library(&eraftpb::Entry::default()),
            None,
            "rule: index 0 holds no entry"
        );
    }

    #[test]
    fn messages_encode_canonically_and_unknown_fields_are_refused() {
        let mut message = eraftpb::Message::default();
        message.set_msg_type(MessageType::MsgHeartbeat);
        message.to = 2;
        message.from = 1;
        message.term = 3;
        let sent = outbound(&message).unwrap();
        assert_eq!(
            sent.to,
            NodeId::new(2).unwrap(),
            "rule: the destination is kept"
        );
        assert_eq!(
            decode_message(&sent.bytes),
            Some(message.clone()),
            "rule: a message decodes from its own bytes"
        );
        assert_eq!(
            decode_message(&[0xff, 0xff, 0xff]),
            None,
            "rule: bytes that are not a message are refused"
        );
        let mut extended = sent.bytes;
        // Field 1000, varint 1: a field this build does not know.
        extended.extend_from_slice(&[0xc0, 0x3e, 0x01]);
        assert_eq!(
            decode_message(&extended),
            None,
            "rule: unknown fields are refused, never stored (W02 audit H-3)"
        );
        message.to = 0;
        assert_eq!(outbound(&message), None, "rule: node 0 is no destination");
    }
}
