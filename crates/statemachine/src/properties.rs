//! Property tests for W09's acceptance (PLAN.md §12.2 W09). Over seeded random logs:
//! - a PUT's version is its execution index;
//! - a replay returns the original result;
//! - a conflicting command changes nothing;
//! - a quota refusal is terminal and deterministic;
//! - a DELETE removes its key and reports whether it existed, and a key's versions strictly
//!   increase across delete and re-create (W27.1, F-3, DM-3);
//! - a compare-and-swap whose expectation fails is a terminal `version_mismatch` that changes
//!   nothing, decided strictly before capacity; one whose expectation holds acts as a PUT
//!   (W27.2, F-5).
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

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{self, AssertUnwindSafe};

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ExpectedVersion, ItemCount, Key,
    KeyValueLimits, LiveStateLimit, LogIndex, MAX_SAFE_INTEGER, Mutation, NodeId, RequestId, Term,
    TerminalResult, Value, Version,
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
    /// A DELETE of key `k<key>`, under request ID `request`.
    Delete { request: u8, key: u8 },
    /// A compare-and-swap of key `k<key>` expecting `expected`, to a value of `len` bytes,
    /// under request ID `request`.
    Cas {
        request: u8,
        key: u8,
        expected: Expect,
        len: u8,
    },
}

/// A generated CAS expectation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// The key must be absent.
    Absent,
    /// The key must be at the version of entry `index`.
    At(u64),
}

