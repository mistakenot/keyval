//! Property tests for W09's acceptance (PLAN.md §12.2 W09). Over seeded random logs:
//! - a PUT's version is its execution index;
//! - a replay returns the original result;
//! - a conflicting command changes nothing;
//! - a quota refusal is terminal and deterministic.
//!
//! These are hand-rolled rather than proptest (the decision on
//! br-w09-4-invariants-property-tests-5mr): this is a deterministic crate, so no RNG crate.
//! - A `SplitMix64` stream drives generation.
//! - A delta-debugging shrinker reduces a failing log.
//! - A failure names its seed, the shrunk log and the exact re-run command.
//! - Seeds that once failed go in [`REGRESSION_SEEDS`], committed with the fix, and run
//!   first every time (W01.6).
#![cfg(test)]

// check: STATE-PROPERTIES

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits,
    LiveStateLimit, LogIndex, MAX_SAFE_INTEGER, Mutation, NodeId, RequestId, Term, TerminalResult,
    Value,
};

use crate::{Applied, Command, StateLimits, StateMachine, Stored, codec};

/// Seeds that once failed, replayed first on every run. Add the seed a failure names, with
/// the shrunk log it printed as a comment, in the commit that fixes it.
const REGRESSION_SEEDS: &[u64] = &[];

/// Random seeds per run, after the regression seeds.
const SEEDS: u64 = 300;

/// Entries per generated log.
const STEPS: usize = 48;

/// The re-run command a failure prints.
const RERUN: &str =
    "./dev check STATE-PROPERTIES --case properties::w09_properties_hold_on_seeded_logs";

/// Tight limits, so quota refusals are frequent: 3 keys and 24 bytes.
fn limits() -> StateLimits {
    StateLimits {
        key_values: KeyValueLimits::MAXIMA,
        max_live_keys: ItemCount::new(3),
        max_live_bytes: ByteCount::new(24),
    }
}

/// One generated log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// An empty entry: a new leader's no-op.
    Empty,
    /// A PUT of key `k<key>` to a value of `len` bytes, under request ID `request`.
    Put { request: u8, key: u8, len: u8 },
}

impl Step {
    fn payload(self) -> EntryPayload {
        match self {
            Self::Empty => EntryPayload::Empty,
            Self::Put { request, key, len } => EntryPayload::Command(codec::encode(&Command {
                request_id: RequestId::from_bytes([request; 16]),
                mutation: Mutation::Put {
                    key: key_named(key),
                    value: Value::parse("v".repeat(usize::from(len)), KeyValueLimits::MAXIMA)
                        .unwrap(),
                },
            })),
        }
    }
}

fn key_named(key: u8) -> Key {
    Key::parse(format!("k{key}"), KeyValueLimits::MAXIMA).unwrap()
}

/// A seeded `SplitMix64` step: these tests' only randomness (§7.14).
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn below(rng: &mut u64, bound: u64) -> u8 {
    u8::try_from(next(rng) % bound).unwrap()
}

/// A log of [`STEPS`] entries from `seed`. Few request IDs and keys, so retries, conflicts
/// and quota refusals all occur.
fn generate(seed: u64) -> Vec<Step> {
    let mut rng = seed;
    let mut first: BTreeMap<u8, Step> = BTreeMap::new();
    (0..STEPS)
        .map(|_| {
            if next(&mut rng).is_multiple_of(16) {
                return Step::Empty;
            }
            let request = below(&mut rng, 12);
            // A reused ID: half the time the same command (a retry), else a new one.
            if let Some(&previous) = first.get(&request)
                && next(&mut rng).is_multiple_of(2)
            {
                return previous;
            }
            let step = Step::Put {
                request,
                key: below(&mut rng, 5),
                len: below(&mut rng, 12),
            };
            first.entry(request).or_insert(step);
            step
        })
        .collect()
}

/// What the properties compare between entries: the live state, history size and counters.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    keys: Vec<(Key, Stored)>,
    records: usize,
    live_bytes: ByteCount,
    command_bytes: ByteCount,
}

