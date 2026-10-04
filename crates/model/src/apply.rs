//! The model's state and how a committed entry changes it (PLAN.md §6.2–§6.4, §7.5
//! apply steps, §8.3–§8.4).
//!
//! An entry is applied once, in log order. An empty entry changes nothing but the applied
//! index. A command whose request ID the history already holds is a **replay** when it is
//! the same command (the stored result, nothing changes) and a **conflict** when it is a
//! different one (nothing changes; the original result stays authoritative). Otherwise it
//! executes for the first time, and its result is recorded, whatever it is:
//! - PUT sets the key with version = the entry's index, unless the live-state capacity
//!   refuses it;
//! - DELETE removes the key if present and says whether it existed; it never gets a
//!   version and is never refused for capacity;
//! - CAS compares its expectation with the key's state at this moment (C-5). A mismatch
//!   returns the current version, or its absence. A match is then checked for capacity
//!   exactly like a PUT: mismatch strictly before capacity.
//!
//! **Capacity (DM-6).** A write is refused if it would create a key beyond
//! `max_live_keys`, or grow the live key and value bytes beyond `max_live_bytes`. An
//! overwrite that keeps or shrinks the bytes, and every DELETE, always succeed. When both
//! limits would be exceeded, the model names `max_live_keys`. §7.5 lists "create a key"
//! before "grow bytes", and the plan does not otherwise rank them.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ops::Bound;

use crate::data::{DataError, Index, Key, Limits, RequestId, Value, Version};

/// A CAS expectation (F-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    /// The key must be absent.
    Absent,
    /// The key must be present with this version.
    Version(Version),
}

/// What a write does (§6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// PUT `key` = `value` (F-1).
    Put {
        /// The key.
        key: Key,
        /// The value.
        value: Value,
    },
    /// DELETE `key` (F-3).
    Delete {
        /// The key.
        key: Key,
    },
    /// `COMPARE_SWAP` of `key` from `expected` to `value` (F-5).
    Cas {
        /// The key.
        key: Key,
        /// What the key must be now.
        expected: Expected,
        /// The value to set if it is.
        value: Value,
    },
}

impl Op {
    /// The key the write concerns.
    #[must_use]
    pub const fn key(&self) -> &Key {
        match self {
            Self::Put { key, .. } | Self::Delete { key } | Self::Cas { key, .. } => key,
        }
    }

    /// The value the write would set, if any.
    #[must_use]
    pub const fn value(&self) -> Option<&Value> {
        match self {
            Self::Put { value, .. } | Self::Cas { value, .. } => Some(value),
            Self::Delete { .. } => None,
        }
    }
}

/// A write command: an operation under a request ID (F-6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The request ID.
    pub request_id: RequestId,
    /// What it does.
    pub op: Op,
}

/// A committed log entry's payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// An empty entry, such as a new leader's no-op: it carries no command.
    Empty,
    /// A write command.
    Command(Command),
}

/// Which live-state limit refused a write, by its configuration name (DM-6, §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveLimit {
    /// `max_live_keys`: the write would have created a key beyond it.
    MaxLiveKeys,
    /// `max_live_bytes`: the write would have grown the live bytes beyond it.
    MaxLiveBytes,
}

impl LiveLimit {
    /// The limit's configuration name, as the API reports it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxLiveKeys => "max_live_keys",
            Self::MaxLiveBytes => "max_live_bytes",
        }
    }
}

/// What a command's first execution decided (§8.3, §8.4). Every one is terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A PUT or a matching CAS set the key; its version is the execution index (DM-3).
    Set {
        /// The key's new version.
        version: Version,
    },
    /// A DELETE ran, whether or not the key existed (F-3).
    Deleted {
        /// Whether the key existed.
        existed: bool,
    },
    /// A CAS expectation did not match (F-5): nothing changed.
    VersionMismatch {
        /// The key's version at that moment, or `None` if it was absent.
        current: Option<Version>,
    },
    /// The live-state capacity refused the write (DM-6): nothing changed.
    CapacityExceeded {
        /// The limit the write would have exceeded.
        limit: LiveLimit,
    },
}

