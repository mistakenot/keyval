//! The ordered map and the pure apply function (PLAN.md §7.5; leaf area
//! `statemachine/apply`).

use std::collections::BTreeMap;

use types::{
    Decision, Entry, EntryIndex, EntryPayload, Key, KeyValueLimits, LogIndex, Mutation, NodeId,
    RequestId, TerminalResult, Value, Version, invariant,
};

use crate::codec::{self, Command};
use crate::history::{CommandDigest, History, Record};

/// A key's stored value and the version that set it (§7.5, DM-3).
///
/// Invariant: `version` is the index of the entry whose execution stored `value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    value: Value,
    version: Version,
}

impl Stored {
    /// The value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    /// The version: the index of the write that set the value (DM-3).
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }
}

/// What applying one committed entry did (§7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// An empty entry, such as a new leader's no-op: only the applied index moved (step 1).
    Empty,
    /// A command's first execution, with the terminal result its waiters get (step 4).
    Executed {
        /// The command's request ID.
        request_id: RequestId,
        /// What it decided, at its index.
        result: TerminalResult,
    },
    /// The request ID already executed this same command: its stored result, returned
    /// unchanged; nothing changed (step 3, DM-5).
    Replayed {
        /// The command's request ID.
        request_id: RequestId,
        /// The result recorded at the first execution.
        result: TerminalResult,
    },
    /// The request ID already executed a different command: nothing changed, and the
    /// original result stays authoritative. Waiters for this entry get
    /// `request_id_conflict` (step 3, F-6).
    Conflict {
        /// The command's request ID.
        request_id: RequestId,
        /// The index of the request's first execution.
        original_index: EntryIndex,
    },
}

/// The replicated state machine (§7.5): the ordered map `key → (value, version)`, the
/// request history and the applied index, changed only by [`StateMachine::apply`].
///
/// Invariants: entries are applied once each, in index order, from index 1, so the
/// applied index is the last one's; every stored version is at most the applied index;
/// a request ID is recorded once, at its first execution, and never changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateMachine {
    /// The node this state belongs to, named in invariant messages (CD-7).
    node: NodeId,
    /// The key and value limits commands are decoded under (D-80).
    limits: KeyValueLimits,
    /// The live keys, in byte-wise key order (DM-4).
    map: BTreeMap<Key, Stored>,
    /// Every request ID's first execution, kept forever (DM-5).
    history: History,
    /// The index of the last applied entry; 0 before the first.
    applied: LogIndex,
}

impl StateMachine {
    /// The state before entry 1 is applied. Commands are decoded under `limits`, the
    /// configured key and value limits (D-80).
    #[must_use]
    pub const fn new(node: NodeId, limits: KeyValueLimits) -> Self {
        Self {
            node,
            limits,
            map: BTreeMap::new(),
            history: History::new(),
            applied: LogIndex::ZERO,
        }
    }

    /// The index of the last applied entry, or 0 before any (§7.5 step 5).
    #[must_use]
    pub const fn applied_index(&self) -> LogIndex {
        self.applied
    }

    /// The key's value and version, if the key is live (F-2).
    #[must_use]
    pub fn get(&self, key: &Key) -> Option<&Stored> {
        self.map.get(key)
    }