fn snapshot(state: &StateMachine) -> Snapshot {
    Snapshot {
        keys: state
            .iter()
            .map(|(key, stored)| (key.clone(), stored.clone()))
            .collect(),
        records: state.history().len(),
        live_bytes: state.live_bytes(),
        command_bytes: state.committed_command_bytes(),
    }
}

/// Applies `steps` from index 1, checking every property after each entry; returns what each
/// entry did.
fn check(steps: &[Step]) -> Result<Vec<Applied>, String> {
    let mut state = StateMachine::new(NodeId::new(1).unwrap(), limits());
    let commit = LogIndex::new(MAX_SAFE_INTEGER).unwrap();
    let mut first: BTreeMap<RequestId, TerminalResult> = BTreeMap::new();
    let mut results = Vec::with_capacity(steps.len());
    for (raw, step) in (1..).zip(steps) {
        let index = EntryIndex::new(raw).unwrap();
        let before = snapshot(&state);
        let applied = state.apply(
            &Entry {
                index,
                term: Term::new(1).unwrap(),
                payload: step.payload(),
            },
            commit,
        );
        let after = snapshot(&state);
        match (&applied, *step) {
            (Applied::Empty, Step::Empty) => {
                if after != before {
                    return Err(format!("entry {raw}: an empty entry changed state"));
                }
            }
            (Applied::Executed { request_id, result }, Step::Put { key, .. }) => {
                if first.insert(*request_id, *result).is_some() {
                    return Err(format!(
                        "entry {raw}: request {request_id} executed twice (C-6)"
                    ));
                }
                match result.decision() {
                    Decision::Set => {
                        let version = state.get(&key_named(key)).map(Stored::version);
                        if version != Some(index.version()) {
                            return Err(format!(
                                "entry {raw}: the key's version is {version:?}, not the \
                                 execution index (DM-3)"
                            ));
                        }
                    }
                    Decision::CapacityExceeded { .. } => {
                        if after.keys != before.keys || after.live_bytes != before.live_bytes {
                            return Err(format!(
                                "entry {raw}: capacity_exceeded changed the live state (DM-6)"
                            ));
                        }
                    }
                    Decision::Deleted { .. } | Decision::VersionMismatch { .. } => {
                        return Err(format!("entry {raw}: a PUT decided {result:?}"));
                    }
                }
                if after.command_bytes <= before.command_bytes
                    || after.records != before.records + 1
                {
                    return Err(format!(
                        "entry {raw}: a first execution was not recorded and counted (DM-5, DM-7)"
                    ));
                }
            }
            (Applied::Replayed { request_id, result }, Step::Put { .. }) => {
                if first.get(request_id) != Some(result) {
                    return Err(format!(
                        "entry {raw}: a replay of {request_id} returned {result:?}, not the \
                         original {:?} (DM-5)",
                        first.get(request_id)
                    ));
                }
                if after != before {
                    return Err(format!("entry {raw}: a replay changed state"));
                }
            }
            (
                Applied::Conflict {
                    request_id,
                    original_index,
                },
                Step::Put { .. },
            ) => {
                let original = first.get(request_id).map(|result| result.index());
                if original != Some(original_index.index()) {
                    return Err(format!(
                        "entry {raw}: the conflict names entry {original_index}, not the first \
                         execution {original:?} (F-6)"
                    ));
                }
                if after != before {
                    return Err(format!(
                        "entry {raw}: a conflicting command changed state (C-6)"
                    ));
                }
            }
            (Applied::Empty, Step::Put { .. })
            | (
                Applied::Executed { .. } | Applied::Replayed { .. } | Applied::Conflict { .. },
                Step::Empty,
            ) => {
                return Err(format!("entry {raw}: {step:?} gave {applied:?}"));
            }
        }
        results.push(applied);
    }
    Ok(results)
}

/// [`check`] twice: every property holds, and the same log gives the same results both
/// times, quota refusals included (D-10). A panic, such as a fired invariant, is a failure
/// too.
fn holds(steps: &[Step]) -> Result<(), String> {
    let run = || panic::catch_unwind(AssertUnwindSafe(|| check(steps)));
    match (run(), run()) {
        (Ok(Ok(once)), Ok(Ok(again))) if once == again => Ok(()),
        (Ok(Ok(_)), Ok(Ok(_))) => Err("the same log gave different results (D-10)".to_owned()),
        (Ok(Err(why)), _) | (_, Ok(Err(why))) => Err(why),
        (Err(payload), _) | (_, Err(payload)) => Err(format!(
            "panicked: {}",
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("(non-text panic)")
        )),
    }
}