/// A request's terminal result: what its first execution decided, and where. Retries and
/// status lookups return it unchanged (§8.1, F-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalResult {
    /// The request ID.
    pub request_id: RequestId,
    /// The index of the first execution.
    pub index: Index,
    /// What it decided.
    pub decision: Decision,
}

/// What applying one entry did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// An empty entry: only the applied index moved.
    Empty,
    /// A command's first execution.
    Executed(TerminalResult),
    /// The same command again under a used request ID: the stored result, and nothing
    /// changed (a replay, `replayed: true`).
    Replayed(TerminalResult),
    /// A different command under a used request ID: nothing changed, and the original
    /// result stays authoritative (`request_id_conflict`, F-6).
    Conflict {
        /// The request ID.
        request_id: RequestId,
        /// The index of the original's first execution.
        original_index: Index,
    },
}

/// Why an entry cannot be applied: the caller broke the model's contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// Entries apply once each, in log order.
    OutOfOrder {
        /// The index that should come next.
        expected: u64,
        /// The index given.
        found: Index,
    },
    /// The command's key or value breaks the limits in force, so no leader could have
    /// admitted it (F-8, D-80).
    Invalid(DataError),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfOrder { expected, found } => write!(
                f,
                "entry {} applied where entry {expected} is next",
                found.get()
            ),
            Self::Invalid(error) => write!(f, "inadmissible command: {error}"),
        }
    }
}

impl Error for ApplyError {}

/// A live key's value and version.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stored {
    value: Value,
    version: Version,
}

/// What the history keeps for a request ID: the command, and what it decided.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    command: Command,
    result: TerminalResult,
}

/// The reference model: the live key-value state, the request history and the applied
/// index, changed only by [`Model::apply`].
///
/// Invariants: `live_bytes` is the sum of every live key's and value's bytes; every
/// version and every recorded index is at most the applied index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    limits: Limits,
    map: BTreeMap<Key, Stored>,
    history: BTreeMap<RequestId, Record>,
    live_bytes: u64,
    applied: u64,
}

