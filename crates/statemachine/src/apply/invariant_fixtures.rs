//! Corrupted-state fixtures (PLAN.md §12.2 W09, CD-7). Each one breaks exactly one
//! invariant the state machine asserts, then makes it fire. They prove the invariants are
//! armed, not decorative. Test-only: they reach into private state.
//!
//! `SM-APPLY-IN-ORDER` and `SM-COMMAND-DECODES` fire in the APPLY-PUT tests.
//! `SM-CODEC-FIELD-LEN` cannot be reached, because no key or value comes near a `u32`
//! length.
#![cfg(test)]

// check: STATE-INVARIANTS

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits, LogIndex,
    Mutation, NodeId, RequestId, Term, TerminalResult, Value, Version,
};

use super::{StateMachine, Stored};
use crate::capacity::tests::{far, roomy};
use crate::history::{CommandDigest, Record};
use crate::{Command, StateLimits, codec};

fn machine(limits: StateLimits) -> StateMachine {
    StateMachine::new(NodeId::new(1).unwrap(), limits)
}

fn index(raw: u64) -> EntryIndex {
    EntryIndex::new(raw).unwrap()
}

fn entry(raw: u64, payload: EntryPayload) -> Entry {
    Entry {
        index: index(raw),
        term: Term::new(1).unwrap(),
        payload,
    }
}

fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn command(request: u8, k: &str, v: &str) -> Command {
    Command {
        request_id: RequestId::from_bytes([request; 16]),
        mutation: Mutation::Put {
            key: key(k),
            value: value(v),
        },
    }
}

fn put(request: u8, k: &str, v: &str) -> EntryPayload {
    EntryPayload::Command(codec::encode(&command(request, k, v)))
}

#[test]
#[should_panic(
    expected = "invariant SM-APPLIED-LE-COMMIT violated: node 1: entry 1 applied \
                           beyond commit index 0"
)]
fn an_entry_beyond_the_commit_index_fires() {
    machine(roomy()).apply(&entry(1, EntryPayload::Empty), LogIndex::ZERO);
}

#[test]
#[should_panic(
    expected = "invariant SM-VERSION-INCREASES violated: node 1: entry 2 would set \
                           version 2 on a key already at version Some(5)"
)]
fn a_version_that_would_not_grow_fires() {
    let mut state = machine(roomy());
    state.apply(&entry(1, put(1, "k", "v")), far());
    // Corrupt: the stored version is ahead of the log.
    state.map.get_mut(&key("k")).unwrap().version = Version::new(5).unwrap();
    state.apply(&entry(2, put(2, "k", "w")), far());
}

#[test]
#[should_panic(
    expected = "invariant SM-LIVE-WITHIN-QUOTA violated: node 1: after entry 2: 2 \
                           live keys"
)]
fn live_keys_over_the_quota_fire() {
    let limits = StateLimits {
        max_live_keys: ItemCount::new(1),
        ..roomy()
    };
    let mut state = machine(limits);
    state.apply(&entry(1, put(1, "a", "x")), far());
    // Corrupt: a key past the quota.
    state.map.insert(
        key("b"),
        Stored {
            value: value("y"),
            version: Version::new(1).unwrap(),
        },
    );
    state.apply(&entry(2, EntryPayload::Empty), far());
}

#[test]
#[should_panic(
    expected = "invariant SM-LIVE-WITHIN-QUOTA violated: node 1: after entry 2: 1 \
                           live keys and 6 live bytes exceed the quota of 10000 keys and 2 bytes"
)]
fn live_bytes_over_the_quota_fire() {
    let mut state = machine(roomy());
    state.apply(&entry(1, put(1, "k", "12345")), far());
    // Corrupt: a quota below the live bytes.
    state.limits.max_live_bytes = ByteCount::new(2);
    state.apply(&entry(2, EntryPayload::Empty), far());
}

#[test]
#[should_panic(
    expected = "invariant SM-LIVE-BYTES violated: node 1: entry 2: live bytes 0 are \
                           fewer than the overwritten entry's Some(2)"
)]
fn a_live_bytes_counter_below_a_live_entry_fires() {
    let mut state = machine(roomy());
    state.apply(&entry(1, put(1, "k", "v")), far());
    // Corrupt: the counter lost the live entry's bytes.
    state.live_bytes = ByteCount::ZERO;
    state.apply(&entry(2, put(2, "k", "w")), far());
}

#[test]
#[should_panic(expected = "invariant SM-COMMAND-BYTES violated: node 1: entry 1: adding")]
fn a_committed_command_bytes_overflow_fires() {
    let mut state = machine(roomy());
    // Corrupt: the DM-7 counter at its ceiling.
    state.command_bytes = ByteCount::new(u64::MAX);
    state.apply(&entry(1, put(1, "k", "v")), far());
}

#[test]
#[should_panic(
    expected = "invariant SM-HISTORY-BOUNDED violated: node 1: after entry 1: 2 \
                           history records exceed the 1 entries applied"
)]
fn more_history_records_than_entries_fire() {
    let mut state = machine(roomy());
    let ghost = command(9, "ghost", "x");
    // Corrupt: a record no entry produced.
    state.history.insert(
        ghost.request_id,
        Record::new(
            index(1),
            CommandDigest::of(&ghost),
            TerminalResult::executed_at(index(1), Decision::Set),
        ),
    );
    state.apply(&entry(1, put(1, "k", "v")), far());
}

#[test]
#[should_panic(expected = "invariant SM-HISTORY-ONCE violated: node 1: entry 2 executed request")]
fn a_second_first_execution_of_a_request_fires() {
    let mut state = machine(roomy());
    let first = command(1, "k", "v");
    state.apply(
        &entry(1, EntryPayload::Command(codec::encode(&first))),
        far(),
    );
    // Corrupt the routing: execute the recorded request again as if it were new.
    state.first_execution(
        index(2),
        CommandDigest::of(&first),
        ByteCount::new(1),
        first,
    );
}

#[test]
#[should_panic(
    expected = "invariant SM-LIVE-BYTES violated: node 1: entry 2: live bytes 1 \
                           are fewer than the deleted entry's 3"
)]
fn a_live_bytes_counter_below_a_deleted_entry_fires() {
    let mut state = machine(roomy());
    state.apply(&entry(1, put(1, "k", "vv")), far());
    state.live_bytes = ByteCount::new(1);
    let delete = codec::encode(&Command {
        request_id: RequestId::from_bytes([2; 16]),
        mutation: Mutation::Delete { key: key("k") },
    });
    state.apply(&entry(2, EntryPayload::Command(delete)), far());
}
