//! The request history (PLAN.md §7.5, DM-5; leaf area `statemachine/history`).
//!
//! Every request ID's first execution is recorded, whatever its terminal result, and kept
//! for the life of the cluster: never expired, never evicted (DM-5, C-6). Apply step 3
//! consults it before any execution. The same command again is a replay of the stored
//! result. A different command under the same ID is a conflict. Neither changes state.

use std::collections::BTreeMap;

use types::{EntryIndex, RequestId, Sha256, TerminalResult};

use crate::codec::{self, Command};

/// SHA-256 over a command's canonical bytes (§7.5 "Digest").
///
/// Two commands with one request ID count as the same command exactly when their digests
/// are equal. That rests on SHA-256's collision resistance, a stated Assumption (§10.15):
/// comparing the full commands would need disk reads inside apply (R-17).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommandDigest([u8; 32]);

impl CommandDigest {
    /// The digest of `command`: SHA-256 over its canonical encoding.
    #[must_use]
    pub fn of(command: &Command) -> Self {
        Self::of_canonical(&codec::encode(command))
    }

    /// The digest of a command's canonical bytes, as a log entry carries them.
    #[must_use]
    pub fn of_canonical(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes))
    }

    /// The 32 digest bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// What the history keeps for one request ID, forever (DM-5).
///
/// Invariant: `result` was decided at `first_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    first_index: EntryIndex,
    digest: CommandDigest,
    result: TerminalResult,
}

impl Record {
    /// The record of a command first executed at `first_index` with `result`.
    pub(crate) const fn new(
        first_index: EntryIndex,
        digest: CommandDigest,
        result: TerminalResult,
    ) -> Self {
        Self {
            first_index,
            digest,
            result,
        }
    }

    /// The index of the request's first execution.
    #[must_use]
    pub const fn first_index(&self) -> EntryIndex {
        self.first_index
    }

    /// The digest of the command that executed.
    #[must_use]
    pub const fn digest(&self) -> CommandDigest {
        self.digest
    }

    /// The terminal result, returned unchanged to every retry (§8.1).
    #[must_use]
    pub const fn result(&self) -> TerminalResult {
        self.result
    }
}

/// The request history: each request ID's first execution (DM-5).
///
/// Invariant: a request ID is recorded at most once, and its record never changes (C-6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    records: BTreeMap<RequestId, Record>,
}

impl History {
    /// The record for `request_id`, if it ever executed.
    #[must_use]
    pub fn get(&self, request_id: &RequestId) -> Option<&Record> {
        self.records.get(request_id)
    }

    /// The number of request IDs recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether nothing has executed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Records `request_id`'s first execution, returning any record it already had; the
    /// caller asserts there was none.
    pub(crate) fn insert(&mut self, request_id: RequestId, record: Record) -> Option<Record> {
        self.records.insert(request_id, record)
    }