    /// The live keys with their values and versions, in byte-wise key order (DM-4).
    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Stored)> {
        self.map.iter()
    }

    /// The request history (DM-5): what each request ID's first execution decided.
    #[must_use]
    pub const fn history(&self) -> &History {
        &self.history
    }

    /// The number of live keys.
    #[must_use]
    pub fn live_keys(&self) -> usize {
        self.map.len()
    }

    /// Applies the committed entry that follows the applied index, and says what it did
    /// (§7.5). This is the only place writes change state. It is a pure, deterministic
    /// function of the state and the entry, so replaying the log rebuilds the same state
    /// and the same results (D-10).
    ///
    /// # Panics
    ///
    /// On a broken invariant (CD-7), naming the node and the entry:
    /// - `SM-APPLY-IN-ORDER`: `entry` is not the one after the applied index;
    /// - `SM-COMMAND-DECODES`: a committed command does not decode, although the leader
    ///   validated it before proposing it (step 2);
    /// - `SM-COMMAND-ADMITTED`: a committed DELETE or compare-and-swap, which no node
    ///   admits before W27 implements their apply;
    /// - `SM-HISTORY-ONCE`: a first execution of a request ID the history already holds.
    pub fn apply(&mut self, entry: &Entry) -> Applied {
        let index = entry.index;
        invariant!(
            self.applied.checked_next() == Some(index.index()),
            "SM-APPLY-IN-ORDER",
            "node {}: entry {index} applied after applied index {}; committed entries \
             apply once each, in index order",
            self.node,
            self.applied
        );
        let applied = match &entry.payload {
            EntryPayload::Empty => Applied::Empty,
            EntryPayload::Command(bytes) => self.execute(index, bytes),
        };
        self.applied = index.index();
        applied
    }

    /// Steps 2 to 4: decodes a committed command, then replays it, refuses it as a
    /// conflict, or executes it for the first time.
    fn execute(&mut self, index: EntryIndex, bytes: &[u8]) -> Applied {
        let command = codec::decode(bytes, self.limits);
        invariant!(
            command.is_ok(),
            "SM-COMMAND-DECODES",
            "node {}: committed entry {index} ({} bytes) does not decode: {:?}",
            self.node,
            bytes.len(),
            command.as_ref().err()
        );
        // The fallback is unreachable once the invariant above holds.
        command.map_or(Applied::Empty, |command| {
            // The entry's bytes are canonical (decode accepts nothing else), so this is the
            // command's digest.
            let digest = CommandDigest::of_canonical(bytes);
            let seen = self.history.get(&command.request_id).copied();
            match seen {
                Some(record) if record.digest() == digest => Applied::Replayed {
                    request_id: command.request_id,
                    result: record.result(),
                },
                Some(record) => Applied::Conflict {
                    request_id: command.request_id,
                    original_index: record.first_index(),
                },
                None => self.first_execution(index, digest, command),
            }
        })
    }

    /// Step 4: a command's first execution, which sets a PUT's key with
    /// `version = index` and records the result in the history, whatever it is. The
    /// live-state quota (W09.3) refuses over-quota writes.
    fn first_execution(
        &mut self,
        index: EntryIndex,
        digest: CommandDigest,
        command: Command,
    ) -> Applied {
        let Command {
            request_id,
            mutation,
        } = command;
        let put = match mutation {
            Mutation::Put { key, value } => Some((key, value)),
            Mutation::Delete { .. } | Mutation::Cas { .. } => None,
        };
        invariant!(
            put.is_some(),
            "SM-COMMAND-ADMITTED",
            "node {}: committed entry {index} carries a DELETE or compare-and-swap, which \
             no node admits before W27 implements their apply",
            self.node
        );
        // The fallback is unreachable once the invariant above holds.
        put.map_or(Applied::Empty, |(key, value)| {
            let version = index.version();
            self.map.insert(key, Stored { value, version });
            let result = TerminalResult::executed_at(index, Decision::Set);
            let earlier = self
                .history
                .insert(request_id, Record::new(index, digest, result));
            invariant!(
                earlier.is_none(),
                "SM-HISTORY-ONCE",
                "node {}: entry {index} executed request {request_id} again; its first \
                 execution was {earlier:?}",
                self.node
            );
            Applied::Executed { request_id, result }
        })
    }
}

#[cfg(test)]
mod tests {
    // check: APPLY-PUT

    use types::{ExpectedVersion, Term};

    use super::*;

    fn machine() -> StateMachine {
        StateMachine::new(NodeId::new(1).unwrap(), KeyValueLimits::MAXIMA)
    }

