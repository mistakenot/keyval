//! W27.2's tests of compare-and-swap in apply (PLAN.md §7.5 step 4, F-5, DM-3, DM-5, DM-6,
//! DM-7): the expectation is checked when the entry applies; a match sets the key with the
//! execution index; a mismatch is terminal, names the current version or absence, changes
//! nothing and is recorded; mismatch is decided strictly before capacity; a matching CAS
//! over the quota is `capacity_exceeded`.
#![cfg(test)]

// check: APPLY-CAS

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ExpectedVersion, ItemCount, Key,
    KeyValueLimits, LiveStateLimit, LogIndex, Mutation, NodeId, RequestId, Term, TerminalResult,
    Value, Version,
};

use super::{Applied, StateMachine, Stored};
use crate::capacity::tests::{far, roomy};
use crate::{Command, StateLimits, codec};

fn id(request: u8) -> RequestId {
    RequestId::from_bytes([request; 16])
}

fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn version(index: u64) -> Version {
    Version::at(LogIndex::new(index).unwrap()).unwrap()
}

fn put(request: u8, k: &str, v: &str) -> Vec<u8> {
    codec::encode(&Command {
        request_id: id(request),
        mutation: Mutation::Put {
            key: key(k),
            value: value(v),
        },
    })
}

fn delete(request: u8, k: &str) -> Vec<u8> {
    codec::encode(&Command {
        request_id: id(request),
        mutation: Mutation::Delete { key: key(k) },
    })
}

fn cas(request: u8, k: &str, expected: ExpectedVersion, v: &str) -> Vec<u8> {
    codec::encode(&Command {
        request_id: id(request),
        mutation: Mutation::Cas {
            key: key(k),
            expected,
            value: value(v),
        },
    })
}

/// Applies `log` from index 1, each entry a command.
fn run(limits: StateLimits, log: &[Vec<u8>]) -> (StateMachine, Vec<Applied>) {
    let mut state = StateMachine::new(NodeId::new(1).unwrap(), limits);
    let applied = (1..)
        .zip(log)
        .map(|(index, command)| {
            state.apply(
                &Entry {
                    index: EntryIndex::new(index).unwrap(),
                    term: Term::new(1).unwrap(),
                    payload: EntryPayload::Command(command.clone()),
                },
                far(),
            )
        })
        .collect();
    (state, applied)
}

fn decision(applied: &Applied) -> Option<Decision> {
    match applied {
        Applied::Executed { result, .. } => Some(result.decision()),
        Applied::Empty | Applied::Replayed { .. } | Applied::Conflict { .. } => None,
    }
}

fn tight(keys: u64, bytes: u64) -> StateLimits {
    StateLimits {
        key_values: KeyValueLimits::MAXIMA,
        max_live_keys: ItemCount::new(keys),
        max_live_bytes: ByteCount::new(bytes),
    }
}

#[test]
fn an_absent_expectation_on_an_absent_key_sets_it_with_the_execution_index() {
    let (state, applied) = run(roomy(), &[cas(1, "k", ExpectedVersion::Absent, "v")]);
    assert_eq!(
        (
            decision(&applied[0]),
            state.get(&key("k")).map(Stored::version)
        ),
        (Some(Decision::Set), Some(version(1))),
        "rule: CAS expecting Absent on an absent key sets it with version = its execution \
         index (F-5, DM-3)"
    );
}

#[test]
fn a_matching_version_sets_the_key_with_the_execution_index() {
    let (state, applied) = run(
        roomy(),
        &[
            put(1, "k", "old"),
            cas(2, "k", ExpectedVersion::Version(version(1)), "new"),
        ],
    );
    assert_eq!(
        (
            decision(&applied[1]),
            state
                .get(&key("k"))
                .map(|stored| (stored.value().clone(), stored.version())),
        ),
        (Some(Decision::Set), Some((value("new"), version(2)))),
        "rule: CAS whose expected version matches sets the key with version = its execution \
         index (F-5, DM-3)"
    );
}

#[test]
fn a_mismatch_is_terminal_names_the_current_version_and_changes_nothing() {
    let (before, _) = run(roomy(), &[put(1, "k", "old")]);
    let wrong = cas(2, "k", ExpectedVersion::Version(version(9)), "new");
    let (state, applied) = run(roomy(), &[put(1, "k", "old"), wrong.clone()]);
    assert_eq!(
        (
            decision(&applied[1]),
            state
                .get(&key("k"))
                .map(|stored| (stored.value().clone(), stored.version())),
            state.live_bytes(),
            state.history().get(&id(2)).map(crate::Record::result),
            state.committed_command_bytes().get(),
        ),
        (
            Some(Decision::VersionMismatch {
                current: Some(version(1))
            }),
            Some((value("old"), version(1))),
            before.live_bytes(),
            Some(TerminalResult::executed_at(
                EntryIndex::new(2).unwrap(),
                Decision::VersionMismatch {
                    current: Some(version(1))
                },
            )),
            before.committed_command_bytes().get() + u64::try_from(wrong.len()).unwrap(),
        ),
        "rule: a mismatch is a terminal result naming the current version, changes no live \
         state, is recorded in the history and counts its command bytes (F-5, DM-5, DM-7)"
    );
}