impl Model {
    /// The state before entry 1, under `limits`.
    #[must_use]
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            map: BTreeMap::new(),
            history: BTreeMap::new(),
            live_bytes: 0,
            applied: 0,
        }
    }

    /// The limits in force.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// The index of the last applied entry, or 0 before any.
    #[must_use]
    pub const fn applied_index(&self) -> u64 {
        self.applied
    }

    /// The key's value and version, or `None` if it is absent (F-2).
    ///
    /// # Errors
    ///
    /// [`DataError::KeyTooLong`] for a key over the configured key limit: the limit is in
    /// force wherever a key is parsed, so such a GET is `invalid_request` (§7.10, D-80,
    /// F-8).
    pub fn get(&self, key: &Key) -> Result<Option<(&Value, Version)>, DataError> {
        self.limits.check_key(key)?;
        Ok(self
            .map
            .get(key)
            .map(|stored| (&stored.value, stored.version)))
    }

    /// The live keys with their values and versions, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&Key, &Value, Version)> {
        self.map
            .iter()
            .map(|(key, stored)| (key, &stored.value, stored.version))
    }

    /// The live keys from `lower` on, with their values and versions, in key order. Only
    /// a lower bound is taken, so the range can never be inverted.
    pub(crate) fn entries_from<'a>(
        &'a self,
        lower: Bound<&'a Key>,
    ) -> impl Iterator<Item = (&'a Key, &'a Value, Version)> {
        self.map
            .range::<Key, _>((lower, Bound::Unbounded))
            .map(|(key, stored)| (key, &stored.value, stored.version))
    }

    /// A request ID's terminal result if it has executed, else `None` (F-9: `unknown`).
    #[must_use]
    pub fn status(&self, request_id: &RequestId) -> Option<TerminalResult> {
        self.history.get(request_id).map(|record| record.result)
    }

    /// How the history answers `command` without applying anything: a replay of the
    /// stored result if its request ID executed this same command, a conflict naming the
    /// original index if it executed a different one, or `None` if the ID never executed.
    ///
    /// This is the answer a Ready node gives a known request ID from its applied history,
    /// with no log entry (F-6, F-7's short-circuit), and the same decision [`Self::apply`]
    /// makes for a duplicate entry.
    #[must_use]
    pub fn classify(&self, command: &Command) -> Option<Applied> {
        self.history.get(&command.request_id).map(|record| {
            if record.command == *command {
                Applied::Replayed(record.result)
            } else {
                Applied::Conflict {
                    request_id: command.request_id,
                    original_index: record.result.index,
                }
            }
        })
    }

    /// The number of live keys (DM-6).
    #[must_use]
    pub fn live_keys(&self) -> u64 {
        u64::try_from(self.map.len()).unwrap_or(u64::MAX)
    }

    /// The live key and value bytes (DM-6).
    #[must_use]
    pub const fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    /// Applies the committed `entry` at `index`, which must be the next index, and says
    /// what it did.
    ///
    /// # Errors
    ///
    /// [`ApplyError::OutOfOrder`] unless `index` follows the applied index, or
    /// [`ApplyError::Invalid`] for a command breaking the key or value limits. On error
    /// nothing changes.
    pub fn apply(&mut self, index: Index, entry: &Entry) -> Result<Applied, ApplyError> {
        let expected = self.applied + 1;
        if index.get() != expected {
            return Err(ApplyError::OutOfOrder {
                expected,
                found: index,
            });
        }
        let applied = match entry {
            Entry::Empty => Applied::Empty,
            Entry::Command(command) => self.command(index, command)?,
        };
        self.applied = index.get();
        Ok(applied)
    }

    /// A command: a replay, a conflict, or its first execution.
    fn command(&mut self, index: Index, command: &Command) -> Result<Applied, ApplyError> {
        self.limits
            .check_key(command.op.key())
            .map_err(ApplyError::Invalid)?;
        if let Some(value) = command.op.value() {
            self.limits
                .check_value(value)
                .map_err(ApplyError::Invalid)?;
        }
        if let Some(answer) = self.classify(command) {
            return Ok(answer);
        }
        let decision = self.execute(index, &command.op);
        let result = TerminalResult {
            request_id: command.request_id,
            index,
            decision,
        };
        self.history.insert(
            command.request_id,
            Record {
                command: command.clone(),
                result,
            },
        );
        Ok(Applied::Executed(result))
    }

    /// A command's first execution against the current state.
    fn execute(&mut self, index: Index, op: &Op) -> Decision {
        match op {
            Op::Put { key, value } => self.set(index, key, value),
            Op::Delete { key } => {
                let removed = self.map.remove(key);
                if let Some(old) = &removed {
                    self.live_bytes -= key.bytes() + old.value.bytes();
                }
                Decision::Deleted {
                    existed: removed.is_some(),
                }
            }
            Op::Cas {
                key,
                expected,
                value,
            } => {
                let current = self.map.get(key).map(|stored| stored.version);
                let matches = match (expected, current) {
                    (Expected::Absent, None) => true,
                    (Expected::Version(wanted), Some(version)) => *wanted == version,
                    (Expected::Absent, Some(_)) | (Expected::Version(_), None) => false,
                };
                if matches {
                    self.set(index, key, value)
                } else {
                    Decision::VersionMismatch { current }
                }
            }
        }
    }

    /// Sets `key` to `value` at `index`, unless the live-state capacity refuses it.
    fn set(&mut self, index: Index, key: &Key, value: &Value) -> Decision {
        let old_bytes = self
            .map
            .get(key)
            .map(|stored| key.bytes() + stored.value.bytes());
        if old_bytes.is_none() && self.live_keys() + 1 > self.limits.max_live_keys() {
            return Decision::CapacityExceeded {
                limit: LiveLimit::MaxLiveKeys,
            };
        }
        let new_bytes = self.live_bytes - old_bytes.unwrap_or(0) + key.bytes() + value.bytes();
        if new_bytes > self.live_bytes && new_bytes > self.limits.max_live_bytes() {
            return Decision::CapacityExceeded {
                limit: LiveLimit::MaxLiveBytes,
            };
        }
        let version = Version::at(index);
        self.map.insert(
            key.clone(),
            Stored {
                value: value.clone(),
                version,
            },
        );
        self.live_bytes = new_bytes;
        Decision::Set { version }
    }
}

