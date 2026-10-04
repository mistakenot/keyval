//! The live-state quota and the committed-command-bytes counter (PLAN.md §7.5, DM-6,
//! DM-7; part of the leaf area `statemachine/apply`).
//!
//! The quota is judged at apply, against committed state, identically on every node,
//! because the semantic-config fingerprint covers its limits (DUR-8). A refused write is
//! a committed terminal result, recorded in the history like any other (DM-5). The limits
//! are checked in a fixed order:
//! 1. **Keys first.** A write that would create a key while the live key count is at
//!    `max_live_keys` is refused with `max_live_keys`, whatever its bytes
//!    (br-dm6-limit-order-31o5).
//! 2. **Then bytes.** A write that would grow the live key+value bytes beyond
//!    `max_live_bytes` is refused with `max_live_bytes`.
//!
//! An overwrite that keeps or shrinks the bytes always passes (DM-6).

use types::{ByteCount, Config, ItemCount, KeyValueLimits};

/// The semantic limits the state machine enforces (§7.10). Every node has the same
/// values, because the semantic-config fingerprint covers them (DUR-8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateLimits {
    /// The key and value limits commands are decoded under (D-80).
    pub key_values: KeyValueLimits,
    /// The most live keys (DM-6, `max_live_keys`).
    pub max_live_keys: ItemCount,
    /// The most live key+value bytes, decoded (DM-6, `max_live_bytes`).
    pub max_live_bytes: ByteCount,
}

impl StateLimits {
    /// The limits `config` sets.
    #[must_use]
    pub const fn of(config: &Config) -> Self {
        Self {
            key_values: config.key_value_limits(),
            max_live_keys: config.max_live_keys(),
            max_live_bytes: config.max_live_bytes(),
        }
    }
}

/// A key's and value's decoded UTF-8 bytes together: what DM-6's byte quota counts.
pub fn entry_bytes(key: &str, value: &str) -> ByteCount {
    // Keys and values are at most 64 KiB (DM-1, DM-2), so the sum always fits.
    u64::try_from(key.len() + value.len()).map_or(ByteCount::new(u64::MAX), ByteCount::new)
}

#[cfg(test)]
pub mod tests {
    // check: CAPACITY

    use types::{
        Decision, Entry, EntryIndex, EntryPayload, Key, LiveStateLimit, Mutation, NodeId,
        RequestId, Term, TerminalResult, Value,
    };

    use super::*;
    use crate::{Applied, Command, StateMachine, codec};

    /// A commit index far beyond any test log, for tests that are not about it.
    pub fn far() -> types::LogIndex {
        types::LogIndex::new(types::MAX_SAFE_INTEGER).unwrap()
    }

    /// Generous limits for tests that are not about the quota: 10,000 keys and 64 MiB.
    pub fn roomy() -> StateLimits {
        limits(10_000, 64 * 1024 * 1024)
    }

    fn limits(keys: u64, bytes: u64) -> StateLimits {
        StateLimits {
            key_values: KeyValueLimits::MAXIMA,
            max_live_keys: ItemCount::new(keys),
            max_live_bytes: ByteCount::new(bytes),
        }
    }

    /// A state machine under `limits`, with `log` applied from index 1, and what each entry
    /// did.
    fn run(limits: StateLimits, log: &[EntryPayload]) -> (StateMachine, Vec<Applied>) {
        let mut state = StateMachine::new(NodeId::new(1).unwrap(), limits);
        let results = (1..)
            .zip(log)
            .map(|(index, payload)| {
                state.apply(
                    &Entry {
                        index: EntryIndex::new(index).unwrap(),
                        term: Term::new(1).unwrap(),
                        payload: payload.clone(),
                    },
                    far(),
                )
            })
            .collect();
        (state, results)
    }

