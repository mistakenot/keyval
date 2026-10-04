//! Differential property tests: the state machine against the reference model (PLAN.md
//! §10.14 use 2, T-7; W10.3). Seeded random command sequences go into both; every apply
//! result (request ID included), every GET through the state machine's own read path, the
//! applied index and live-state counters after every step, and the final map (in the state
//! machine's own iteration order) and request history must match. PUT and GET are enforced
//! now; DELETE and compare-and-swap (W27) and SCAN (W28) are registered `planned`
//! (DIFFERENTIAL-DELETE-CAS, DIFFERENTIAL-SCAN) until the state machine implements them.
//!
//! This is an integration test of `sim`, the one crate allowed to depend on both a domain
//! crate and an oracle (DEP-RULE): Cargo builds it as its own test crate (§10.14 "a separate
//! test crate"; owner decision on br-w10-3-differential-tests-6ez).
//!
//! Hand-rolled rather than proptest, as for W09.4's property tests:
//! - a `SplitMix64` stream drives generation;
//! - a panic on either side is a divergence too, so it keeps its seed and log;
//! - a delta-debugging shrinker reduces a failing log;
//! - a failure names its seed, the shrunk log and its divergence, and the exact re-run
//!   command;
//! - seeds that once failed go in [`REGRESSION_SEEDS`] with the fix and run first (W01.6).
//!
//! A deliberately broken state machine must fail it (W10's acceptance). The broken variants
//! are test-local mutants around the real state machine, since no bug-variant switch exists
//! yet (§10.6; owner decision on the bead): a key quota and a byte quota each one too
//! loose, a version off by one, a replay reported as an execution, history kept under the
//! wrong request IDs, and a GET answered from the state before the last entry.
#![cfg(test)]

mod differential {
    // check: DIFFERENTIAL
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;
    use std::panic::{self, AssertUnwindSafe};

    use statemachine::{Applied, Command, StateLimits, StateMachine, codec};
    use types::{
        ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits,
        LiveStateLimit, Mutation, NodeId, RequestId, Term, TerminalResult, Value,
    };

    /// Seeds that once failed, replayed first on every run. Add the seed a failure names,
    /// with the shrunk log it printed as a comment, in the commit that fixes it.
    const REGRESSION_SEEDS: &[u64] = &[];

    /// Random seeds per run, after the regression seeds.
    const SEEDS: u64 = 400;

    /// Steps per generated log.
    const STEPS: usize = 64;

    /// The re-run command a failure of the main property prints.
    const RERUN: &str = "./dev check DIFFERENTIAL --case \
                         differential::put_and_get_match_the_model_on_seeded_logs";

    /// The re-run command a failure of the coverage floors prints.
    const RERUN_COVERAGE: &str = "./dev check DIFFERENTIAL --case \
                                  differential::the_generated_logs_reach_every_outcome_and_boundary";

    /// The filler of a request ID's bytes after its two-byte number.
    const FILLER: u8 = 0x5A;

    /// Request IDs a machine that keeps history under the wrong IDs uses instead: numbers
    /// no generated log reaches.
    const WRONG_IDS: u16 = 0x8000;

    /// Keys, with DM-4's ordering edge cases, composed and decomposed forms of one text,
    /// URL-hostile characters, and a key at the 256-byte maximum (§10.3).
    fn keys() -> [String; 7] {
        [
            "a".to_owned(),
            "aa".to_owned(),
            "b".to_owned(),
            "\u{e9}".to_owned(),
            "e\u{301}".to_owned(),
            "k/?&%".to_owned(),
            "x".repeat(256),
        ]
    }

    /// The index of the 256-byte key in [`keys`].
    const LONG_KEY: u8 = 6;

    /// Values: empty, short, multibyte and longer ones (§10.3).
    fn values() -> [String; 5] {
        [
            String::new(),
            "v".to_owned(),
            "w".to_owned(),
            "\u{65e5}\u{672c}\u{8a9e}".to_owned(),
            "y".repeat(40),
        ]
    }

    /// The index of the empty value in [`values`].
    const EMPTY_VALUE: u8 = 0;

