//! Differential property tests: the state machine against the reference model (PLAN.md
//! §10.14 use 2, T-7; W10.3). Seeded random command sequences go into both; every apply
//! result (request ID included), every GET through the state machine's own read path, the
//! applied index, live-state counters and committed command bytes after every step, and the
//! final map (in the state machine's own iteration order) and request history must match.
//! PUT and GET are enforced now; DELETE and compare-and-swap (W27) and SCAN (W28) are
//! registered `planned` (DIFFERENTIAL-DELETE-CAS, DIFFERENTIAL-SCAN) until the state machine
//! implements them.
//!
//! This is an integration test of `sim`, the one crate allowed to depend on both a domain
//! crate and an oracle (DEP-RULE): Cargo builds it as its own test crate (§10.14 "a separate
//! test crate"; owner decision on br-w10-3-differential-tests-6ez).
//!
//! Hand-rolled rather than proptest, as for W09.4's property tests:
//! - a `SplitMix64` stream drives generation, and the model follows each log as it is
//!   generated, so the generator can make room for a refused PUT and then replay it;
//! - a panic on either side is a divergence too, so it keeps its seed and log;
//! - a delta-debugging shrinker reduces a failing log;
//! - a failure names its seed, the comparison that caught it, the shrunk log and its
//!   divergence, and the exact re-run command;
//! - seeds that once failed go in [`REGRESSION_SEEDS`] with the fix and run first (W01.6).
//!
//! A deliberately broken state machine must fail it (W10's acceptance). The broken variants
//! are test-local mutants around the real state machine, since no bug-variant switch exists
//! yet (§10.6; owner decision on the bead). Each is asserted caught by a named comparison, so
//! every comparison is shown to bite (br-lamf):
//! - by an apply result: a key or byte quota one too loose, a version off by one, a replay
//!   reported as an execution, and a replayed refusal re-executed once it would fit;
//! - by a GET: an answer from the state before the last entry;
//! - by the counters: live bytes not reclaimed on an overwrite, and replays counted in the
//!   committed command bytes;
//! - by the final map: a stored version off by one (in GET and the map, not in results), and
//!   the map iterated in reverse;
//! - by the stored results: a first index off by one, and history kept under the wrong
//!   request IDs (it shrinks to one PUT, whose stored result is then missing);
//! - by the unknown-ID check: a request ID no command carried.
//!
//! The committed-command-bytes counter (DM-7) is checked after every step: the model decides
//! which entries count (first executions, refusals included; never replays, conflicts or empty
//! entries), and the length added is the production codec's own, since the model has no byte
//! accounting until br-camk.
#![cfg(test)]

mod differential {
    // check: DIFFERENTIAL
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::LazyLock;

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
    /// URL-hostile characters, and two keys at the 256-byte maximum, one ASCII and one of
    /// two-byte characters (§10.3). Built once: [`values`] holds a 64 KiB string.
    static KEYS: LazyLock<[String; 8]> = LazyLock::new(|| {
        [
            "a".to_owned(),
            "aa".to_owned(),
            "b".to_owned(),
            "\u{e9}".to_owned(),
            "e\u{301}".to_owned(),
            "k/?&%".to_owned(),
            "x".repeat(256),
            "\u{e9}".repeat(128),
        ]
    });