    /// An empty history.
    pub(crate) const fn new() -> Self {
        Self {
            records: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    // check: REQUEST-HISTORY

    use types::{
        Decision, Entry, EntryPayload, Key, KeyValueLimits, Mutation, NodeId, Term, Value,
    };

    use super::*;
    use crate::capacity::tests::far;
    use crate::{Applied, StateMachine};

    fn entry(index: u64, payload: EntryPayload) -> Entry {
        Entry {
            index: EntryIndex::new(index).unwrap(),
            term: Term::new(1).unwrap(),
            payload,
        }
    }

    fn put(request: u8, key: &str, value: &str) -> Command {
        Command {
            request_id: RequestId::from_bytes([request; 16]),
            mutation: Mutation::Put {
                key: Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
                value: Value::parse(value.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            },
        }
    }

    fn payload(command: &Command) -> EntryPayload {
        EntryPayload::Command(codec::encode(command))
    }

    fn machine() -> StateMachine {
        StateMachine::new(NodeId::new(1).unwrap(), crate::capacity::tests::roomy())
    }

    fn set_at(index: u64) -> TerminalResult {
        TerminalResult::executed_at(EntryIndex::new(index).unwrap(), Decision::Set)
    }

    #[test]
    fn the_digest_is_sha256_over_the_canonical_bytes() {
        let abc = CommandDigest::of_canonical(b"abc");
        assert_eq!(
            abc.bytes()[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "rule: the digest is SHA-256 (FIPS 180-4 'abc' vector begins ba7816bf); \
             digest={abc:?}"
        );
        let command = put(1, "k", "v");
        assert_eq!(
            CommandDigest::of(&command),
            CommandDigest::of_canonical(&codec::encode(&command)),
            "rule: a command's digest is over its canonical encoding (§7.5)"
        );
        assert_ne!(
            CommandDigest::of(&command),
            CommandDigest::of(&put(1, "k", "w")),
            "rule: different commands under one ID have different digests"
        );
    }

    #[test]
    fn a_first_execution_is_recorded_with_index_digest_and_result() {
        let mut state = machine();
        state.apply(&entry(1, EntryPayload::Empty), far());
        let command = put(7, "k", "v");
        state.apply(&entry(2, payload(&command)), far());
        assert_eq!(
            state.history().get(&command.request_id),
            Some(&Record::new(
                EntryIndex::new(2).unwrap(),
                CommandDigest::of(&command),
                set_at(2),
            )),
            "rule: the history holds request_id -> (first_index, digest, result) (DM-5)"
        );
        assert_eq!(state.history().len(), 1, "rule: one record per request ID");
    }

    #[test]
    fn a_replay_returns_the_original_result_and_changes_nothing() {
        let mut state = machine();
        let command = put(7, "k", "v");
        state.apply(&entry(1, payload(&command)), far());
        state.apply(&entry(2, payload(&put(8, "other", "x"))), far());
        let before = (
            state.get(command.mutation.key()).cloned(),
            state.history().clone(),
        );
        assert_eq!(
            state.apply(&entry(3, payload(&command)), far()),
            Applied::Replayed {
                request_id: command.request_id,
                result: set_at(1),
            },
            "rule: the same command again returns the stored result (§7.5 step 3)"
        );
        assert_eq!(
            (
                state.get(command.mutation.key()).cloned(),
                state.history().clone()
            ),
            before,
            "rule: a replay changes no key, version or record (DM-3, DM-5)"
        );
        assert_eq!(
            state.applied_index().get(),
            3,
            "rule: the applied index still advances (§7.5 step 5)"
        );
    }

    #[test]
    fn a_conflicting_command_changes_nothing_and_names_the_original() {
        let mut state = machine();
        let original = put(7, "k", "v");
        state.apply(&entry(1, payload(&original)), far());
        let different = put(7, "other", "w");
        let before = state.clone();
        assert_eq!(
            state.apply(&entry(2, payload(&different)), far()),
            Applied::Conflict {
                request_id: original.request_id,
                original_index: EntryIndex::new(1).unwrap(),
            },
            "rule: another command under a used ID is a conflict naming the first \
             execution (§7.5 step 3, F-6)"
        );
        assert_eq!(
            state.get(different.mutation.key()),
            None,
            "rule: the conflicting command never executes (C-6)"
        );
        assert!(
            state.iter().eq(before.iter()) && state.history() == before.history(),
            "rule: the original result stays authoritative; no key or record changes"
        );
    }

    #[test]
    fn replaying_a_log_with_retries_and_conflicts_rebuilds_the_same_results() {
        let log = || {
            [
                payload(&put(1, "a", "x")),
                payload(&put(2, "b", "y")),
                payload(&put(1, "a", "x")),
                EntryPayload::Empty,
                payload(&put(2, "b", "z")),
                payload(&put(3, "a", "w")),
                payload(&put(1, "a", "x")),
            ]
        };
        let run = || {
            let mut state = machine();
            let results: Vec<Applied> = (1..)
                .zip(log())
                .map(|(index, payload)| state.apply(&entry(index, payload), far()))
                .collect();
            (state, results)
        };
        let (state, results) = run();
        assert_eq!(
            (state.clone(), results.clone()),
            run(),
            "rule: replay recomputes every result, replays and conflicts included (D-10)"
        );
        assert_eq!(
            results[6],
            Applied::Replayed {
                request_id: RequestId::from_bytes([1; 16]),
                result: set_at(1),
            },
            "rule: a retry long after the first execution still returns its result (DM-5)"
        );
        assert_eq!(
            state
                .get(&Key::parse("a".to_owned(), KeyValueLimits::MAXIMA).unwrap())
                .map(crate::Stored::version)
                .map(types::Version::get),
            Some(6),
            "rule: only first executions change keys; a's last set was entry 6"
        );
    }
}