    fn entry(index: u64, payload: EntryPayload) -> Entry {
        Entry {
            index: EntryIndex::new(index).unwrap(),
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

    fn id(n: u8) -> RequestId {
        RequestId::from_bytes([n; 16])
    }

    fn put(request: u8, k: &str, v: &str) -> EntryPayload {
        EntryPayload::Command(codec::encode(&Command {
            request_id: id(request),
            mutation: Mutation::Put {
                key: key(k),
                value: value(v),
            },
        }))
    }

    fn version(raw: u64) -> Version {
        Version::new(raw).unwrap()
    }

    fn set_at(index: u64) -> TerminalResult {
        TerminalResult::executed_at(EntryIndex::new(index).unwrap(), Decision::Set)
    }

    #[test]
    fn an_empty_entry_only_advances_the_applied_index() {
        let mut state = machine();
        assert_eq!(
            state.applied_index(),
            LogIndex::ZERO,
            "rule: nothing applied yet"
        );
        assert_eq!(
            state.apply(&entry(1, EntryPayload::Empty)),
            Applied::Empty,
            "rule: an empty entry carries no command (§7.5 step 1)"
        );
        assert_eq!(
            state.applied_index(),
            LogIndex::new(1).unwrap(),
            "rule: the applied index advances (§7.5 step 5)"
        );
        assert_eq!(state.live_keys(), 0, "rule: an empty entry changes no key");
    }

    #[test]
    fn a_put_sets_its_key_with_the_execution_index_as_version() {
        let mut state = machine();
        state.apply(&entry(1, EntryPayload::Empty));
        let applied = state.apply(&entry(2, put(7, "k", "v")));
        assert_eq!(
            applied,
            Applied::Executed {
                request_id: id(7),
                result: set_at(2),
            },
            "rule: a PUT's first execution decides `set` at its index (§7.5 step 4)"
        );
        let stored = state.get(&key("k")).unwrap();
        assert_eq!(
            (stored.value(), stored.version()),
            (&value("v"), version(2)),
            "rule: the key holds the value, with version == execution index (DM-3)"
        );
        assert_eq!(
            state.applied_index(),
            LogIndex::new(2).unwrap(),
            "rule: applied_index = index (§7.5 step 5)"
        );
    }

    #[test]
    fn versions_are_execution_indices_new_on_every_put_and_with_gaps() {
        let mut state = machine();
        let log = [
            put(1, "a", "x"),
            EntryPayload::Empty,
            put(2, "b", "y"),
            put(3, "a", "x"),
            EntryPayload::Empty,
            put(4, "a", "z"),
        ];
        for (index, payload) in (1..).zip(log) {
            state.apply(&entry(index, payload));
        }
        assert_eq!(
            state.get(&key("a")).map(Stored::version),
            Some(version(6)),
            "rule: the latest PUT's index is the version, gaps included (DM-3)"
        );
        assert_eq!(
            state.get(&key("b")).map(Stored::version),
            Some(version(3)),
            "rule: versions are per key, from the write that set it (DM-3)"
        );

        let mut same = machine();
        same.apply(&entry(1, put(1, "k", "same")));
        same.apply(&entry(2, put(2, "k", "same")));
        assert_eq!(
            same.get(&key("k")).map(Stored::version),
            Some(version(2)),
            "rule: a same-value PUT still gets a new version (DM-3)"
        );
    }

    #[test]
    fn live_keys_iterate_in_byte_order() {
        let mut state = machine();
        let keys = ["b", "\u{e9}", "a", "B", "z", "a\u{0}"];
        for (index, (request, k)) in (1..).zip((1..).zip(keys)) {
            state.apply(&entry(index, put(request, k, "v")));
        }
        let order: Vec<&str> = state.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            order,
            ["B", "a", "a\u{0}", "b", "z", "\u{e9}"],
            "rule: keys are ordered by their UTF-8 bytes (DM-4)"
        );
        assert_eq!(
            state.live_keys(),
            keys.len(),
            "rule: one live key per PUT key"
        );
    }

    #[test]
    fn replaying_the_log_rebuilds_the_same_state_and_results() {
        let log = || {
            [
                EntryPayload::Empty,
                put(1, "k", "one"),
                put(2, "j", ""),
                EntryPayload::Empty,
                put(3, "k", "two"),
            ]
        };
        let run = || {
            let mut state = machine();
            let results: Vec<Applied> = (1..)
                .zip(log())
                .map(|(index, payload)| state.apply(&entry(index, payload)))
                .collect();
            (state, results)
        };
        assert_eq!(
            run(),
            run(),
            "rule: apply is a pure function of state and entry, so replay is exact (D-10)"
        );
    }

    #[test]
    #[should_panic(
        expected = "invariant SM-APPLY-IN-ORDER violated: node 1: entry 3 applied \
                               after applied index 1"
    )]
    fn skipping_an_entry_breaks_the_apply_order_invariant() {
        let mut state = machine();
        state.apply(&entry(1, EntryPayload::Empty));
        state.apply(&entry(3, EntryPayload::Empty));
    }

    #[test]
    #[should_panic(
        expected = "invariant SM-APPLY-IN-ORDER violated: node 1: entry 1 applied \
                               after applied index 1"
    )]
    fn applying_an_entry_twice_breaks_the_apply_order_invariant() {
        let mut state = machine();
        state.apply(&entry(1, put(1, "k", "v")));
        state.apply(&entry(1, put(1, "k", "v")));
    }

    #[test]
    #[should_panic(
        expected = "invariant SM-COMMAND-DECODES violated: node 1: committed entry \
                               1 (3 bytes) does not decode"
    )]
    fn an_undecodable_committed_command_breaks_its_invariant() {
        machine().apply(&entry(1, EntryPayload::Command(vec![1, 9, 9])));
    }

    #[test]
    #[should_panic(
        expected = "invariant SM-COMMAND-ADMITTED violated: node 1: committed entry \
                               1 carries a DELETE or compare-and-swap"
    )]
    fn a_committed_compare_and_swap_is_not_admitted_before_w27() {
        let cas = codec::encode(&Command {
            request_id: id(1),
            mutation: Mutation::Cas {
                key: key("k"),
                expected: ExpectedVersion::Absent,
                value: value("v"),
            },
        });
        machine().apply(&entry(1, EntryPayload::Command(cas)));
    }
}