    /// Live-state quotas `(max_live_keys, max_live_bytes)`, tight so that DM-6 refusals of
    /// both kinds are common, and the byte quota is often hit exactly at its boundary.
    const PROFILES: [(u64, u64); 4] = [(3, 1000), (8, 4), (2, 300), (1, 12)];

    /// One generated step.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        /// An empty entry: a new leader's no-op.
        Empty,
        /// A PUT entry: request number `request` sets key `key` to value `value`.
        Put { request: u16, key: u8, value: u8 },
        /// A GET of key `key` between entries.
        Get { key: u8 },
    }

    /// A generated log: its quota profile and its steps.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Log {
        profile: usize,
        steps: Vec<Step>,
    }

    impl Log {
        /// The request numbers the log's PUTs use.
        fn requests(&self) -> BTreeSet<u16> {
            self.steps
                .iter()
                .filter_map(|step| match step {
                    Step::Put { request, .. } => Some(*request),
                    Step::Empty | Step::Get { .. } => None,
                })
                .collect()
        }
    }

    /// The `SplitMix64` generator: deterministic, seeded, no RNG crate.
    struct SplitMix64(u64);

    impl SplitMix64 {
        const fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
        }

        fn byte_below(&mut self, bound: usize) -> u8 {
            u8::try_from(self.below(bound)).unwrap()
        }
    }

    /// The log for `seed`: empty entries, GETs, exact repeats of an earlier PUT (replays),
    /// earlier request IDs with another command (conflicts), and PUTs under fresh request
    /// IDs (first executions: overwrites and quota refusals), so logs stay deep.
    fn generate(seed: u64) -> Log {
        let mut rng = SplitMix64(seed);
        let profile = rng.below(PROFILES.len());
        let mut steps: Vec<Step> = Vec::with_capacity(STEPS);
        let mut fresh: u16 = 0;
        for _ in 0..STEPS {
            let roll = rng.below(100);
            let earlier: Vec<Step> = steps
                .iter()
                .copied()
                .filter(|step| matches!(step, Step::Put { .. }))
                .collect();
            let used: Vec<u16> = steps
                .iter()
                .filter_map(|step| match step {
                    Step::Put { request, .. } => Some(*request),
                    Step::Empty | Step::Get { .. } => None,
                })
                .collect();
            let key = rng.byte_below(keys().len());
            let value = rng.byte_below(values().len());
            let step = if roll < 8 {
                Step::Empty
            } else if roll < 30 {
                Step::Get { key }
            } else if roll < 42 && !earlier.is_empty() {
                earlier[rng.below(earlier.len())]
            } else if roll < 52 && !used.is_empty() {
                Step::Put {
                    request: used[rng.below(used.len())],
                    key,
                    value,
                }
            } else {
                fresh += 1;
                Step::Put {
                    request: fresh,
                    key,
                    value,
                }
            };
            steps.push(step);
        }
        Log { profile, steps }
    }

    /// The request ID numbered `request`: its number, then filler.
    fn request_bytes(request: u16) -> [u8; 16] {
        let mut bytes = [FILLER; 16];
        bytes[..2].copy_from_slice(&request.to_be_bytes());
        bytes
    }

    /// The number of a request ID, if it is one this test made.
    fn request_number(bytes: [u8; 16]) -> Option<u16> {
        let number = u16::from_be_bytes([bytes[0], bytes[1]]);
        (bytes == request_bytes(number)).then_some(number)
    }

    /// A decision, comparable across the two implementations: a PUT's version, DELETE's
    /// `existed`, a mismatch's current version, or the refusing limit's name.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Dec {
        Set(u64),
        Deleted(bool),
        Mismatch(Option<u64>),
        Capacity(&'static str),
    }

    /// What applying one entry did, comparable across the two implementations. Request IDs
    /// are their numbers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Outcome {
        Empty,
        Executed(u16, u64, Dec),
        Replayed(u16, u64, Dec),
        Conflict(u16, u64),
    }

    /// The first point where the two implementations disagree, or where one panicked.
    #[derive(Debug)]
    struct Divergence {
        step: usize,
        what: String,
    }

    impl fmt::Display for Divergence {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at step {}: {}", self.step, self.what)
        }
    }

    /// What the model's state said about a step before it applied, to classify the outcome.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Fact {
        /// The PUT creates its key.
        NewKey,
        /// Its key and value would take the live bytes past the byte quota.
        OverBytes,
        /// Its request ID's first execution was a quota refusal.
        OriginalRefused,
        /// After it, the live bytes sit exactly at the byte quota.
        AtQuotaAfter,
        /// It writes the 256-byte key.
        LongKey,
        /// It writes the empty value.
        EmptyValue,
    }

    /// The facts that hold for a step.
    type Context = BTreeSet<Fact>;

    /// Outcomes a run reached, so a run that never refuses, replays or meets a boundary
    /// cannot pass as evidence (P10).
    #[derive(Clone, Copy, Debug, Default)]
    struct Coverage {
        empty: u64,
        set: u64,
        overwrite: u64,
        set_at_quota: u64,
        long_key_set: u64,
        empty_value_set: u64,
        refused_keys: u64,
        refused_bytes: u64,
        refused_both: u64,
        replayed: u64,
        replayed_refusal: u64,
        conflict: u64,
        conflict_with_refusal: u64,
        get_hit: u64,
        get_miss: u64,
    }

    impl Coverage {
        fn count(&mut self, outcome: Outcome, context: &Context) {
            match outcome {
                Outcome::Empty => self.empty += 1,
                Outcome::Executed(_, _, Dec::Set(_)) => {
                    self.set += 1;
                    self.overwrite += u64::from(!context.contains(&Fact::NewKey));
                    self.set_at_quota += u64::from(context.contains(&Fact::AtQuotaAfter));
                    self.long_key_set += u64::from(context.contains(&Fact::LongKey));
                    self.empty_value_set += u64::from(context.contains(&Fact::EmptyValue));
                }
                Outcome::Executed(_, _, Dec::Capacity(limit)) => {
                    if limit == "max_live_keys" {
                        self.refused_keys += 1;
                        self.refused_both += u64::from(context.contains(&Fact::OverBytes));
                    } else {
                        self.refused_bytes += 1;
                    }
                }
                Outcome::Executed(_, _, Dec::Deleted(_) | Dec::Mismatch(_)) => {}
                Outcome::Replayed(_, _, decision) => {
                    self.replayed += 1;
                    self.replayed_refusal += u64::from(matches!(decision, Dec::Capacity(_)));
                }
                Outcome::Conflict(..) => {
                    self.conflict += 1;
                    self.conflict_with_refusal +=
                        u64::from(context.contains(&Fact::OriginalRefused));
                }
            }
        }

        const fn add(&mut self, other: Self) {
            self.empty += other.empty;
            self.set += other.set;
            self.overwrite += other.overwrite;
            self.set_at_quota += other.set_at_quota;
            self.long_key_set += other.long_key_set;
            self.empty_value_set += other.empty_value_set;
            self.refused_keys += other.refused_keys;
            self.refused_bytes += other.refused_bytes;
            self.refused_both += other.refused_both;
            self.replayed += other.replayed;
            self.replayed_refusal += other.replayed_refusal;
            self.conflict += other.conflict;
            self.conflict_with_refusal += other.conflict_with_refusal;
            self.get_hit += other.get_hit;
            self.get_miss += other.get_miss;
        }

        const fn all(self) -> [u64; 15] {
            [
                self.empty,
                self.set,
                self.overwrite,
                self.set_at_quota,
                self.long_key_set,
                self.empty_value_set,
                self.refused_keys,
                self.refused_bytes,
                self.refused_both,
                self.replayed,
                self.replayed_refusal,
                self.conflict,
                self.conflict_with_refusal,
                self.get_hit,
                self.get_miss,
            ]
        }
    }

    /// The broken variants a differential test must catch, and `None`, the real thing.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mutant {
        None,
        LooseKeyQuota,
        LooseByteQuota,
        VersionOffByOne,
        ReplayAsExecution,
        HistoryUnderWrongIds,
        StaleGet,
    }

    fn decision(result: TerminalResult) -> Dec {
        match result.decision() {
            // A missing version reads as 0, which no model version is, so it diverges.
            Decision::Set => Dec::Set(result.version().map_or(0, types::Version::get)),
            Decision::Deleted { existed } => Dec::Deleted(existed),
            Decision::VersionMismatch { current } => {
                Dec::Mismatch(current.map(types::Version::get))
            }
            Decision::CapacityExceeded { limit } => Dec::Capacity(match limit {
                LiveStateLimit::Keys => "max_live_keys",
                LiveStateLimit::Bytes => "max_live_bytes",
            }),
        }
    }

    /// The state machine under test, real or mutated.
    struct Machine {
        state: StateMachine,
        mutant: Mutant,
        /// The live state before the last entry, which a stale GET answers from.
        before: BTreeMap<String, (String, u64)>,
        /// Wrong request IDs handed out so far by a machine that keeps history under them.
        wrong: u16,
    }

    impl Machine {
        fn new(profile: usize, mutant: Mutant) -> Self {
            let (keys, bytes) = PROFILES[profile];
            let limits = StateLimits {
                key_values: KeyValueLimits::MAXIMA,
                max_live_keys: ItemCount::new(keys + u64::from(mutant == Mutant::LooseKeyQuota)),
                max_live_bytes: ByteCount::new(bytes + u64::from(mutant == Mutant::LooseByteQuota)),
            };
            Self {
                state: StateMachine::new(NodeId::new(1).unwrap(), limits),
                mutant,
                before: BTreeMap::new(),
                wrong: 0,
            }
        }

        fn version_shift(&self) -> u64 {
            u64::from(self.mutant == Mutant::VersionOffByOne)
        }

        /// Applies one entry. The state machine must echo the request ID it was sent.
        fn apply(&mut self, index: u64, step: Step) -> Result<Outcome, String> {
            if self.mutant == Mutant::StaleGet {
                self.before = self.map().into_iter().collect();
            }
            let (payload, sent) = match step {
                Step::Put {
                    request,
                    key,
                    value,
                } => {
                    let sent = self.request(request);
                    let command = Command {
                        request_id: RequestId::from_bytes(sent),
                        mutation: Mutation::Put {
                            key: Key::parse(
                                keys()[usize::from(key)].clone(),
                                KeyValueLimits::MAXIMA,
                            )
                            .unwrap(),
                            value: Value::parse(
                                values()[usize::from(value)].clone(),
                                KeyValueLimits::MAXIMA,
                            )
                            .unwrap(),
                        },
                    };
                    (
                        EntryPayload::Command(codec::encode(&command)),
                        Some((sent, request)),
                    )
                }
                Step::Empty | Step::Get { .. } => (EntryPayload::Empty, None),
            };
            let entry = Entry {
                index: EntryIndex::new(index).unwrap(),
                term: Term::new(1).unwrap(),
                payload,
            };
            let applied = self.state.apply(&entry, entry.index.index());
            self.outcome(&applied, sent)
        }

        /// The request ID a command carries: its own, or a wrong one for a machine that
        /// keeps its history under wrong IDs (so every command looks first-seen).
        fn request(&mut self, request: u16) -> [u8; 16] {
            if self.mutant == Mutant::HistoryUnderWrongIds {
                self.wrong += 1;
                request_bytes(WRONG_IDS + self.wrong)
            } else {
                request_bytes(request)
            }
        }

        fn outcome(
            &self,
            applied: &Applied,
            sent: Option<([u8; 16], u16)>,
        ) -> Result<Outcome, String> {
            let echoed = |request_id: RequestId| match sent {
                Some((bytes, number)) if request_id.to_bytes() == bytes => Ok(number),
                Some((bytes, _)) => Err(format!(
                    "the state machine echoed request ID {:02x?} for a command sent as {bytes:02x?}",
                    request_id.to_bytes()
                )),
                None => Err(format!(
                    "the state machine named request ID {:02x?} for an empty entry",
                    request_id.to_bytes()
                )),
            };
            let shift = |decision: Dec| match decision {
                Dec::Set(version) => Dec::Set(version + self.version_shift()),
                Dec::Deleted(_) | Dec::Mismatch(_) | Dec::Capacity(_) => decision,
            };
            Ok(match *applied {
                Applied::Empty => Outcome::Empty,
                Applied::Executed { request_id, result } => Outcome::Executed(
                    echoed(request_id)?,
                    result.index().get(),
                    shift(decision(result)),
                ),
                Applied::Replayed { request_id, result } => {
                    let replayed = (
                        echoed(request_id)?,
                        result.index().get(),
                        shift(decision(result)),
                    );
                    if self.mutant == Mutant::ReplayAsExecution {
                        Outcome::Executed(replayed.0, replayed.1, replayed.2)
                    } else {
                        Outcome::Replayed(replayed.0, replayed.1, replayed.2)
                    }
                }
                Applied::Conflict {
                    request_id,
                    original_index,
                } => Outcome::Conflict(echoed(request_id)?, original_index.index().get()),
            })
        }

        /// The live map as the state machine iterates it, in its own order.
        fn map(&self) -> Vec<(String, (String, u64))> {
            self.state
                .iter()
                .map(|(key, stored)| {
                    let version = stored.version().get() + self.version_shift();
                    (
                        key.as_str().to_owned(),
                        (stored.value().as_str().to_owned(), version),
                    )
                })
                .collect()
        }

        /// A GET through the state machine's own read path (F-2).
        fn get(&self, key: &str) -> Option<(String, u64)> {
            if self.mutant == Mutant::StaleGet {
                return self.before.get(key).cloned();
            }
            let key = Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap();
            self.state.get(&key).map(|stored| {
                let version = stored.version().get() + self.version_shift();
                (stored.value().as_str().to_owned(), version)
            })
        }

        fn counters(&self) -> (u64, u64, u64) {
            (
                self.state.applied_index().get(),
                self.state.live_keys().get(),
                self.state.live_bytes().get(),
            )
        }

        /// The stored result of request number `request`: its first execution's index and
        /// decision.
        fn status(&self, request: u16) -> Option<(u64, Dec)> {
            let id = RequestId::from_bytes(request_bytes(request));
            self.state
                .history()
                .get(&id)
                .map(|record| (record.first_index().get(), decision(record.result())))
        }

        /// The request IDs in the state machine's history that no command of the log carried.
        fn unknown_ids(&self, requests: &BTreeSet<u16>) -> Vec<[u8; 16]> {
            self.state
                .history()
                .iter()
                .map(|(request_id, _)| request_id.to_bytes())
                .filter(|bytes| request_number(*bytes).is_none_or(|n| !requests.contains(&n)))
                .collect()
        }
    }

    fn model_key(key: u8) -> model::Key {
        model::Key::new(keys()[usize::from(key)].clone()).unwrap()
    }

    fn model_decision(decision: model::Decision) -> Dec {
        match decision {
            model::Decision::Set { version } => Dec::Set(version.get()),
            model::Decision::Deleted { existed } => Dec::Deleted(existed),
            model::Decision::VersionMismatch { current } => {
                Dec::Mismatch(current.map(model::Version::get))
            }
            model::Decision::CapacityExceeded { limit } => Dec::Capacity(limit.as_str()),
        }
    }

    /// The number of a request ID the model names: always one the log sent.
    fn model_request(request_id: model::RequestId, requests: &BTreeSet<u16>) -> u16 {
        requests
            .iter()
            .copied()
            .find(|request| model::RequestId::from_bytes(request_bytes(*request)) == request_id)
            .unwrap()
    }

    fn model_outcome(applied: model::Applied, requests: &BTreeSet<u16>) -> Outcome {
        match applied {
            model::Applied::Empty => Outcome::Empty,
            model::Applied::Executed(result) => Outcome::Executed(
                model_request(result.request_id, requests),
                result.index.get(),
                model_decision(result.decision),
            ),
            model::Applied::Replayed(result) => Outcome::Replayed(
                model_request(result.request_id, requests),
                result.index.get(),
                model_decision(result.decision),
            ),
            model::Applied::Conflict {
                request_id,
                original_index,
            } => Outcome::Conflict(model_request(request_id, requests), original_index.get()),
        }
    }

    fn model_entry(step: Step) -> model::Entry {
        match step {
            Step::Put {
                request,
                key,
                value,
            } => model::Entry::Command(model::Command {
                request_id: model::RequestId::from_bytes(request_bytes(request)),
                op: model::Op::Put {
                    key: model_key(key),
                    value: model::Value::new(values()[usize::from(value)].clone()).unwrap(),
                },
            }),
            Step::Empty | Step::Get { .. } => model::Entry::Empty,
        }
    }

    fn model_limits(profile: usize) -> model::Limits {
        let (keys, bytes) = PROFILES[profile];
        model::Limits::new(model::MAX_KEY_BYTES, model::MAX_VALUE_BYTES, keys, bytes).unwrap()
    }

    /// What the model's state says about `step` before it applies.
    fn context_before(step: Step, profile: usize, model: &model::Model) -> Context {
        let Step::Put {
            request,
            key,
            value,
        } = step
        else {
            return Context::new();
        };
        let (_, max_bytes) = PROFILES[profile];
        let grown = keys()[usize::from(key)].len() + values()[usize::from(value)].len();
        let original = model.status(&model::RequestId::from_bytes(request_bytes(request)));
        let facts = [
            (Fact::NewKey, model.get(&model_key(key)).unwrap().is_none()),
            (
                Fact::OverBytes,
                model.live_bytes() + u64::try_from(grown).unwrap() > max_bytes,
            ),
            (
                Fact::OriginalRefused,
                original.is_some_and(|result| {
                    matches!(result.decision, model::Decision::CapacityExceeded { .. })
                }),
            ),
            (Fact::LongKey, key == LONG_KEY),
            (Fact::EmptyValue, value == EMPTY_VALUE),
        ];
        facts
            .into_iter()
            .filter_map(|(fact, holds)| holds.then_some(fact))
            .collect()
    }

    fn diverged(step: usize, what: String) -> Result<(), Divergence> {
        Err(Divergence { step, what })
    }

    /// One entry into both; the results must be equal.
    fn compare_apply(
        at: usize,
        index: u64,
        step: Step,
        log: &Log,
        machine: &mut Machine,
        model: &mut model::Model,
        coverage: &mut Coverage,
    ) -> Result<(), Divergence> {
        let mut context = context_before(step, log.profile, model);
        let index_of = model::Index::new(index).unwrap();
        let expected = match model.apply(index_of, &model_entry(step)) {
            Ok(applied) => model_outcome(applied, &log.requests()),
            Err(error) => return diverged(at, format!("the model refused {step:?}: {error}")),
        };
        if model.live_bytes() == PROFILES[log.profile].1 {
            context.insert(Fact::AtQuotaAfter);
        }
        coverage.count(expected, &context);
        match machine.apply(index, step) {
            Ok(observed) if observed == expected => Ok(()),
            Ok(observed) => diverged(
                at,
                format!(
                    "entry {index} {step:?}: the state machine says {observed:?}, the model \
                     {expected:?}"
                ),
            ),
            Err(what) => diverged(at, format!("entry {index} {step:?}: {what}")),
        }
    }

    /// One GET of both; the answers must be equal.
    fn compare_get(
        at: usize,
        key: u8,
        machine: &Machine,
        model: &model::Model,
        coverage: &mut Coverage,
    ) -> Result<(), Divergence> {
        let expected = model
            .get(&model_key(key))
            .unwrap()
            .map(|(value, version)| (value.as_str().to_owned(), version.get()));
        let observed = machine.get(&keys()[usize::from(key)]);
        if expected.is_some() {
            coverage.get_hit += 1;
        } else {
            coverage.get_miss += 1;
        }
        if observed == expected {
            Ok(())
        } else {
            diverged(
                at,
                format!(
                    "GET key {key}: the state machine says {observed:?}, the model {expected:?}"
                ),
            )
        }
    }

    /// The applied index and the live-state counters after a step (§7.5 step 5, DM-6).
    fn compare_counters(
        at: usize,
        machine: &Machine,
        model: &model::Model,
    ) -> Result<(), Divergence> {
        let observed = machine.counters();
        let expected = (model.applied_index(), model.live_keys(), model.live_bytes());
        if observed == expected {
            Ok(())
        } else {
            diverged(
                at,
                format!(
                    "(applied index, live keys, live bytes): the state machine says \
                     {observed:?}, the model {expected:?}"
                ),
            )
        }
    }

    /// The final map, each in its own iteration order, and every request's stored result
    /// (DM-4, DM-5).
    fn compare_final(log: &Log, machine: &Machine, model: &model::Model) -> Result<(), Divergence> {
        let at = log.steps.len();
        let observed = machine.map();
        let expected: Vec<(String, (String, u64))> = model
            .entries()
            .map(|(key, value, version)| {
                (
                    key.as_str().to_owned(),
                    (value.as_str().to_owned(), version.get()),
                )
            })
            .collect();
        if observed != expected {
            return diverged(
                at,
                format!("final map: the state machine has {observed:?}, the model {expected:?}"),
            );
        }
        let requests = log.requests();
        for request in requests.iter().copied() {
            let observed = machine.status(request);
            let expected = model
                .status(&model::RequestId::from_bytes(request_bytes(request)))
                .map(|result| (result.index.get(), model_decision(result.decision)));
            if observed != expected {
                return diverged(
                    at,
                    format!(
                        "request {request}'s stored result: the state machine has {observed:?}, \
                         the model {expected:?}"
                    ),
                );
            }
        }
        let unknown = machine.unknown_ids(&requests);
        if unknown.is_empty() {
            Ok(())
        } else {
            diverged(
                at,
                format!(
                    "the state machine's history holds request IDs no command carried: {unknown:02x?}"
                ),
            )
        }
    }

    /// Runs `log` through the machine (with `mutant`) and the model, and returns the first
    /// divergence, or what the run covered.
    fn run(log: &Log, mutant: Mutant) -> Result<Coverage, Divergence> {
        let mut machine = Machine::new(log.profile, mutant);
        let mut model = model::Model::new(model_limits(log.profile));
        let mut coverage = Coverage::default();
        let mut index = 0;
        for (at, step) in log.steps.iter().copied().enumerate() {
            if let Step::Get { key } = step {
                compare_get(at, key, &machine, &model, &mut coverage)?;
            } else {
                index += 1;
                compare_apply(
                    at,
                    index,
                    step,
                    log,
                    &mut machine,
                    &mut model,
                    &mut coverage,
                )?;
            }
            compare_counters(at, &machine, &model)?;
        }
        compare_final(log, &machine, &model)?;
        Ok(coverage)
    }

    /// [`run`], with a panic on either side (a broken invariant, an overflow) reported as a
    /// divergence, so it keeps its seed and log and can be shrunk.
    fn guarded(log: &Log, mutant: Mutant) -> Result<Coverage, Divergence> {
        panic::catch_unwind(AssertUnwindSafe(|| run(log, mutant))).unwrap_or_else(|payload| {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("a non-string panic");
            Err(Divergence {
                step: log.steps.len(),
                what: format!("a panic: {message}"),
            })
        })
    }

    /// The smallest log the delta-debugging pass finds that still diverges.
    fn shrink(log: &Log, mutant: Mutant) -> Log {
        let mut current = log.clone();
        let mut chunk = (current.steps.len() / 2).max(1);
        loop {
            let mut start = 0;
            let mut removed = false;
            while start < current.steps.len() {
                let mut candidate = current.clone();
                let end = (start + chunk).min(candidate.steps.len());
                candidate.steps.drain(start..end);
                if guarded(&candidate, mutant).is_err() {
                    current = candidate;
                    removed = true;
                } else {
                    start += chunk;
                }
            }
            if !removed {
                if chunk == 1 {
                    return current;
                }
                chunk /= 2;
            }
        }
    }

    /// The first seed on which `mutant` diverges, with its shrunk log and that log's
    /// divergence. The shrunk log passes on the real state machine, so the mutation, not
    /// the log, is what fails.
    fn caught(mutant: Mutant) -> (u64, Log, Divergence) {
        for seed in 0..SEEDS {
            let log = generate(seed);
            if guarded(&log, mutant).is_err() {
                let shrunk = shrink(&log, mutant);
                let divergence = guarded(&shrunk, mutant).unwrap_err();
                let control = guarded(&shrunk, Mutant::None);
                assert!(
                    control.is_ok(),
                    "rule: a mutant's shrunk log passes on the real state machine; mutant \
                     {mutant:?} seed {seed}: shrunk log {:?} (profile {}) also fails unmutated: \
                     {:?}",
                    shrunk.steps,
                    shrunk.profile,
                    control.err()
                );
                return (seed, shrunk, divergence);
            }
        }
        panic!(
            "rule: a deliberately broken state machine fails the differential test (W10); \
             mutant {mutant:?} survived seeds 0..{SEEDS}"
        )
    }

    /// `mutant` is caught, and its shrunk log is small enough to read.
    fn assert_caught(mutant: Mutant, most_steps: usize) {
        let (seed, shrunk, divergence) = caught(mutant);
        assert!(
            !shrunk.steps.is_empty() && shrunk.steps.len() <= most_steps,
            "rule: a failure shrinks to a readable log; mutant {mutant:?} seed {seed}: \
             {divergence}; shrunk to {} steps {:?} (profile {})",
            shrunk.steps.len(),
            shrunk.steps,
            shrunk.profile
        );
    }

    #[test]
    fn put_and_get_match_the_model_on_seeded_logs() {
        for seed in REGRESSION_SEEDS.iter().copied().chain(0..SEEDS) {
            let log = generate(seed);
            if let Err(divergence) = guarded(&log, Mutant::None) {
                let shrunk = shrink(&log, Mutant::None);
                let shrunk_divergence = guarded(&shrunk, Mutant::None).unwrap_err();
                panic!(
                    "rule: the state machine matches the reference model on random command \
                     sequences (§10.14, T-7); seed {seed}: {divergence}; shrunk log {:?} \
                     (profile {}) fails {shrunk_divergence}; re-run: {RERUN}; add the seed to \
                     REGRESSION_SEEDS with the fix",
                    shrunk.steps, shrunk.profile
                );
            }
        }
    }

    #[test]
    fn the_generated_logs_reach_every_outcome_and_boundary() {
        let mut total = Coverage::default();
        for seed in 0..SEEDS {
            match guarded(&generate(seed), Mutant::None) {
                Ok(coverage) => total.add(coverage),
                Err(divergence) => panic!(
                    "rule: coverage is counted only over passing logs, and seed {seed} fails: \
                     {divergence}; re-run: {RERUN_COVERAGE}"
                ),
            }
        }
        assert!(
            total.all().iter().all(|count| *count > 0),
            "rule: the generated logs reach empty entries; sets, overwrites, a set landing on \
             the byte quota, the 256-byte key and the empty value; key, byte and both-limit \
             refusals; replays, of refusals too; conflicts, with refused originals too; and \
             GET hits and misses, so a pass is not vacuous (P10); coverage={total:?}; re-run: \
             {RERUN_COVERAGE}"
        );
    }

    #[test]
    fn a_failure_shrinks_to_its_cause() {
        let (seed, shrunk, divergence) = caught(Mutant::StaleGet);
        let cause = match shrunk.steps[..] {
            [Step::Put { key: put, .. }, Step::Get { key: got }] => put == got,
            _ => false,
        };
        assert!(
            cause,
            "rule: the shrinker reduces a failure to its smallest cause, a PUT then a GET of \
             its key (W01.6); seed {seed}: {divergence}; shrunk to {:?}",
            shrunk.steps
        );
    }

    #[test]
    fn a_key_quota_one_too_loose_is_caught() {
        assert_caught(Mutant::LooseKeyQuota, 8);
    }

    #[test]
    fn a_byte_quota_one_too_loose_is_caught() {
        assert_caught(Mutant::LooseByteQuota, 8);
    }

    #[test]
    fn a_version_off_by_one_is_caught() {
        assert_caught(Mutant::VersionOffByOne, 1);
    }

    #[test]
    fn a_replay_reported_as_an_execution_is_caught() {
        assert_caught(Mutant::ReplayAsExecution, 4);
    }

    #[test]
    fn history_under_the_wrong_request_ids_is_caught() {
        assert_caught(Mutant::HistoryUnderWrongIds, 4);
    }

    #[test]
    fn a_get_from_before_the_last_entry_is_caught() {
        assert_caught(Mutant::StaleGet, 4);
    }
}