#[cfg(test)]
mod tests {
    // check: MODEL
    use super::*;

    fn key(text: &str) -> Key {
        Key::new(text).unwrap()
    }

    fn value(text: &str) -> Value {
        Value::new(text).unwrap()
    }

    fn id(n: u8) -> RequestId {
        RequestId::from_bytes([n; 16])
    }

    fn at(raw: u64) -> Index {
        Index::new(raw).unwrap()
    }

    fn version(raw: u64) -> Version {
        Version::new(raw).unwrap()
    }

    fn put(request: u8, k: &str, v: &str) -> Entry {
        Entry::Command(Command {
            request_id: id(request),
            op: Op::Put {
                key: key(k),
                value: value(v),
            },
        })
    }

    fn delete(request: u8, k: &str) -> Entry {
        Entry::Command(Command {
            request_id: id(request),
            op: Op::Delete { key: key(k) },
        })
    }

    fn cas(request: u8, k: &str, expected: Expected, v: &str) -> Entry {
        Entry::Command(Command {
            request_id: id(request),
            op: Op::Cas {
                key: key(k),
                expected,
                value: value(v),
            },
        })
    }

    /// Applies `entries` from index 1 and returns what each did.
    fn run(model: &mut Model, entries: &[Entry]) -> Vec<Applied> {
        let first = model.applied_index() + 1;
        (first..)
            .zip(entries)
            .map(|(index, entry)| model.apply(at(index), entry).unwrap())
            .collect()
    }

    fn executed(request: u8, index: u64, decision: Decision) -> Applied {
        Applied::Executed(TerminalResult {
            request_id: id(request),
            index: at(index),
            decision,
        })
    }

    fn set_at(index: u64) -> Decision {
        Decision::Set {
            version: version(index),
        }
    }

    #[test]
    fn a_put_sets_its_key_with_the_execution_index_as_version() {
        let mut model = Model::new(Limits::DEFAULT);
        let results = run(&mut model, &[Entry::Empty, put(1, "k", "v")]);
        assert_eq!(
            results,
            [Applied::Empty, executed(1, 2, set_at(2))],
            "rule: an empty entry only advances the applied index; a PUT at index 2 gives \
             version 2 (F-1, DM-3)"
        );
        assert_eq!(
            model.get(&key("k")).unwrap(),
            Some((&value("v"), version(2)))
        );
        assert_eq!(model.applied_index(), 2);
    }

    #[test]
    fn versions_increase_per_key_with_gaps_and_across_delete_and_recreate() {
        let mut model = Model::new(Limits::DEFAULT);
        let results = run(
            &mut model,
            &[
                put(1, "a", "x"),
                Entry::Empty,
                put(2, "a", "x"),
                delete(3, "a"),
                put(4, "a", "y"),
            ],
        );
        assert_eq!(
            results[2],
            executed(2, 3, set_at(3)),
            "rule: a same-value PUT still gets a new version (DM-3)"
        );
        assert_eq!(
            results[3],
            executed(3, 4, Decision::Deleted { existed: true }),
            "rule: DELETE says the key existed and assigns no version (F-3, DM-3)"
        );
        assert_eq!(
            model.get(&key("a")).unwrap().map(|(_, version)| version),
            Some(version(5)),
            "rule: versions strictly increase across delete and re-create (DM-3)"
        );
    }

    #[test]
    fn deleting_an_absent_key_succeeds_with_existed_false() {
        let mut model = Model::new(Limits::DEFAULT);
        assert_eq!(
            run(&mut model, &[delete(1, "nothing")]),
            [executed(1, 1, Decision::Deleted { existed: false })],
            "rule: deleting an absent key succeeds (F-3)"
        );
        assert_eq!(
            model.status(&id(1)),
            Some(TerminalResult {
                request_id: id(1),
                index: at(1),
                decision: Decision::Deleted { existed: false }
            }),
            "rule: it consumed its request ID (C-6)"
        );
    }