impl Expect {
    fn version(self) -> ExpectedVersion {
        match self {
            Self::Absent => ExpectedVersion::Absent,
            Self::At(index) => {
                ExpectedVersion::Version(Version::at(LogIndex::new(index).unwrap()).unwrap())
            }
        }
    }
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
            Self::Delete { request, key } => EntryPayload::Command(codec::encode(&Command {
                request_id: RequestId::from_bytes([request; 16]),
                mutation: Mutation::Delete {
                    key: key_named(key),
                },
            })),
            Self::Cas {
                request,
                key,
                expected,
                len,
            } => EntryPayload::Command(codec::encode(&Command {
                request_id: RequestId::from_bytes([request; 16]),
                mutation: Mutation::Cas {
                    key: key_named(key),
                    expected: expected.version(),
                    value: Value::parse("v".repeat(usize::from(len)), KeyValueLimits::MAXIMA)
                        .unwrap(),
                },
            })),
        }
    }

    /// The key a PUT, DELETE or compare-and-swap names.
    const fn key(self) -> Option<u8> {
        match self {
            Self::Put { key, .. } | Self::Delete { key, .. } | Self::Cas { key, .. } => Some(key),
            Self::Empty => None,
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
    // The entry that last wrote each key, so a CAS often expects the key's current version.
    let mut last_write: BTreeMap<u8, u64> = BTreeMap::new();
    (1..=u64::try_from(STEPS).unwrap())
        .map(|index| {
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
            let key = below(&mut rng, 5);
            let step = match next(&mut rng) % 8 {
                0 | 1 => Step::Delete { request, key },
                2 | 3 => Step::Cas {
                    request,
                    key,
                    expected: match next(&mut rng) % 3 {
                        0 => Expect::Absent,
                        1 => last_write
                            .get(&key)
                            .copied()
                            .map_or(Expect::Absent, Expect::At),
                        _ => Expect::At(1 + next(&mut rng) % index),
                    },
                    len: below(&mut rng, 12),
                },
                _ => Step::Put {
                    request,
                    key,
                    len: below(&mut rng, 12),
                },
            };
            match step {
                Step::Put { .. } | Step::Cas { .. } => {
                    last_write.insert(key, index);
                }
                Step::Delete { .. } => {
                    last_write.remove(&key);
                }
                Step::Empty => {}
            }
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
                once(raw, &mut first, *request_id, *result)?;
                put_executed(raw, index, key, result, &before, &after)?;
                recorded(raw, &before, &after)?;
            }
            (Applied::Executed { request_id, result }, Step::Delete { key, .. }) => {
                once(raw, &mut first, *request_id, *result)?;
                delete_executed(raw, key, result, &before, &after)?;
                recorded(raw, &before, &after)?;
            }
            (Applied::Executed { request_id, result }, Step::Cas { key, expected, .. }) => {
                once(raw, &mut first, *request_id, *result)?;
                cas_executed(raw, index, (key, expected), result, &before, &after)?;
                recorded(raw, &before, &after)?;
            }
            (
                Applied::Replayed { request_id, result },
                Step::Put { .. } | Step::Delete { .. } | Step::Cas { .. },
            ) => {
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
                Step::Put { .. } | Step::Delete { .. } | Step::Cas { .. },
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
            (Applied::Empty, Step::Put { .. } | Step::Delete { .. } | Step::Cas { .. })
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

/// A DELETE's first execution of key `k<key>` (F-3, DM-3, DM-6): `existed` says whether the
/// key was live, nothing gets a version, the key is gone and every other key is untouched,
/// and the live bytes shrink only if it existed.
fn delete_executed(
    raw: u64,
    key: u8,
    result: &TerminalResult,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), String> {
    let was = before.keys.iter().any(|(live, _)| *live == key_named(key));
    if result.decision() != (Decision::Deleted { existed: was }) || result.version().is_some() {
        return Err(format!(
            "entry {raw}: a DELETE of a key that {} decided {result:?} (F-3, DM-3)",
            if was { "existed" } else { "was absent" }
        ));
    }
    let left: Vec<&(Key, Stored)> = before
        .keys
        .iter()
        .filter(|(live, _)| *live != key_named(key))
        .collect();
    if after.keys.iter().collect::<Vec<_>>() != left
        || (!was && after.live_bytes != before.live_bytes)
        || (was && after.live_bytes >= before.live_bytes)
    {
        return Err(format!(
            "entry {raw}: a DELETE left the live state wrong: before {before:?}, \
             after {after:?} (F-3, DM-6)"
        ));
    }
    Ok(())
}

/// A request ID executes for the first time once (C-6); records its result.
fn once(
    raw: u64,
    first: &mut BTreeMap<RequestId, TerminalResult>,
    request_id: RequestId,
    result: TerminalResult,
) -> Result<(), String> {
    if first.insert(request_id, result).is_some() {
        return Err(format!(
            "entry {raw}: request {request_id} executed twice (C-6)"
        ));
    }
    Ok(())
}

/// A compare-and-swap's first execution at entry `index` (F-5, §7.5 step 4): a failed
/// expectation is a `version_mismatch` naming the key's current version and changes nothing,
/// whatever the quota; a held one sets the key with `version = index`, or is
/// `capacity_exceeded` and changes nothing.
fn cas_executed(
    raw: u64,
    index: EntryIndex,
    (key, expected): (u8, Expect),
    result: &TerminalResult,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), String> {
    let current = version_in(before, key);
    let unchanged = after.keys == before.keys && after.live_bytes == before.live_bytes;
    let fits = if expected.version().matches(current) {
        match result.decision() {
            Decision::Set => version_in(after, key) == Some(index.version()),
            Decision::CapacityExceeded { .. } => unchanged,
            Decision::VersionMismatch { .. } | Decision::Deleted { .. } => false,
        }
    } else {
        result.decision() == (Decision::VersionMismatch { current }) && unchanged
    };
    if fits {
        Ok(())
    } else {
        Err(format!(
            "entry {raw}: a CAS of k{key} expecting {expected:?} with the key at {current:?} \
             decided {result:?} (F-5; mismatch is decided strictly before capacity)"
        ))
    }
}

/// A PUT's first execution of key `k<key>` at entry `index` (DM-3, DM-6): it sets the key
/// with `version = index`, or is `capacity_exceeded` and changes nothing.
fn put_executed(
    raw: u64,
    index: EntryIndex,
    key: u8,
    result: &TerminalResult,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), String> {
    match result.decision() {
        Decision::Set => {
            let version = version_in(after, key);
            if version == Some(index.version()) {
                Ok(())
            } else {
                Err(format!(
                    "entry {raw}: the key's version is {version:?}, not the execution index (DM-3)"
                ))
            }
        }
        Decision::CapacityExceeded { .. } => {
            if after.keys == before.keys && after.live_bytes == before.live_bytes {
                Ok(())
            } else {
                Err(format!(
                    "entry {raw}: capacity_exceeded changed the live state (DM-6)"
                ))
            }
        }
        Decision::Deleted { .. } | Decision::VersionMismatch { .. } => {
            Err(format!("entry {raw}: a PUT decided {result:?}"))
        }
    }
}

/// The version of key `k<key>` in `snapshot`, if it is live.
fn version_in(snapshot: &Snapshot, key: u8) -> Option<Version> {
    snapshot
        .keys
        .iter()
        .find(|(live, _)| *live == key_named(key))
        .map(|(_, stored)| stored.version())
}

/// A first execution is recorded in the history and counted toward DM-7 (DM-5, §7.5).
fn recorded(raw: u64, before: &Snapshot, after: &Snapshot) -> Result<(), String> {
    if after.command_bytes <= before.command_bytes || after.records != before.records + 1 {
        return Err(format!(
            "entry {raw}: a first execution was not recorded and counted (DM-5, DM-7)"
        ));
    }
    Ok(())
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
        let steps = generate(seed);
        let mut deleted: Vec<u8> = Vec::new();
        for (step, applied) in steps.iter().zip(check(&steps).unwrap()) {
            if let Applied::Executed { result, .. } = &applied {
                match (result.decision(), step.key()) {
                    (Decision::Set, Some(_)) if matches_cas(*step) => {
                        *seen.entry("cas set").or_insert(0) += 1;
                    }
                    (Decision::Deleted { existed: true }, Some(key)) => deleted.push(key),
                    (Decision::Set, Some(key)) if deleted.contains(&key) => {
                        *seen.entry("re-created").or_insert(0) += 1;
                        deleted.retain(|gone| *gone != key);
                    }
                    (
                        Decision::Set
                        | Decision::Deleted { .. }
                        | Decision::VersionMismatch { .. }
                        | Decision::CapacityExceeded { .. },
                        _,
                    ) => {}
                }
            }
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
                    Decision::Deleted { existed: true } => "deleted",
                    Decision::Deleted { existed: false } => "deleted absent",
                    Decision::VersionMismatch { .. } => "mismatch",
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
        "deleted",
        "deleted absent",
        "re-created",
        "mismatch",
        "cas set",
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
        Step::Delete { .. } | Step::Cas { .. } | Step::Empty => false,
    }
}

const fn matches_cas(step: Step) -> bool {
    match step {
        Step::Cas { .. } => true,
        Step::Put { .. } | Step::Delete { .. } | Step::Empty => false,
    }
}

#[test]
fn versions_strictly_increase_across_delete_and_re_create_on_seeded_logs() {
    // W27.1, DM-3: a key's version only grows over its whole life, deletes included. The
    // test is non-vacuous only if some key is re-created after a DELETE.
    let mut re_created = 0;
    for seed in REGRESSION_SEEDS.iter().copied().chain(0..SEEDS) {
        let steps = generate(seed);
        let mut highest: BTreeMap<u8, Version> = BTreeMap::new();
        let mut deleted: Vec<u8> = Vec::new();
        for (raw, (step, applied)) in (1_u64..).zip(steps.iter().zip(check(&steps).unwrap())) {
            let Applied::Executed { result, .. } = applied else {
                continue;
            };
            let Some(key) = step.key() else {
                continue;
            };
            match (result.decision(), result.version()) {
                (Decision::Set, Some(version)) => {
                    let previous = highest.insert(key, version);
                    assert!(
                        previous.is_none_or(|previous| previous < version),
                        "rule: a key's version strictly increases, across delete and \
                         re-create too (DM-3); seed {seed}, entry {raw}: k{key} got {version:?} \
                         after {previous:?}; re-run: ./dev check STATE-PROPERTIES --case \
                         properties::versions_strictly_increase_across_delete_and_re_create_on_seeded_logs"
                    );
                    if deleted.contains(&key) {
                        re_created += 1;
                        deleted.retain(|gone| *gone != key);
                    }
                }
                (Decision::Deleted { existed: true }, _) => deleted.push(key),
                (
                    Decision::Set
                    | Decision::Deleted { existed: false }
                    | Decision::VersionMismatch { .. }
                    | Decision::CapacityExceeded { .. },
                    _,
                ) => {}
            }
        }
    }
    assert!(
        re_created > 0,
        "fixture: some key is re-created after a DELETE, so the property is not vacuous (P10)"
    );
}

#[test]
fn a_cas_mismatch_is_decided_before_the_quota_on_seeded_logs() {
    // W27.2, §7.5 step 4: a CAS's expectation is checked strictly before capacity, and `check`
    // demands version_mismatch for every failed expectation. This keeps that ordering from
    // being vacuous (P10): the logs must hold failed expectations on a new key while the key
    // quota is full, whose write would also be refused, and held ones that the quota refuses.
    // The live keys are followed from the results alone.
    let quota = usize::try_from(limits().max_live_keys.get()).unwrap();
    let (mut mismatched_at_quota, mut refused) = (0, 0);
    for seed in REGRESSION_SEEDS.iter().copied().chain(0..SEEDS) {
        let steps = generate(seed);
        let mut live: BTreeSet<u8> = BTreeSet::new();
        for (step, applied) in steps.iter().zip(check(&steps).unwrap()) {
            let Applied::Executed { result, .. } = applied else {
                continue;
            };
            let Some(key) = step.key() else {
                continue;
            };
            let full = live.len() == quota && !live.contains(&key);
            match (result.decision(), matches_cas(*step)) {
                (Decision::VersionMismatch { current: None }, true) if full => {
                    mismatched_at_quota += 1;
                }
                (Decision::CapacityExceeded { .. }, true) => refused += 1,
                (Decision::Set, _) => {
                    live.insert(key);
                }
                (Decision::Deleted { existed: true }, _) => {
                    live.remove(&key);
                }
                (
                    Decision::VersionMismatch { .. }
                    | Decision::CapacityExceeded { .. }
                    | Decision::Deleted { existed: false },
                    _,
                ) => {}
            }
        }
    }
    assert!(
        mismatched_at_quota > 0 && refused > 0,
        "fixture: the seeded logs hold CAS mismatches on a new key at a full key quota \
         ({mismatched_at_quota}) and matching CAS writes refused by the quota ({refused}), so \
         mismatch-before-capacity is exercised (P10); re-run: ./dev check STATE-PROPERTIES \
         --case properties::a_cas_mismatch_is_decided_before_the_quota_on_seeded_logs"
    );
}