    fn put(request: u8, key: &str, value: &str) -> EntryPayload {
        EntryPayload::Command(codec::encode(&Command {
            request_id: RequestId::from_bytes([request; 16]),
            mutation: Mutation::Put {
                key: Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
                value: Value::parse(value.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            },
        }))
    }

    fn decision(applied: &Applied) -> Option<Decision> {
        match applied {
            Applied::Executed { result, .. } | Applied::Replayed { result, .. } => {
                Some(result.decision())
            }
            Applied::Empty | Applied::Conflict { .. } => None,
        }
    }

    const KEYS_REFUSED: Option<Decision> = Some(Decision::CapacityExceeded {
        limit: LiveStateLimit::Keys,
    });

    const BYTES_REFUSED: Option<Decision> = Some(Decision::CapacityExceeded {
        limit: LiveStateLimit::Bytes,
    });

    #[test]
    fn the_live_key_count_admits_n_minus_1_and_n_keys_and_refuses_n_plus_1() {
        let log = [
            put(1, "a", "1"),
            put(2, "b", "2"),
            put(3, "c", "3"),
            put(4, "a", "11"),
        ];
        let (state, results) = run(limits(2, 1_000), &log);
        let decisions: Vec<Option<Decision>> = results.iter().map(decision).collect();
        assert_eq!(
            decisions,
            [
                Some(Decision::Set),
                Some(Decision::Set),
                KEYS_REFUSED,
                Some(Decision::Set)
            ],
            "rule: keys 1 (N-1) and 2 (N) are admitted, a 3rd (N+1) is refused, and an \
             overwrite at N adds no key (DM-6)"
        );
        assert_eq!(
            state.live_keys(),
            ItemCount::new(2),
            "rule: the refused key was never created"
        );
        assert_eq!(
            state.get(&Key::parse("c".to_owned(), KeyValueLimits::MAXIMA).unwrap()),
            None,
            "rule: capacity_exceeded changes nothing (DM-6)"
        );
    }

    #[test]
    fn live_bytes_admit_n_minus_1_and_n_and_refuse_n_plus_1() {
        // Limit 10 bytes of key+value: "k"+8 bytes = 9 (N-1), then 10 (N), then 11 (N+1).
        let log = [
            put(1, "k", "12345678"),
            put(2, "k", "123456789"),
            put(3, "k", "1234567890"),
            put(4, "j", ""),
        ];
        let (state, results) = run(limits(100, 10), &log);
        let decisions: Vec<Option<Decision>> = results.iter().map(decision).collect();
        assert_eq!(
            decisions,
            [
                Some(Decision::Set),
                Some(Decision::Set),
                BYTES_REFUSED,
                BYTES_REFUSED
            ],
            "rule: 9 and 10 bytes are admitted, growth to 11 is refused, and a new 1-byte \
             key at the limit is refused for bytes (DM-6)"
        );
        assert_eq!(
            state.live_bytes(),
            ByteCount::new(10),
            "rule: refused writes leave the live bytes unchanged"
        );
    }

    #[test]
    fn equal_and_shrinking_overwrites_always_pass_at_the_limit() {
        let log = [
            put(1, "k", "123456789"),
            put(2, "k", "abcdefghi"),
            put(3, "k", "1"),
            put(4, "k", "123456789"),
        ];
        let (state, results) = run(limits(1, 10), &log);
        assert!(
            results.iter().all(|r| decision(r) == Some(Decision::Set)),
            "rule: at both limits, an equal-size and a shrinking overwrite pass, and growth \
             back up to the limit passes (DM-6); results={results:?}"
        );
        assert_eq!(
            state.live_bytes(),
            ByteCount::new(10),
            "rule: bytes track overwrites"
        );
    }

    #[test]
    fn the_key_count_is_checked_before_the_bytes() {
        let log = [put(1, "k", "123456789"), put(2, "j", "x")];
        let (_, results) = run(limits(1, 10), &log);
        assert_eq!(
            decision(&results[1]),
            KEYS_REFUSED,
            "rule: a write breaching both quotas reports max_live_keys \
             (br-dm6-limit-order-31o5)"
        );
    }

    #[test]
    fn capacity_exceeded_is_terminal_and_replays_after_space_frees() {
        let log = [
            put(1, "k", "123456789"),
            put(2, "j", "x"),
            put(3, "k", ""),
            put(2, "j", "x"),
        ];
        let (state, results) = run(limits(10, 10), &log);
        assert_eq!(
            results[3],
            Applied::Replayed {
                request_id: RequestId::from_bytes([2; 16]),
                result: TerminalResult::executed_at(
                    EntryIndex::new(2).unwrap(),
                    Decision::CapacityExceeded {
                        limit: LiveStateLimit::Bytes
                    }
                ),
            },
            "rule: a refused write is terminal: its retry replays capacity_exceeded even \
             after space freed (DM-5, DM-6)"
        );
        assert_eq!(
            state.get(&Key::parse("j".to_owned(), KeyValueLimits::MAXIMA).unwrap()),
            None,
            "rule: the replay executes nothing"
        );
    }

    #[test]
    fn committed_command_bytes_count_first_executions_only() {
        let lens: Vec<u64> = [put(1, "k", "v"), put(2, "kk", "vv"), put(3, "j", "1234")]
            .iter()
            .map(|p| match p {
                EntryPayload::Command(bytes) => u64::try_from(bytes.len()).unwrap(),
                EntryPayload::Empty => 0,
            })
            .collect();
        let log = [
            put(1, "k", "v"),
            EntryPayload::Empty,
            put(1, "k", "v"),
            put(2, "kk", "vv"),
            put(1, "other", "x"),
            put(3, "j", "1234"),
        ];
        let (state, results) = run(limits(10, 6), &log);
        assert_eq!(
            decision(&results[5]),
            BYTES_REFUSED,
            "rule: the last PUT is over quota (6 + 5 > 6 bytes)"
        );
        assert_eq!(
            state.committed_command_bytes(),
            ByteCount::new(lens.iter().sum()),
            "rule: first executions add their canonical length whatever the result \
             (capacity_exceeded included); replays, conflicts and empty entries add nothing \
             (DM-7)"
        );
    }

    #[test]
    fn accounting_is_identical_after_replay_from_scratch() {
        let log = [
            put(1, "a", "1"),
            put(2, "b", "22"),
            put(1, "a", "1"),
            EntryPayload::Empty,
            put(3, "c", "333"),
            put(4, "a", "4444"),
            put(2, "z", "conflict"),
        ];
        let first = run(limits(2, 8), &log);
        let again = run(limits(2, 8), &log);
        assert_eq!(
            first, again,
            "rule: replaying the log rebuilds the same keys, quotas, results and DM-7 \
             counter (D-10, DM-7)"
        );
        assert!(
            first.0.committed_command_bytes() > ByteCount::ZERO,
            "rule: the counter is exercised, not vacuously equal"
        );
    }
}