    fn keys() -> &'static [String; 8] {
        &KEYS
    }

    /// The indices of the 256-byte keys in [`keys`].
    const LONG_KEYS: [u8; 2] = [6, 7];

    /// The index of the 256-byte key of two-byte characters in [`keys`].
    const MULTIBYTE_LONG_KEY: u8 = 7;

    /// Values: empty, short, multibyte, longer, and one at DM-2's 65,536-byte maximum (§10.3).
    static VALUES: LazyLock<[String; 6]> = LazyLock::new(|| {
        [
            String::new(),
            "v".to_owned(),
            "w".to_owned(),
            "\u{65e5}\u{672c}\u{8a9e}".to_owned(),
            "y".repeat(40),
            "z".repeat(65_536),
        ]
    });

    fn values() -> &'static [String; 6] {
        &VALUES
    }

    /// The index of the empty value in [`values`].
    const EMPTY_VALUE: u8 = 0;

    /// The index of the 65,536-byte value in [`values`].
    const MAX_VALUE: u8 = 5;

    /// Live-state quotas `(max_live_keys, max_live_bytes)`, mostly tight so that DM-6
    /// refusals of both kinds are common, and one wide enough for two maximum-size values.
    /// In every profile, both the byte quota and one byte more are totals the keys and values
    /// can reach, so a byte quota one too loose is observable whichever profile a log draws.
    const PROFILES: [(u64, u64); 5] = [(3, 520), (3, 4), (2, 300), (1, 11), (4, 131_585)];

    /// Whether DM-6 admits a write that leaves its key holding `grown` bytes (key and value),
    /// where the key holds `held` bytes now (`None` if absent), under `quota` and the live
    /// `(keys, bytes)`: it must not create a key beyond `max_live_keys` nor take the live
    /// bytes beyond `max_live_bytes` (§7.5 step 4). A plain `<= max_live_bytes` matches the
    /// plan's "grow bytes beyond" because the live bytes never exceed the quota. A live total
    /// below what the key holds (a broken counter) admits nothing.
    fn admits(quota: (u64, u64), live: (u64, u64), held: Option<u64>, grown: u64) -> bool {
        let ((max_keys, max_bytes), (live_keys, live_bytes)) = (quota, live);
        (held.is_some() || live_keys < max_keys)
            && live_bytes
                .checked_sub(held.unwrap_or(0))
                .is_some_and(|rest| rest + grown <= max_bytes)
    }

    /// The bytes a key and value take together (DM-6).
    fn entry_bytes(key: u8, value: u8) -> u64 {
        u64::try_from(keys()[usize::from(key)].len() + values()[usize::from(value)].len()).unwrap()
    }

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

    /// The model as a log is generated: it applies each entry, and remembers the PUTs whose
    /// first execution was a byte-quota refusal, so the generator can replay one after making
    /// room. (A key-quota refusal never fits again: with PUT only, no key is ever removed.)
    struct Follower {
        profile: usize,
        model: model::Model,
        refused: Vec<Step>,
    }

    impl Follower {
        fn new(profile: usize) -> Self {
            Self {
                profile,
                model: model::Model::new(model_limits(profile)),
                refused: Vec::new(),
            }
        }

        /// The refused PUTs that would fit once every live value is emptied.
        fn fitting(&self) -> Vec<Step> {
            let key_bytes: usize = self
                .model
                .entries()
                .map(|(key, _, _)| key.as_str().len())
                .sum();
            let live = (self.model.live_keys(), u64::try_from(key_bytes).unwrap());
            self.refused
                .iter()
                .copied()
                .filter(|step| {
                    let Step::Put { key, value, .. } = *step else {
                        return false;
                    };
                    let held = self
                        .model
                        .get(&model_key(key))
                        .unwrap()
                        .map(|_| entry_bytes(key, EMPTY_VALUE));
                    admits(PROFILES[self.profile], live, held, entry_bytes(key, value))
                })
                .collect()
        }

        fn follow(&mut self, step: Step) {
            if let Step::Get { .. } = step {
                return;
            }
            let index = model::Index::new(self.model.applied_index() + 1).unwrap();
            let applied = self
                .model
                .apply(index, &model_entry(step))
                .unwrap_or_else(|error| {
                    panic!("test setup: the generator's model applies {step:?}: {error}")
                });
            if let model::Applied::Executed(result) = applied
                && result.decision
                    == (model::Decision::CapacityExceeded {
                        limit: model::LiveLimit::MaxLiveBytes,
                    })
            {
                self.refused.push(step);
            }
        }

        /// PUTs, under fresh request numbers from `fresh`, emptying every live value that is
        /// not empty: a shrinking overwrite of each key, in key order.
        fn make_room(&self, fresh: &mut u16) -> Vec<Step> {
            let all = keys();
            self.model
                .entries()
                .filter(|(_, value, _)| !value.as_str().is_empty())
                .map(|(key, _, _)| {
                    *fresh += 1;
                    Step::Put {
                        request: *fresh,
                        key: u8::try_from(all.iter().position(|k| k == key.as_str()).unwrap())
                            .unwrap(),
                        value: EMPTY_VALUE,
                    }
                })
                .collect()
        }
    }

    /// The log for `seed`: empty entries, GETs, exact repeats of an earlier PUT (replays),
    /// earlier request IDs with another command (conflicts), PUTs under fresh request IDs
    /// (first executions: overwrites and quota refusals), and, once a PUT was refused,
    /// shrinking overwrites followed by that PUT again (a replayed refusal that may now
    /// fit), so logs stay deep.
    fn generate(seed: u64) -> Log {
        let mut rng = SplitMix64(seed);
        let profile = rng.below(PROFILES.len());
        let mut steps: Vec<Step> = Vec::with_capacity(STEPS);
        let mut follower = Follower::new(profile);
        let mut fresh: u16 = 0;
        while steps.len() < STEPS {
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
            let fitting = follower.fitting();
            let next = if roll < 8 {
                vec![Step::Empty]
            } else if roll < 28 {
                vec![Step::Get { key }]
            } else if roll < 38 && !earlier.is_empty() {
                vec![earlier[rng.below(earlier.len())]]
            } else if roll < 46 && !used.is_empty() {
                vec![Step::Put {
                    request: used[rng.below(used.len())],
                    key,
                    value,
                }]
            } else if roll < 56 && !fitting.is_empty() {
                let replay = fitting[rng.below(fitting.len())];
                let mut room = follower.make_room(&mut fresh);
                room.push(replay);
                room
            } else {
                fresh += 1;
                vec![Step::Put {
                    request: fresh,
                    key,
                    value,
                }]
            };
            for step in next.into_iter().take(STEPS - steps.len()) {
                follower.follow(step);
                steps.push(step);
            }
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

    /// The state machine's command for a PUT step, under request ID `sent`.
    fn command(key: u8, value: u8, sent: [u8; 16]) -> Command {
        Command {
            request_id: RequestId::from_bytes(sent),
            mutation: Mutation::Put {
                key: Key::parse(keys()[usize::from(key)].clone(), KeyValueLimits::MAXIMA).unwrap(),
                value: Value::parse(values()[usize::from(value)].clone(), KeyValueLimits::MAXIMA)
                    .unwrap(),
            },
        }
    }

    /// The canonical encoded length of a PUT step's command (the production codec's), as the
    /// committed-command-bytes counter adds it on a first execution (§7.5, DM-7).
    fn encoded_len(step: Step) -> u64 {
        match step {
            Step::Put {
                request,
                key,
                value,
            } => u64::try_from(codec::encode(&command(key, value, request_bytes(request))).len())
                .unwrap(),
            Step::Empty | Step::Get { .. } => 0,
        }
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

    /// The comparison that found a divergence.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Path {
        /// An entry's apply result.
        Apply,
        /// A GET between entries.
        Get,
        /// The applied index, live keys, live bytes and committed command bytes after a step.
        Counters,
        /// The final map, in iteration order.
        Map,
        /// A request's stored result in the final history.
        History,
        /// A request ID in the final history that no command carried.
        UnknownIds,
        /// A panic on either side.
        Panic,
    }

    /// The first point where the two implementations disagree, or where one panicked.
    #[derive(Debug)]
    struct Divergence {
        step: usize,
        path: Path,
        what: String,
    }

    impl fmt::Display for Divergence {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at step {} ({:?}): {}", self.step, self.path, self.what)
        }
    }

    /// What the model's state said about a step before it applied, to classify the outcome.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Fact {
        /// The PUT creates its key.
        NewKey,
        /// Its key and value would take the live bytes past the byte quota.
        OverBytes,
        /// It would leave the live bytes exactly one byte over the byte quota.
        OneByteOver,
        /// Its request ID's first execution was a quota refusal.
        OriginalRefused,
        /// It repeats a refused command that the quotas would now admit.
        RefusalNowFits,
        /// After it, the live bytes sit exactly at the byte quota.
        AtQuotaAfter,
        /// It writes a 256-byte key.
        LongKey,
        /// It writes the 256-byte key of two-byte characters.
        MultibyteLongKey,
        /// It writes the empty value.
        EmptyValue,
        /// It writes the 65,536-byte value.
        MaxValue,
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
        multibyte_long_key_set: u64,
        empty_value_set: u64,
        max_value_set: u64,
        refused_keys: u64,
        refused_bytes: u64,
        refused_bytes_by_one: u64,
        refused_both: u64,
        replayed: u64,
        replayed_refusal: u64,
        replayed_refusal_fits: u64,
        conflict: u64,
        conflict_with_refusal: u64,
        get_hit: u64,
        get_miss: u64,
    }

    /// The floor of each coverage count over [`SEEDS`] seeds, by name: roughly a quarter of
    /// what the generator reached when the floor was set (the measured count beside it), so
    /// a generator change that starves an outcome fails (br-lamf).
    const FLOORS: [(&str, u64); 19] = [
        ("empty", 450),                 // measured 1901
        ("set", 1000),                  // measured 4041
        ("overwrite", 750),             // measured 3087
        ("set_at_quota", 45),           // measured 196
        ("long_key_set", 180),          // measured 760
        ("multibyte_long_key_set", 90), // measured 373
        ("empty_value_set", 250),       // measured 1035
        ("max_value_set", 55),          // measured 233
        ("refused_keys", 1750),         // measured 7081
        ("refused_bytes", 650),         // measured 2735
        ("refused_bytes_by_one", 50),   // measured 203
        ("refused_both", 700),          // measured 2934
        ("replayed", 600),              // measured 2498
        ("replayed_refusal", 400),      // measured 1637
        ("replayed_refusal_fits", 75),  // measured 304
        ("conflict", 550),              // measured 2243
        ("conflict_with_refusal", 330), // measured 1323
        ("get_hit", 350),               // measured 1401
        ("get_miss", 900),              // measured 3700
    ];

    impl Coverage {
        fn count(&mut self, outcome: Outcome, context: &Context) {
            let holds = |fact: Fact| u64::from(context.contains(&fact));
            match outcome {
                Outcome::Empty => self.empty += 1,
                Outcome::Executed(_, _, Dec::Set(_)) => {
                    self.set += 1;
                    self.overwrite += 1 - holds(Fact::NewKey);
                    self.set_at_quota += holds(Fact::AtQuotaAfter);
                    self.long_key_set += holds(Fact::LongKey);
                    self.multibyte_long_key_set += holds(Fact::MultibyteLongKey);
                    self.empty_value_set += holds(Fact::EmptyValue);
                    self.max_value_set += holds(Fact::MaxValue);
                }
                Outcome::Executed(_, _, Dec::Capacity(limit)) => {
                    if limit == "max_live_keys" {
                        self.refused_keys += 1;
                        self.refused_both += holds(Fact::OverBytes);
                    } else {
                        self.refused_bytes += 1;
                        self.refused_bytes_by_one += holds(Fact::OneByteOver);
                    }
                }
                Outcome::Executed(_, _, Dec::Deleted(_) | Dec::Mismatch(_)) => {}
                Outcome::Replayed(_, _, decision) => {
                    self.replayed += 1;
                    if let Dec::Capacity(_) = decision {
                        self.replayed_refusal += 1;
                        self.replayed_refusal_fits += holds(Fact::RefusalNowFits);
                    }
                }
                Outcome::Conflict(..) => {
                    self.conflict += 1;
                    self.conflict_with_refusal += holds(Fact::OriginalRefused);
                }
            }
        }

        const fn add(&mut self, other: Self) {
            self.empty += other.empty;
            self.set += other.set;
            self.overwrite += other.overwrite;
            self.set_at_quota += other.set_at_quota;
            self.long_key_set += other.long_key_set;
            self.multibyte_long_key_set += other.multibyte_long_key_set;
            self.empty_value_set += other.empty_value_set;
            self.max_value_set += other.max_value_set;
            self.refused_keys += other.refused_keys;
            self.refused_bytes += other.refused_bytes;
            self.refused_bytes_by_one += other.refused_bytes_by_one;
            self.refused_both += other.refused_both;
            self.replayed += other.replayed;
            self.replayed_refusal += other.replayed_refusal;
            self.replayed_refusal_fits += other.replayed_refusal_fits;
            self.conflict += other.conflict;
            self.conflict_with_refusal += other.conflict_with_refusal;
            self.get_hit += other.get_hit;
            self.get_miss += other.get_miss;
        }

        /// Each count with its name.
        const fn named(self) -> [(&'static str, u64); 19] {
            [
                ("empty", self.empty),
                ("set", self.set),
                ("overwrite", self.overwrite),
                ("set_at_quota", self.set_at_quota),
                ("long_key_set", self.long_key_set),
                ("multibyte_long_key_set", self.multibyte_long_key_set),
                ("empty_value_set", self.empty_value_set),
                ("max_value_set", self.max_value_set),
                ("refused_keys", self.refused_keys),
                ("refused_bytes", self.refused_bytes),
                ("refused_bytes_by_one", self.refused_bytes_by_one),
                ("refused_both", self.refused_both),
                ("replayed", self.replayed),
                ("replayed_refusal", self.replayed_refusal),
                ("replayed_refusal_fits", self.replayed_refusal_fits),
                ("conflict", self.conflict),
                ("conflict_with_refusal", self.conflict_with_refusal),
                ("get_hit", self.get_hit),
                ("get_miss", self.get_miss),
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
        RefusalReExecuted,
        StaleGet,
        BytesNotReclaimed,
        ReplayCountsBytes,
        StoredVersionOffByOne,
        ReversedMap,
        StoredResultOffByOne,
        PhantomHistoryId,
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
        /// The real quotas `(max_live_keys, max_live_bytes)`, which a re-executed refusal
        /// checks.
        quota: (u64, u64),
        /// The live state before the last entry, which a stale GET answers from.
        before: BTreeMap<String, (String, u64)>,
        /// Wrong request IDs handed out so far by a machine that keeps history under them.
        wrong: u16,
        /// Overwritten bytes a machine that does not reclaim them still counts as live.
        leaked: u64,
        /// Replayed command bytes a machine that counts replays adds to the committed bytes.
        replayed_bytes: u64,
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
                quota: (keys, bytes),
                before: BTreeMap::new(),
                wrong: 0,
                leaked: 0,
                replayed_bytes: 0,
            }
        }

        /// What a mutant adds to the versions an apply result reports.
        fn result_shift(&self) -> u64 {
            u64::from(self.mutant == Mutant::VersionOffByOne)
        }

        /// What a mutant adds to the stored versions GET and the map report.
        fn stored_shift(&self) -> u64 {
            u64::from(
                [Mutant::VersionOffByOne, Mutant::StoredVersionOffByOne].contains(&self.mutant),
            )
        }

        /// The live bytes key `key` holds now (its key and value), if it is live.
        fn holding(&self, key: u8) -> Option<u64> {
            let text = &keys()[usize::from(key)];
            let parsed = Key::parse(text.clone(), KeyValueLimits::MAXIMA).unwrap();
            self.state
                .get(&parsed)
                .map(|stored| u64::try_from(text.len() + stored.value().as_str().len()).unwrap())
        }

        /// Whether the real quotas would now admit setting `key` to `value` (DM-6).
        fn fits(&self, key: u8, value: u8) -> bool {
            let live = (self.state.live_keys().get(), self.state.live_bytes().get());
            admits(self.quota, live, self.holding(key), entry_bytes(key, value))
        }

        /// Applies one entry. The state machine must echo the request ID it was sent.
        fn apply(&mut self, index: u64, step: Step) -> Result<Outcome, String> {
            if self.mutant == Mutant::StaleGet {
                self.before = self.map().into_iter().collect();
            }
            let (payload, sent, before) = match step {
                Step::Put {
                    request,
                    key,
                    value,
                } => {
                    let sent = self.request(request);
                    // Only the mutants that use them read the state before the entry.
                    let held = (self.mutant == Mutant::BytesNotReclaimed)
                        .then(|| self.holding(key))
                        .flatten();
                    let fits = self.mutant == Mutant::RefusalReExecuted && self.fits(key, value);
                    let before = (held, fits);
                    (
                        EntryPayload::Command(codec::encode(&command(key, value, sent))),
                        Some((sent, request)),
                        before,
                    )
                }
                Step::Empty | Step::Get { .. } => (EntryPayload::Empty, None, (None, false)),
            };
            let entry = Entry {
                index: EntryIndex::new(index).unwrap(),
                term: Term::new(1).unwrap(),
                payload,
            };
            let applied = self.state.apply(&entry, entry.index.index());
            let (held, fits) = before;
            match &applied {
                Applied::Executed { result, .. } if self.mutant == Mutant::BytesNotReclaimed => {
                    if result.decision() == Decision::Set {
                        self.leaked += held.unwrap_or(0);
                    }
                }
                Applied::Replayed { .. } if self.mutant == Mutant::ReplayCountsBytes => {
                    self.replayed_bytes += encoded_len(step);
                }
                Applied::Empty
                | Applied::Executed { .. }
                | Applied::Replayed { .. }
                | Applied::Conflict { .. } => {}
            }
            self.outcome(&applied, sent, index, fits)
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

        /// The outcome the machine reports for entry `index`; `fits` says whether the
        /// quotas would have admitted its command just before it.
        fn outcome(
            &self,
            applied: &Applied,
            sent: Option<([u8; 16], u16)>,
            index: u64,
            fits: bool,
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
                Dec::Set(version) => Dec::Set(version + self.result_shift()),
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
                    match (self.mutant, replayed.2) {
                        (Mutant::ReplayAsExecution, _) => {
                            Outcome::Executed(replayed.0, replayed.1, replayed.2)
                        }
                        // A refusal executed again once it would fit, as if DM-6 refusals
                        // were not terminal.
                        (Mutant::RefusalReExecuted, Dec::Capacity(_)) if fits => {
                            Outcome::Executed(replayed.0, index, Dec::Set(index))
                        }
                        _ => Outcome::Replayed(replayed.0, replayed.1, replayed.2),
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
            let mut map: Vec<(String, (String, u64))> = self
                .state
                .iter()
                .map(|(key, stored)| {
                    let version = stored.version().get() + self.stored_shift();
                    (
                        key.as_str().to_owned(),
                        (stored.value().as_str().to_owned(), version),
                    )
                })
                .collect();
            if self.mutant == Mutant::ReversedMap {
                map.reverse();
            }
            map
        }

        /// A GET through the state machine's own read path (F-2).
        fn get(&self, key: &str) -> Option<(String, u64)> {
            if self.mutant == Mutant::StaleGet {
                return self.before.get(key).cloned();
            }
            let key = Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap();
            self.state.get(&key).map(|stored| {
                let version = stored.version().get() + self.stored_shift();
                (stored.value().as_str().to_owned(), version)
            })
        }

        /// The applied index, live keys, live bytes and committed command bytes.
        fn counters(&self) -> (u64, u64, u64, u64) {
            (
                self.state.applied_index().get(),
                self.state.live_keys().get(),
                self.state.live_bytes().get() + self.leaked,
                self.state.committed_command_bytes().get() + self.replayed_bytes,
            )
        }

        /// The stored result of request number `request`: its first execution's index and
        /// decision.
        fn status(&self, request: u16) -> Option<(u64, Dec)> {
            let id = RequestId::from_bytes(request_bytes(request));
            let shift = u64::from(self.mutant == Mutant::StoredResultOffByOne);
            self.state.history().get(&id).map(|record| {
                (
                    record.first_index().get() + shift,
                    decision(record.result()),
                )
            })
        }

        /// The request IDs in the state machine's history that no command of the log carried.
        fn unknown_ids(&self, requests: &BTreeSet<u16>) -> Vec<[u8; 16]> {
            // A machine with a phantom record has one more ID in its history, which goes
            // through the same filter as the real ones.
            let phantom = (self.mutant == Mutant::PhantomHistoryId
                && !self.state.history().is_empty())
            .then(|| request_bytes(WRONG_IDS));
            self.state
                .history()
                .iter()
                .map(|(request_id, _)| request_id.to_bytes())
                .chain(phantom)
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

    fn model_command(request: u16, key: u8, value: u8) -> model::Command {
        model::Command {
            request_id: model::RequestId::from_bytes(request_bytes(request)),
            op: model::Op::Put {
                key: model_key(key),
                value: model::Value::new(values()[usize::from(value)].clone()).unwrap(),
            },
        }
    }

    fn model_entry(step: Step) -> model::Entry {
        match step {
            Step::Put {
                request,
                key,
                value,
            } => model::Entry::Command(model_command(request, key, value)),
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
        let grown = entry_bytes(key, value);
        let held = model.get(&model_key(key)).unwrap().map(|(value, _)| {
            u64::try_from(keys()[usize::from(key)].len() + value.as_str().len()).unwrap()
        });
        let live = (model.live_keys(), model.live_bytes());
        let fits = admits(PROFILES[profile], live, held, grown);
        // The live bytes the write would leave, which a byte refusal exceeds the quota by.
        let after = model.live_bytes() - held.unwrap_or(0) + grown;
        let original = model.status(&model::RequestId::from_bytes(request_bytes(request)));
        let refused = original.is_some_and(|result| {
            matches!(result.decision, model::Decision::CapacityExceeded { .. })
        });
        let replay = matches!(
            model.classify(&model_command(request, key, value)),
            Some(model::Applied::Replayed(_))
        );
        let facts = [
            (Fact::NewKey, held.is_none()),
            (Fact::OverBytes, model.live_bytes() + grown > max_bytes),
            (Fact::OneByteOver, after == max_bytes + 1),
            (Fact::OriginalRefused, refused),
            (Fact::RefusalNowFits, refused && replay && fits),
            (Fact::LongKey, LONG_KEYS.contains(&key)),
            (Fact::MultibyteLongKey, key == MULTIBYTE_LONG_KEY),
            (Fact::EmptyValue, value == EMPTY_VALUE),
            (Fact::MaxValue, value == MAX_VALUE),
        ];
        facts
            .into_iter()
            .filter_map(|(fact, holds)| holds.then_some(fact))
            .collect()
    }

    /// A key or value as a failure shows it: whole up to 48 bytes, otherwise its first 16
    /// characters and its length, so a 64 KiB value cannot flood a bounded log (W01.6).
    fn brief(text: &str) -> String {
        if text.len() <= 48 {
            format!("{text:?}")
        } else {
            let prefix: String = text.chars().take(16).collect();
            format!("{prefix:?}... ({} B)", text.len())
        }
    }

    fn brief_entry(entry: Option<&(String, u64)>) -> String {
        entry.map_or_else(
            || "absent".to_owned(),
            |(value, version)| format!("{} at version {version}", brief(value)),
        )
    }

    fn brief_map(map: &[(String, (String, u64))]) -> String {
        map.iter()
            .map(|(key, (value, version))| format!("{} = {} @{version}", brief(key), brief(value)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The exact command that re-runs the current test: libtest names each test's thread after
    /// the test.
    fn rerun() -> String {
        format!(
            "./dev check DIFFERENTIAL --case {}",
            std::thread::current().name().unwrap_or("differential")
        )
    }

    fn diverged<T>(step: usize, path: Path, what: String) -> Result<T, Divergence> {
        Err(Divergence { step, path, what })
    }

    /// One entry into both; the results must be equal. Returns the model's outcome.
    fn compare_apply(
        at: usize,
        index: u64,
        step: Step,
        log: &Log,
        machine: &mut Machine,
        model: &mut model::Model,
        coverage: &mut Coverage,
    ) -> Result<Outcome, Divergence> {
        let mut context = context_before(step, log.profile, model);
        let index_of = model::Index::new(index).unwrap();
        let expected = match model.apply(index_of, &model_entry(step)) {
            Ok(applied) => model_outcome(applied, &log.requests()),
            Err(error) => {
                return diverged(
                    at,
                    Path::Apply,
                    format!("the model refused {step:?}: {error}"),
                );
            }
        };
        if model.live_bytes() == PROFILES[log.profile].1 {
            context.insert(Fact::AtQuotaAfter);
        }
        coverage.count(expected, &context);
        match machine.apply(index, step) {
            Ok(observed) if observed == expected => Ok(expected),
            Ok(observed) => diverged(
                at,
                Path::Apply,
                format!(
                    "entry {index} {step:?}: the state machine says {observed:?}, the model \
                     {expected:?}"
                ),
            ),
            Err(what) => diverged(at, Path::Apply, format!("entry {index} {step:?}: {what}")),
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
                Path::Get,
                format!(
                    "GET key {key}: the state machine says {}, the model {}",
                    brief_entry(observed.as_ref()),
                    brief_entry(expected.as_ref())
                ),
            )
        }
    }

    /// The applied index and the live-state counters after a step (§7.5 step 5, DM-6), and
    /// the committed command bytes: the codec's length of every command the model executed
    /// for the first time, and nothing for replays, conflicts or empty entries (DM-7).
    fn compare_counters(
        at: usize,
        machine: &Machine,
        model: &model::Model,
        command_bytes: u64,
    ) -> Result<(), Divergence> {
        let observed = machine.counters();
        let expected = (
            model.applied_index(),
            model.live_keys(),
            model.live_bytes(),
            command_bytes,
        );
        if observed == expected {
            Ok(())
        } else {
            diverged(
                at,
                Path::Counters,
                format!(
                    "(applied index, live keys, live bytes, committed command bytes): the state \
                     machine says {observed:?}, the model and the codec {expected:?}"
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
                Path::Map,
                format!(
                    "final map: the state machine has [{}], the model [{}]",
                    brief_map(&observed),
                    brief_map(&expected)
                ),
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
                    Path::History,
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
                Path::UnknownIds,
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
        let mut command_bytes = 0;
        for (at, step) in log.steps.iter().copied().enumerate() {
            if let Step::Get { key } = step {
                compare_get(at, key, &machine, &model, &mut coverage)?;
            } else {
                index += 1;
                let expected = compare_apply(
                    at,
                    index,
                    step,
                    log,
                    &mut machine,
                    &mut model,
                    &mut coverage,
                )?;
                if let Outcome::Executed(..) = expected {
                    command_bytes += encoded_len(step);
                }
            }
            compare_counters(at, &machine, &model, command_bytes)?;
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
                path: Path::Panic,
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
             mutant {mutant:?} survived seeds 0..{SEEDS}; re-run: {}",
            rerun()
        )
    }

    /// `mutant` is caught by the comparison `path`, and its shrunk log is small enough to
    /// read: a comparison that stopped comparing would let its mutant through (br-lamf).
    fn assert_caught(mutant: Mutant, path: Path, most_steps: usize) {
        let (seed, shrunk, divergence) = caught(mutant);
        assert!(
            divergence.path == path && !shrunk.steps.is_empty() && shrunk.steps.len() <= most_steps,
            "rule: a broken state machine is caught by its own comparison, {path:?}, and shrinks \
             to at most {most_steps} steps; mutant {mutant:?} seed {seed}: {divergence}; shrunk \
             to {} steps {:?} (profile {}); re-run: {}",
            shrunk.steps.len(),
            shrunk.steps,
            shrunk.profile,
            rerun()
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
        let short: Vec<(&str, u64, u64)> = total
            .named()
            .into_iter()
            .map(|(name, count)| {
                let floor = FLOORS
                    .iter()
                    .find(|(floored, _)| *floored == name)
                    .map_or_else(
                        || panic!("test setup: coverage count {name} has no floor"),
                        |(_, floor)| *floor,
                    );
                (name, count, floor)
            })
            .filter(|(_, count, floor)| count < floor)
            .collect();
        assert_eq!(
            FLOORS.len(),
            total.named().len(),
            "test setup: every floor names a coverage count"
        );
        assert!(
            short.is_empty(),
            "rule: the generated logs reach empty entries; sets, overwrites, a set landing on \
             the byte quota, both 256-byte keys, the empty and the 65,536-byte value; key, byte \
             and both-limit refusals, byte refusals one byte over the quota too; replays, of \
             refusals too, and of refusals that would now fit; conflicts, with refused originals too; and GET hits and misses, each at least \
             its floor, so a pass is not vacuous (P10); below floor (name, count, floor): \
             {short:?}; coverage={total:?}; re-run: {RERUN_COVERAGE}"
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
        assert_caught(Mutant::LooseKeyQuota, Path::Apply, 8);
    }

    #[test]
    fn a_byte_quota_one_too_loose_is_caught() {
        assert_caught(Mutant::LooseByteQuota, Path::Apply, 8);
    }

    #[test]
    fn a_version_off_by_one_is_caught() {
        assert_caught(Mutant::VersionOffByOne, Path::Apply, 1);
    }

    #[test]
    fn a_replay_reported_as_an_execution_is_caught() {
        assert_caught(Mutant::ReplayAsExecution, Path::Apply, 4);
    }

    #[test]
    fn history_under_the_wrong_request_ids_is_caught() {
        assert_caught(Mutant::HistoryUnderWrongIds, Path::History, 1);
    }

    #[test]
    fn a_refusal_re_executed_once_it_fits_is_caught() {
        assert_caught(Mutant::RefusalReExecuted, Path::Apply, 8);
    }

    #[test]
    fn a_get_from_before_the_last_entry_is_caught() {
        assert_caught(Mutant::StaleGet, Path::Get, 4);
    }

    #[test]
    fn live_bytes_not_reclaimed_on_an_overwrite_are_caught() {
        assert_caught(Mutant::BytesNotReclaimed, Path::Counters, 4);
    }

    #[test]
    fn replays_counted_in_the_committed_command_bytes_are_caught() {
        assert_caught(Mutant::ReplayCountsBytes, Path::Counters, 4);
    }

    #[test]
    fn a_stored_version_off_by_one_is_caught() {
        assert_caught(Mutant::StoredVersionOffByOne, Path::Map, 1);
    }

    #[test]
    fn a_map_iterated_in_reverse_is_caught() {
        assert_caught(Mutant::ReversedMap, Path::Map, 2);
    }

    #[test]
    fn a_stored_result_off_by_one_is_caught() {
        assert_caught(Mutant::StoredResultOffByOne, Path::History, 1);
    }

    #[test]
    fn a_request_id_no_command_carried_is_caught() {
        assert_caught(Mutant::PhantomHistoryId, Path::UnknownIds, 1);
    }
}
