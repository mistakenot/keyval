//! W27.1's tests of DELETE in apply (PLAN.md §7.5, F-3, DM-3, DM-5, DM-6, DM-7): it removes
//! a live key and reports whether it existed, succeeds on an absent key, assigns no version,
//! frees the quota, is recorded once in the history and replays its original result, and
//! counts its command bytes toward DM-7.
#![cfg(test)]

// check: APPLY-DELETE

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits,
    LiveStateLimit, LogIndex, Mutation, NodeId, RequestId, Term, TerminalResult, Value, Version,
};

use super::{Applied, StateMachine, Stored};
use crate::capacity::tests::{far, roomy};
use crate::history::CommandDigest;
use crate::{Command, StateLimits, codec};

fn machine(limits: StateLimits) -> StateMachine {
    StateMachine::new(NodeId::new(1).unwrap(), limits)
}

fn id(request: u8) -> RequestId {
    RequestId::from_bytes([request; 16])
}

fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
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

/// Applies `log` from index 1, each a command (`Some`) or an empty entry (`None`).
fn run(limits: StateLimits, log: &[Option<Vec<u8>>]) -> (StateMachine, Vec<Applied>) {
    let mut state = machine(limits);
    let applied = (1..)
        .zip(log)
        .map(|(index, command)| {
            state.apply(
                &Entry {
                    index: EntryIndex::new(index).unwrap(),
                    term: Term::new(1).unwrap(),
                    payload: command
                        .clone()
                        .map_or(EntryPayload::Empty, EntryPayload::Command),
                },
                far(),
            )
        })
        .collect();
    (state, applied)
}

fn deleted(request: u8, index: u64, existed: bool) -> Applied {
    Applied::Executed {
        request_id: id(request),
        result: TerminalResult::executed_at(
            EntryIndex::new(index).unwrap(),
            Decision::Deleted { existed },
        ),
    }
}

#[test]
fn deleting_a_live_key_removes_it_and_reports_that_it_existed() {
    let (state, applied) = run(roomy(), &[Some(put(1, "k", "value")), Some(delete(2, "k"))]);
    assert_eq!(
        (
            applied[1].clone(),
            state.get(&key("k")).is_some(),
            state.live_keys(),
            state.live_bytes(),
            state.applied_index().get(),
        ),
        (
            deleted(2, 2, true),
            false,
            ItemCount::new(0),
            ByteCount::ZERO,
            2
        ),
        "rule: DELETE removes a live key and returns existed = true; the live key and byte \
         counters shrink by the removed entry (F-3, §7.5 step 4)"
    );
}

#[test]
fn deleting_an_absent_key_succeeds_with_existed_false_and_changes_no_live_state() {
    let (state, applied) = run(
        roomy(),
        &[Some(put(1, "a", "v")), Some(delete(2, "absent"))],
    );
    assert_eq!(
        (
            applied[1].clone(),
            state.live_keys(),
            state.live_bytes(),
            state.get(&key("a")).map(Stored::version),
        ),
        (
            deleted(2, 2, false),
            ItemCount::new(1),
            ByteCount::new(2),
            Version::at(LogIndex::new(1).unwrap()),
        ),
        "rule: deleting an absent key succeeds with existed = false, and the live state is \
         unchanged (F-3)"
    );
}

#[test]
fn a_delete_assigns_no_version_and_a_re_create_gets_a_larger_one() {
    let (state, applied) = run(
        roomy(),
        &[
            Some(put(1, "k", "first")),
            Some(delete(2, "k")),
            None,
            Some(put(3, "k", "again")),
        ],
    );
    let result = match &applied[1] {
        Applied::Executed { result, .. } => Some(*result),
        Applied::Empty | Applied::Replayed { .. } | Applied::Conflict { .. } => None,
    };
    assert_eq!(
        (
            result.and_then(TerminalResult::version),
            state.get(&key("k")).map(Stored::version),
        ),
        (None, Version::at(LogIndex::new(4).unwrap())),
        "rule: a DELETE assigns no version, and a re-created key gets its re-creating \
         execution's index, larger than any version it had before (DM-3)"
    );
}

#[test]
fn a_delete_frees_the_quota_for_another_key_and_is_never_refused() {
    let one_key = StateLimits {
        key_values: KeyValueLimits::MAXIMA,
        max_live_keys: ItemCount::new(1),
        max_live_bytes: ByteCount::new(64),
    };
    let (state, applied) = run(
        one_key,
        &[
            Some(put(1, "a", "v")),
            Some(put(2, "b", "v")),
            Some(delete(3, "a")),
            Some(put(4, "b", "v")),
        ],
    );
    let decision = |at: usize| match &applied[at] {
        Applied::Executed { result, .. } => Some(result.decision()),
        Applied::Empty | Applied::Replayed { .. } | Applied::Conflict { .. } => None,
    };
    assert_eq!(
        (decision(1), decision(2), decision(3), state.live_keys()),
        (
            Some(Decision::CapacityExceeded {
                limit: LiveStateLimit::Keys
            }),
            Some(Decision::Deleted { existed: true }),
            Some(Decision::Set),
            ItemCount::new(1),
        ),
        "rule: at the key quota a new key is refused; a DELETE always succeeds and frees \
         room for one (DM-6)"
    );
}

#[test]
fn a_delete_is_recorded_once_and_replays_its_original_result() {
    let (state, applied) = run(
        roomy(),
        &[
            Some(put(1, "k", "v")),
            Some(delete(2, "k")),
            Some(delete(2, "k")),
            Some(delete(2, "other")),
        ],
    );
    assert_eq!(
        (applied[2].clone(), applied[3].clone()),
        (
            Applied::Replayed {
                request_id: id(2),
                result: TerminalResult::executed_at(
                    EntryIndex::new(2).unwrap(),
                    Decision::Deleted { existed: true },
                ),
            },
            Applied::Conflict {
                request_id: id(2),
                original_index: EntryIndex::new(2).unwrap(),
            },
        ),
        "rule: a DELETE's result is recorded in the history (DM-5): a duplicate entry \
         replays existed = true although the key is now absent, and another command with \
         its ID is a conflict"
    );
    let record = state.history().get(&id(2)).copied();
    assert_eq!(
        record.map(|record| (record.first_index(), record.digest())),
        Some((
            EntryIndex::new(2).unwrap(),
            CommandDigest::of_canonical(&delete(2, "k"))
        )),
        "rule: the history records the first execution's index and digest"
    );
}

#[test]
fn a_delete_counts_its_command_bytes_toward_dm7_once() {
    let first = delete(2, "absent");
    let len = u64::try_from(first.len()).unwrap();
    let (state, _) = run(roomy(), &[Some(first.clone()), Some(first)]);
    assert_eq!(
        state.committed_command_bytes(),
        ByteCount::new(len),
        "rule: a DELETE's first execution adds its canonical length to the DM-7 counter, \
         whatever its result; the replay adds nothing (§7.5)"
    );
}

#[test]
fn replaying_a_log_with_deletes_rebuilds_the_same_state_and_results() {
    let log = [
        Some(put(1, "a", "1")),
        Some(put(2, "b", "22")),
        Some(delete(3, "a")),
        None,
        Some(delete(4, "a")),
        Some(put(5, "a", "333")),
        Some(delete(3, "a")),
    ];
    let (once, first) = run(roomy(), &log);
    let (again, second) = run(roomy(), &log);
    assert_eq!(
        (once.digest(), first),
        (again.digest(), second),
        "rule: apply is deterministic, deletes included: replaying the log rebuilds the same \
         state and results (D-10)"
    );
}
