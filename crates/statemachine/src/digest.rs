//! The applied-state digest (PLAN.md §10.2 heal, §10.5 K-7; T-5; leaf area
//! `statemachine/digest`, br-dbn0): SHA-256 over a canonical encoding of the ordered map
//! and the request history, the state that convergence compares across nodes.
//!
//! The applied index is left out on purpose: a trailing election no-op moves it without
//! changing state (K-7), and the god view records it separately. So are the live-bytes and
//! committed-command-bytes counters, which the map and history determine.
//!
//! The encoding, every integer an unsigned big-endian u64:
//! - the domain tag `keyval.state/v1` and a newline;
//! - the number of keys, then for each key in key order: its byte length and UTF-8 bytes,
//!   the value's byte length and bytes, and the version;
//! - the number of history records, then for each request ID in ID order: its 16 bytes,
//!   the first-execution index, the command digest (32 bytes), the result's index, and the
//!   decision as a tag byte, 0 set, 1 deleted, 2 version mismatch or 3 capacity exceeded,
//!   followed by its field: `existed` (one byte, 0 or 1), `current` (one byte, 0 absent
//!   or 1 present, then the version when present) or the refusing limit (one byte, 0 keys
//!   or 1 bytes).

use std::fmt;

use types::{Decision, LiveStateLimit, Sha256};

use crate::apply::StateMachine;

/// The domain tag that starts every encoding, so a digest of anything else never matches.
const DOMAIN: &[u8] = b"keyval.state/v1\n";

/// The applied-state digest of a state machine's map and request history (K-7).
///
/// Equal states have equal digests; distinct states differ, barring a SHA-256 collision,
/// the stated Assumption the command digests rest on too (§10.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StateDigest([u8; 32]);

impl StateDigest {
    /// The digest's bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for StateDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl StateMachine {
    /// The applied-state digest of the map and the request history (see the module
    /// documentation for the encoding).
    #[must_use]
    pub fn digest(&self) -> StateDigest {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN);
        number(&mut hasher, self.iter().count());
        for (key, stored) in self.iter() {
            number(&mut hasher, key.byte_len());
            hasher.update(key.as_str().as_bytes());
            number(&mut hasher, stored.value().byte_len());
            hasher.update(stored.value().as_str().as_bytes());
            hasher.update(&stored.version().get().to_be_bytes());
        }
        let history = self.history();
        number(&mut hasher, history.len());
        for (request_id, record) in history.iter() {
            hasher.update(&request_id.to_bytes());
            hasher.update(&record.first_index().get().to_be_bytes());
            hasher.update(record.digest().bytes());
            let result = record.result();
            hasher.update(&result.index().get().to_be_bytes());
            decision(&mut hasher, result.decision());
        }
        StateDigest(hasher.finalize())
    }
}

/// A count or length, as a big-endian u64.
fn number(hasher: &mut Sha256, count: usize) {
    hasher.update(&u64::try_from(count).unwrap_or(u64::MAX).to_be_bytes());
}

/// A decision: its tag byte, then its field.
fn decision(hasher: &mut Sha256, decision: Decision) {
    match decision {
        Decision::Set => hasher.update(&[0]),
        Decision::Deleted { existed } => hasher.update(&[1, u8::from(existed)]),
        Decision::VersionMismatch { current } => match current {
            Some(version) => {
                hasher.update(&[2, 1]);
                hasher.update(&version.get().to_be_bytes());
            }
            None => hasher.update(&[2, 0]),
        },
        Decision::CapacityExceeded { limit } => hasher.update(&[
            3,
            match limit {
                LiveStateLimit::Keys => 0,
                LiveStateLimit::Bytes => 1,
            },
        ]),
    }
}

#[cfg(test)]
mod tests {
    use types::{Entry, EntryIndex, EntryPayload, Key, KeyValueLimits, LogIndex, Mutation};
    use types::{NodeId, RequestId, Term, Value};

    use super::*;
    use crate::capacity::tests::{far, roomy};
    use crate::codec::{self, Command};
    use crate::history::CommandDigest;