#[test]
fn each_kind_of_mismatch_names_the_current_version_or_its_absence() {
    let (_, applied) = run(
        roomy(),
        &[
            put(1, "live", "v"),
            cas(2, "live", ExpectedVersion::Absent, "w"),
            cas(3, "absent", ExpectedVersion::Version(version(1)), "w"),
        ],
    );
    assert_eq!(
        (decision(&applied[1]), decision(&applied[2])),
        (
            Some(Decision::VersionMismatch {
                current: Some(version(1))
            }),
            Some(Decision::VersionMismatch { current: None }),
        ),
        "rule: expecting Absent on a live key mismatches with its version; expecting a \
         version on an absent key mismatches with no current version (F-5)"
    );
}

#[test]
fn mismatch_is_decided_strictly_before_capacity() {
    // One key, 8 bytes: "a" = "v" fills the key quota.
    let (state, applied) = run(
        tight(1, 8),
        &[
            put(1, "a", "v"),
            // A new key: over the key quota, and the expectation does not hold.
            cas(2, "b", ExpectedVersion::Version(version(5)), "v"),
            // An overwrite that would pass the byte quota, and the expectation does not hold.
            cas(
                3,
                "a",
                ExpectedVersion::Version(version(7)),
                "much too long",
            ),
        ],
    );
    assert_eq!(
        (
            decision(&applied[1]),
            decision(&applied[2]),
            state.live_keys(),
        ),
        (
            Some(Decision::VersionMismatch { current: None }),
            Some(Decision::VersionMismatch {
                current: Some(version(1))
            }),
            ItemCount::new(1),
        ),
        "rule: a CAS whose expectation fails is version_mismatch even when the write would \
         also exceed DM-6: mismatch is checked strictly before capacity (§7.5 step 4)"
    );
}

#[test]
fn a_matching_cas_over_the_quota_is_capacity_exceeded() {
    let (state, applied) = run(
        tight(1, 8),
        &[
            put(1, "a", "v"),
            cas(2, "b", ExpectedVersion::Absent, "v"),
            cas(
                3,
                "a",
                ExpectedVersion::Version(version(1)),
                "much too long",
            ),
        ],
    );
    assert_eq!(
        (
            decision(&applied[1]),
            decision(&applied[2]),
            state.get(&key("a")).map(Stored::version),
        ),
        (
            Some(Decision::CapacityExceeded {
                limit: LiveStateLimit::Keys
            }),
            Some(Decision::CapacityExceeded {
                limit: LiveStateLimit::Bytes
            }),
            Some(version(1)),
        ),
        "rule: a CAS whose expectation holds but whose write would exceed the key or byte \
         quota is capacity_exceeded, like a PUT, and changes nothing (DM-6)"
    );
}

#[test]
fn a_cas_result_is_recorded_once_and_replays_even_when_it_would_now_match() {
    let mismatch = cas(2, "k", ExpectedVersion::Version(version(3)), "w");
    let (_, applied) = run(
        roomy(),
        &[
            put(1, "k", "v"),
            mismatch.clone(),
            put(3, "k", "v"),
            mismatch,
            cas(2, "k", ExpectedVersion::Absent, "w"),
        ],
    );
    let original = TerminalResult::executed_at(
        EntryIndex::new(2).unwrap(),
        Decision::VersionMismatch {
            current: Some(version(1)),
        },
    );
    assert_eq!(
        (applied[3].clone(), applied[4].clone()),
        (
            Applied::Replayed {
                request_id: id(2),
                result: original,
            },
            Applied::Conflict {
                request_id: id(2),
                original_index: EntryIndex::new(2).unwrap(),
            },
        ),
        "rule: a CAS's terminal result is recorded once: a duplicate entry replays the \
         original mismatch although the key is now at version 3, and another command with its \
         ID is a conflict (DM-5, F-6)"
    );
}

#[test]
fn a_cas_after_delete_and_re_create_needs_the_new_version() {
    let (state, applied) = run(
        roomy(),
        &[
            put(1, "k", "first"),
            delete(2, "k"),
            put(3, "k", "second"),
            cas(4, "k", ExpectedVersion::Version(version(1)), "stale"),
            cas(5, "k", ExpectedVersion::Version(version(3)), "fresh"),
        ],
    );
    assert_eq!(
        (
            decision(&applied[3]),
            decision(&applied[4]),
            state
                .get(&key("k"))
                .map(|stored| (stored.value().clone(), stored.version())),
        ),
        (
            Some(Decision::VersionMismatch {
                current: Some(version(3))
            }),
            Some(Decision::Set),
            Some((value("fresh"), version(5))),
        ),
        "rule: versions grow across delete and re-create, so a CAS naming the deleted key's \
         old version mismatches and only the new one matches (DM-3, F-5)"
    );
}