    #[test]
    fn a_replay_returns_the_original_result_and_changes_nothing() {
        let mut model = Model::new(Limits::DEFAULT);
        run(&mut model, &[put(1, "k", "v"), put(2, "k", "w")]);
        let before = model.clone();
        let replay = model.apply(at(3), &put(1, "k", "v")).unwrap();
        assert_eq!(
            replay,
            Applied::Replayed(TerminalResult {
                request_id: id(1),
                index: at(1),
                decision: set_at(1),
            }),
            "rule: a retry returns its original index and version, never the duplicate's \
             (DM-3, F-6)"
        );
        assert_eq!(
            (model.get(&key("k")).unwrap(), model.status(&id(1))),
            (before.get(&key("k")).unwrap(), before.status(&id(1))),
            "rule: a replay changes no key, version or record (C-6)"
        );
        assert_eq!(
            model.applied_index(),
            3,
            "rule: the applied index still advances"
        );
    }

    #[test]
    fn a_different_command_under_a_used_id_is_a_conflict_that_changes_nothing() {
        let mut model = Model::new(Limits::DEFAULT);
        run(&mut model, &[put(1, "k", "v")]);
        let before = model.clone();
        for (index, different) in [(2, put(1, "k", "other")), (3, delete(1, "k"))] {
            assert_eq!(
                model.apply(at(index), &different).unwrap(),
                Applied::Conflict {
                    request_id: id(1),
                    original_index: at(1),
                },
                "rule: another command under a used ID is request_id_conflict, naming the \
                 original's index (F-6, §8.4)"
            );
        }
        assert!(
            model.entries().eq(before.entries()) && model.status(&id(1)) == before.status(&id(1)),
            "rule: nothing changes and the original result stays authoritative (C-6)"
        );
    }

    #[test]
    fn cas_compares_with_the_state_at_application_and_a_mismatch_is_terminal() {
        let mut model = Model::new(Limits::DEFAULT);
        let results = run(
            &mut model,
            &[
                cas(1, "k", Expected::Absent, "first"),
                cas(2, "k", Expected::Absent, "again"),
                cas(3, "k", Expected::Version(version(1)), "second"),
                cas(4, "k", Expected::Version(version(1)), "stale"),
                cas(5, "gone", Expected::Version(version(3)), "x"),
            ],
        );
        assert_eq!(
            results,
            [
                executed(1, 1, set_at(1)),
                executed(
                    2,
                    2,
                    Decision::VersionMismatch {
                        current: Some(version(1))
                    }
                ),
                executed(3, 3, set_at(3)),
                executed(
                    4,
                    4,
                    Decision::VersionMismatch {
                        current: Some(version(3))
                    }
                ),
                executed(5, 5, Decision::VersionMismatch { current: None }),
            ],
            "rule: expected absent matches only an absent key, expected version v only version \
             v; a mismatch reports the current version or its absence (F-5, C-5)"
        );
        assert_eq!(
            model.apply(at(6), &cas(4, "k", Expected::Version(version(1)), "stale")),
            Ok(Applied::Replayed(TerminalResult {
                request_id: id(4),
                index: at(4),
                decision: Decision::VersionMismatch {
                    current: Some(version(3))
                },
            })),
            "rule: a mismatch consumes its request ID; retrying returns the same mismatch \
             (F-5)"
        );
    }

    #[test]
    fn live_keys_are_capped_at_n_and_refusals_are_terminal() {
        let limits = Limits::new(256, 65_536, 2, 1 << 20).unwrap();
        let mut model = Model::new(limits);
        let results = run(
            &mut model,
            &[
                put(1, "a", "1"),
                put(2, "b", "2"),
                put(3, "c", "3"),
                put(4, "a", "11"),
            ],
        );
        assert_eq!(
            results[..3],
            [
                executed(1, 1, set_at(1)),
                executed(2, 2, set_at(2)),
                executed(
                    3,
                    3,
                    Decision::CapacityExceeded {
                        limit: LiveLimit::MaxLiveKeys
                    }
                ),
            ],
            "rule: N-1 and N keys fit; the N+1th key is a terminal capacity_exceeded (DM-6)"
        );
        assert_eq!(
            results[3],
            executed(4, 4, set_at(4)),
            "rule: overwriting an existing key creates none, so it fits"
        );
        assert_eq!(
            model.get(&key("c")).unwrap(),
            None,
            "rule: the refused write changed nothing"
        );
        assert_eq!(
            model.apply(at(5), &put(3, "c", "3")),
            Ok(Applied::Replayed(TerminalResult {
                request_id: id(3),
                index: at(3),
                decision: Decision::CapacityExceeded {
                    limit: LiveLimit::MaxLiveKeys
                },
            })),
            "rule: the refusal is recorded; a retry returns it (§8.3)"
        );
    }