    fn machine() -> StateMachine {
        StateMachine::new(NodeId::new(1).unwrap(), roomy())
    }

    fn command(request: u8, key: &str, value: &str) -> Command {
        Command {
            request_id: RequestId::from_bytes([request; 16]),
            mutation: Mutation::Put {
                key: Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
                value: Value::parse(value.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            },
        }
    }

    fn apply(state: &mut StateMachine, index: u64, payload: EntryPayload) {
        let entry = Entry {
            index: EntryIndex::new(index).unwrap(),
            term: Term::new(1).unwrap(),
            payload,
        };
        let _ = state.apply(&entry, far());
    }

    fn put(state: &mut StateMachine, index: u64, request: u8, key: &str, value: &str) {
        let bytes = codec::encode(&command(request, key, value));
        apply(state, index, EntryPayload::Command(bytes));
    }

    #[test]
    fn the_empty_state_has_a_fixed_digest() {
        // sha256(b"keyval.state/v1\n" ++ 0u64 ++ 0u64), computed independently.
        assert_eq!(
            machine().digest().to_string(),
            "1d507e1a39948e592f8cb583f566c54773c363d4aa5f89b63c50e0fab0d4cf48",
            "rule: the encoding is the documented one, domain tag first"
        );
    }

    #[test]
    fn the_digest_follows_the_documented_encoding() {
        let mut state = machine();
        put(&mut state, 1, 7, "a", "bc");
        let digest = CommandDigest::of(&command(7, "a", "bc"));
        let mut expected = b"keyval.state/v1\n".to_vec();
        for word in [1_u64, 1] {
            expected.extend(word.to_be_bytes());
        }
        expected.extend(b"a");
        expected.extend(2_u64.to_be_bytes());
        expected.extend(b"bc");
        expected.extend(1_u64.to_be_bytes()); // version: the execution index (DM-3)
        expected.extend(1_u64.to_be_bytes()); // one history record
        expected.extend([7; 16]);
        expected.extend(1_u64.to_be_bytes()); // first execution
        expected.extend(digest.bytes());
        expected.extend(1_u64.to_be_bytes()); // the result's index
        expected.push(0); // set
        assert_eq!(state.digest().bytes(), &Sha256::digest(&expected));
    }

    #[test]
    fn equal_states_agree_and_any_change_to_map_or_history_shows() {
        let build = |puts: &[(u64, u8, &str, &str)]| {
            let mut state = machine();
            for &(index, request, key, value) in puts {
                put(&mut state, index, request, key, value);
            }
            state
        };
        let base = build(&[(1, 1, "a", "x"), (2, 2, "b", "y")]);
        assert_eq!(
            base.digest(),
            build(&[(1, 1, "a", "x"), (2, 2, "b", "y")]).digest()
        );
        for (case, other) in [
            (
                "another value",
                build(&[(1, 1, "a", "x"), (2, 2, "b", "z")]),
            ),
            ("another key", build(&[(1, 1, "a", "x"), (2, 2, "c", "y")])),
            (
                "another request ID",
                build(&[(1, 1, "a", "x"), (2, 3, "b", "y")]),
            ),
            (
                "another version",
                build(&[(1, 1, "a", "x"), (2, 2, "b", "w"), (3, 4, "b", "y")]),
            ),
        ] {
            assert_ne!(
                base.digest(),
                other.digest(),
                "rule: {case} changes the digest"
            );
        }
    }

    #[test]
    fn a_trailing_no_op_or_a_replay_leaves_the_digest_unchanged() {
        let mut state = machine();
        put(&mut state, 1, 1, "a", "x");
        let before = state.digest();
        apply(&mut state, 2, EntryPayload::Empty);
        put(&mut state, 3, 1, "a", "x");
        assert_eq!(
            state.applied_index(),
            LogIndex::new(3).unwrap(),
            "the no-op and the replay were applied"
        );
        assert_eq!(
            state.digest(),
            before,
            "rule: K-7 compares state, which neither an election no-op nor a replayed \
             request changes; only the applied index moved"
        );
    }
}