/// The shortest log, by deleting chunks and then single entries, for which `fails` still
/// holds: a delta-debugging shrink.
fn shrink(mut steps: Vec<Step>, fails: impl Fn(&[Step]) -> bool) -> Vec<Step> {
    let mut chunk = steps.len().div_ceil(2).max(1);
    loop {
        let mut start = 0;
        let mut removed = false;
        while start < steps.len() {
            let end = (start + chunk).min(steps.len());
            let candidate: Vec<Step> = steps[..start]
                .iter()
                .chain(&steps[end..])
                .copied()
                .collect();
            if fails(&candidate) {
                steps = candidate;
                removed = true;
            } else {
                start = end;
            }
        }
        if !removed {
            if chunk == 1 {
                return steps;
            }
            chunk = chunk.div_ceil(2);
        }
    }
}

#[test]
fn w09_properties_hold_on_seeded_logs() {
    for seed in REGRESSION_SEEDS.iter().copied().chain(0..SEEDS) {
        let steps = generate(seed);
        if let Err(why) = holds(&steps) {
            let shrunk = shrink(steps, |log| holds(log).is_err());
            panic!(
                "W09 property failed for seed {seed}: {why}\nshrunk log ({} entries): \
                 {shrunk:?}\nits failure: {:?}\nre-run: {RERUN}\npersist: add {seed} to \
                 REGRESSION_SEEDS in crates/statemachine/src/properties.rs",
                shrunk.len(),
                holds(&shrunk).err()
            );
        }
    }
}

#[test]
fn the_generated_logs_reach_every_outcome() {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for seed in 0..SEEDS {
        for applied in check(&generate(seed)).unwrap() {
            let kind = match applied {
                Applied::Empty => "empty",
                Applied::Replayed { .. } => "replayed",
                Applied::Conflict { .. } => "conflict",
                Applied::Executed { result, .. } => match result.decision() {
                    Decision::Set => "set",
                    Decision::CapacityExceeded {
                        limit: LiveStateLimit::Keys,
                    } => "refused for keys",
                    Decision::CapacityExceeded {
                        limit: LiveStateLimit::Bytes,
                    } => "refused for bytes",
                    Decision::Deleted { .. } | Decision::VersionMismatch { .. } => "other",
                },
            };
            *seen.entry(kind).or_insert(0) += 1;
        }
    }
    for kind in [
        "empty",
        "set",
        "refused for keys",
        "refused for bytes",
        "replayed",
        "conflict",
    ] {
        assert!(
            seen.get(kind).is_some_and(|&count| count > 0),
            "rule: the generator reaches every outcome, so the properties are not vacuous \
             (P10); missing {kind:?}; seen={seen:?}"
        );
    }
}

#[test]
fn the_shrinker_finds_a_minimal_failing_log() {
    // A stand-in failure: some PUT of key 2 is followed later by a PUT of key 3.
    let fails = |log: &[Step]| {
        let first_two = log.iter().position(|step| matches_key(*step, 2));
        first_two.is_some_and(|at| log[at..].iter().any(|step| matches_key(*step, 3)))
    };
    let (seed, log) = (0..SEEDS)
        .map(|seed| (seed, generate(seed)))
        .find(|(_, log)| fails(log))
        .unwrap();
    let shrunk = shrink(log.clone(), fails);
    assert!(
        fails(&shrunk) && shrunk.len() == 2,
        "rule: the shrinker keeps the failure and reduces it to its 2 essential entries; \
         seed={seed} from {} to {} entries: {shrunk:?}",
        log.len(),
        shrunk.len()
    );
}

fn matches_key(step: Step, wanted: u8) -> bool {
    match step {
        Step::Put { key, .. } => key == wanted,
        Step::Empty => false,
    }
}