    #[test]
    fn live_bytes_are_capped_exactly_and_shrinking_writes_always_fit() {
        // Keys of 1 byte: "a" + 4 bytes and "b" + 4 bytes is exactly 10.
        let limits = Limits::new(256, 65_536, 100, 10).unwrap();
        let mut model = Model::new(limits);
        let results = run(
            &mut model,
            &[
                put(1, "a", "1234"),
                put(2, "b", "1234"),
                put(3, "a", "12345"),
                put(4, "a", "12"),
                put(5, "a", "1234"),
                put(6, "c", ""),
                delete(7, "b"),
            ],
        );
        assert_eq!(
            results,
            [
                executed(1, 1, set_at(1)),
                executed(2, 2, set_at(2)),
                executed(
                    3,
                    3,
                    Decision::CapacityExceeded {
                        limit: LiveLimit::MaxLiveBytes
                    }
                ),
                executed(4, 4, set_at(4)),
                executed(5, 5, set_at(5)),
                executed(
                    6,
                    6,
                    Decision::CapacityExceeded {
                        limit: LiveLimit::MaxLiveBytes
                    }
                ),
                executed(7, 7, Decision::Deleted { existed: true }),
            ],
            "rule: bytes may reach the limit exactly; a write that would grow them past it is \
             refused; shrinking overwrites and deletes always fit (DM-6)"
        );
        assert_eq!((model.live_keys(), model.live_bytes()), (1, 5));
    }

    #[test]
    fn a_cas_mismatch_is_decided_before_capacity() {
        let limits = Limits::new(256, 65_536, 1, 1 << 20).unwrap();
        let mut model = Model::new(limits);
        run(&mut model, &[put(1, "a", "1")]);
        assert_eq!(
            model.apply(at(2), &cas(2, "b", Expected::Version(version(9)), "x")),
            Ok(executed(2, 2, Decision::VersionMismatch { current: None })),
            "rule: a CAS that does not match is version_mismatch even when it would also be over \
             quota (§7.5: mismatch strictly before capacity)"
        );
        assert_eq!(
            model.apply(at(3), &cas(3, "b", Expected::Absent, "x")),
            Ok(executed(
                3,
                3,
                Decision::CapacityExceeded {
                    limit: LiveLimit::MaxLiveKeys
                }
            )),
            "rule: a matching CAS is checked for capacity exactly like a PUT (DM-6)"
        );
    }

    #[test]
    fn a_write_over_both_limits_names_max_live_keys() {
        let limits = Limits::new(256, 65_536, 1, 4).unwrap();
        let mut model = Model::new(limits);
        run(&mut model, &[put(1, "a", "12")]);
        assert_eq!(
            model.apply(at(2), &put(2, "b", "123")),
            Ok(executed(
                2,
                2,
                Decision::CapacityExceeded {
                    limit: LiveLimit::MaxLiveKeys
                }
            )),
            "rule: the model checks keys before bytes, in §7.5's order"
        );
    }

    #[test]
    fn classify_answers_a_known_request_id_from_history_without_applying() {
        let mut model = Model::new(Limits::DEFAULT);
        run(&mut model, &[put(1, "k", "v")]);
        let Entry::Command(same) = put(1, "k", "v") else {
            panic!("test setup: a PUT is a command");
        };
        let Entry::Command(different) = put(1, "k", "w") else {
            panic!("test setup: a PUT is a command");
        };
        let Entry::Command(unseen) = put(2, "k", "v") else {
            panic!("test setup: a PUT is a command");
        };
        let before = model.clone();
        assert_eq!(
            model.classify(&same),
            Some(Applied::Replayed(TerminalResult {
                request_id: id(1),
                index: at(1),
                decision: set_at(1),
            })),
            "rule: a Ready node answers a known request ID locally (F-6, F-7)"
        );
        assert_eq!(
            model.classify(&different),
            Some(Applied::Conflict {
                request_id: id(1),
                original_index: at(1),
            }),
            "rule: a different command under a used ID is a conflict, answered from history"
        );
        assert_eq!(
            model.classify(&unseen),
            None,
            "rule: an unseen ID has no history answer"
        );
        assert_eq!(
            model, before,
            "rule: classifying applies nothing (no log entry)"
        );
        assert_eq!(
            model.apply(at(2), &put(1, "k", "w")),
            Ok(model.classify(&different).unwrap()),
            "rule: a duplicate entry gets the same answer as the short-circuit"
        );
    }

    #[test]
    fn a_read_of_a_key_over_the_configured_limit_is_refused() {
        let limits = Limits::new(4, 8, 100, 1 << 20).unwrap();
        let model = Model::new(limits);
        assert_eq!(
            model.get(&key("abcde")),
            Err(DataError::KeyTooLong { bytes: 5, limit: 4 }),
            "rule: the configured key limit is in force wherever a key is parsed, so this GET \
             is invalid_request, never not_found (§7.10, D-80, F-8)"
        );
        assert_eq!(
            model.get(&key("abcd")),
            Ok(None),
            "rule: a key at the limit is fine"
        );
    }

    #[test]
    fn entries_apply_once_each_in_order() {
        let mut model = Model::new(Limits::DEFAULT);
        assert_eq!(
            model.apply(at(2), &Entry::Empty),
            Err(ApplyError::OutOfOrder {
                expected: 1,
                found: at(2)
            })
        );
        model.apply(at(1), &put(1, "k", "v")).unwrap();
        let before = model.clone();
        assert_eq!(
            model.apply(at(1), &put(2, "j", "w")),
            Err(ApplyError::OutOfOrder {
                expected: 2,
                found: at(1)
            }),
            "rule: an entry is applied once, in log order"
        );
        assert_eq!(model, before, "rule: a refused entry changes nothing");
    }

    #[test]
    fn a_command_over_the_configured_key_or_value_limit_is_inadmissible() {
        let limits = Limits::new(4, 8, 100, 1 << 20).unwrap();
        let mut model = Model::new(limits);
        assert!(matches!(
            model.apply(at(1), &put(1, "toolong", "v")),
            Err(ApplyError::Invalid(DataError::KeyTooLong {
                bytes: 7,
                limit: 4
            }))
        ));
        assert!(matches!(
            model.apply(at(1), &put(1, "k", "123456789")),
            Err(ApplyError::Invalid(DataError::ValueTooLong {
                bytes: 9,
                limit: 8
            }))
        ));
        assert_eq!(model.applied_index(), 0, "rule: nothing was applied");
    }

    #[test]
    fn status_lookups_return_the_stored_result_or_unknown() {
        let mut model = Model::new(Limits::DEFAULT);
        assert_eq!(
            model.status(&id(1)),
            None,
            "rule: unknown before it executes (F-9)"
        );
        run(&mut model, &[put(1, "k", "v"), put(1, "k", "other")]);
        assert_eq!(
            model.status(&id(1)),
            Some(TerminalResult {
                request_id: id(1),
                index: at(1),
                decision: set_at(1),
            }),
            "rule: the stored result is the first execution's, never a conflict (F-9, §8.3)"
        );
    }

    #[test]
    fn live_bytes_track_every_change() {
        let mut model = Model::new(Limits::DEFAULT);
        run(
            &mut model,
            &[
                put(1, "ab", "xyz"),
                put(2, "c", "1234"),
                put(3, "ab", "x"),
                delete(4, "c"),
                delete(5, "c"),
            ],
        );
        let recomputed: u64 = model
            .entries()
            .map(|(key, value, _)| key.bytes() + value.bytes())
            .sum();
        assert_eq!(
            (model.live_bytes(), model.live_keys()),
            (recomputed, 1),
            "rule: live bytes are the sum of live keys' and values' bytes (DM-6)"
        );
        assert_eq!(recomputed, 3);
    }
}
