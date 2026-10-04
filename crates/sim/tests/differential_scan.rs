//! Differential tests of SCAN: the state machine's pages against the reference model's
//! (PLAN.md §10.14 use 2, T-7, F-4, §8.5, §7.11; W28.2). The accumulation rule is written
//! three times on purpose (T-12): in the core, in the model and in K-5. This test holds the
//! first two to each other on seeded random logs:
//! - writes keep both states in step: PUTs (many of them 60 to 65 KB, so the 1 MiB byte cap
//!   decides pages), DELETEs and empty entries, each under a fresh request ID, with both
//!   sides' results compared;
//! - every SCAN step asks both sides the same request: random bounds (keys, and strings
//!   between keys), an optional `after`, limits including the invalid 0 and 129, and
//!   sometimes `start > end`. Refusals must agree, and pages must agree item for item (key,
//!   value, version), with `has_more` and `next_after`;
//! - some SCAN steps continue the traversal with `after = next_after` to its end, comparing
//!   every page.
//!
//! This is an integration test of `sim`, the one crate allowed to depend on both a domain
//! crate and an oracle (DEP-RULE), as W10.3's `differential.rs` is. Hand-rolled like it: a
//! `SplitMix64` stream drives generation, a panic on either side is a divergence, a failure
//! shrinks its log and names its seed, the comparison that caught it and the re-run command,
//! and seeds that once failed go in [`REGRESSION_SEEDS`] (W01.6).
//!
//! Test-local mutants around the real state machine's answer show each comparison bites:
//! a page one item short, a lost `has_more`, a `next_after` that is not the last returned
//! key, a page from the state before the last entry, a version off by one, and a limit one
//! too loose.
#![cfg(test)]

mod differential_scan {
    // check: DIFFERENTIAL-SCAN
    use std::fmt;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::LazyLock;

    use statemachine::{Applied, Command, StateLimits, StateMachine, codec};
    use types::{
        ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits,
        LogIndex, MAX_SAFE_INTEGER, Mutation, NodeId, PageSize, RequestId, ScanRange, ServedBy,
        Term, Value,
    };

    /// Seeds that once failed, replayed first on every run. Add the seed a failure names,
    /// with the shrunk log it printed as a comment, in the commit that fixes it.
    const REGRESSION_SEEDS: &[u64] = &[];

    /// Random seeds per run, after the regression seeds.
    const SEEDS: u64 = 40;

    /// Steps per generated log.
    const STEPS: usize = 96;

    /// The re-run command a failure of the main property prints.
    const RERUN: &str = "./dev check DIFFERENTIAL-SCAN --case \
                         differential_scan::scan_pages_match_the_model_on_seeded_logs";

    /// Keys: seven stems (ASCII, decomposed and precomposed é, three- and four-byte
    /// characters, URL-special characters), each alone and with a digit, and two 256-byte
    /// keys. 44 keys, so 16 or more large values can share a range.
    static KEYS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let stems = [
            "a",
            "b",
            "e\u{301}",
            "\u{e9}",
            "\u{20ac}",
            "\u{1f600}",
            "k/?&%",
        ];
        let mut keys: Vec<String> = stems
            .iter()
            .flat_map(|stem| ["", "0", "1", "2", "3", "4"].map(|digit| format!("{stem}{digit}")))
            .collect();
        keys.push("x".repeat(256));
        keys.push("\u{e9}".repeat(128));
        keys
    });

    /// Bounds: every key, and strings no key equals, so bounds also fall between keys.
    static BOUNDS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let mut bounds = KEYS.clone();
        for between in ["aa", "c", "e", "\u{100}", "\u{ff5e}", "\u{10ffff}", "k"] {
            bounds.push(between.to_owned());
        }
        bounds
    });

    /// Values: empty, short, multibyte, longer, 60,000 bytes of four-byte characters, and
    /// DM-2's 65,536-byte maximum.
    static VALUES: LazyLock<[String; 6]> = LazyLock::new(|| {
        [
            String::new(),
            "v".to_owned(),
            "\u{65e5}\u{672c}\u{8a9e}".to_owned(),
            "y".repeat(40),
            "\u{1f600}".repeat(15_000),
            "z".repeat(65_536),
        ]
    });

    /// Value indices from here on are boundary fillers, never generated: `FILLER + n` is a
    /// value of `n` bytes, so a designed log can make a page hold exactly N ± 1 bytes.
    const FILLER: usize = 1 << 20;

    /// The value a step's index names: one of [`VALUES`], or a boundary filler.
    fn value_text(value: usize) -> String {
        VALUES
            .get(value)
            .cloned()
            .unwrap_or_else(|| "f".repeat(value - FILLER))
    }

    /// Limits: the valid edges and middles, and the invalid 0 and 129. The wide 32 and 128
    /// come three times each, so many pages can reach the byte cap first.
    const LIMITS: [u64; 13] = [0, 1, 2, 3, 5, 16, 32, 32, 32, 128, 128, 128, 129];

    /// A seeded `SplitMix64` stream: this test's only randomness.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
        }

        fn maybe(&mut self, bound: usize) -> Option<usize> {
            (self.below(3) > 0).then(|| self.below(bound))
        }
    }

    /// A SCAN request, as indices into [`BOUNDS`] and a limit.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Scan {
        start: Option<usize>,
        end: Option<usize>,
        after: Option<usize>,
        limit: u64,
        /// Whether to continue the traversal to its end.
        traverse: bool,
    }

    /// One generated log step.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        /// An empty entry: a new leader's no-op.
        Empty,
        /// A PUT of `KEYS[key]` to `VALUES[value]`.
        Put { key: usize, value: usize },
        /// A DELETE of `KEYS[key]`.
        Delete { key: usize },
        /// A SCAN of both states; not an entry.
        Scan(Scan),
    }

    fn generate(seed: u64) -> Vec<Step> {
        let mut rng = SplitMix64(seed);
        (0..STEPS)
            .map(|_| match rng.below(20) {
                0 => Step::Empty,
                1..=11 => Step::Put {
                    key: rng.below(KEYS.len()),
                    value: if rng.below(4) < 3 {
                        4 + rng.below(2)
                    } else {
                        rng.below(4)
                    },
                },
                12..=13 => Step::Delete {
                    key: rng.below(KEYS.len()),
                },
                _ => {
                    let (mut start, mut end) = (rng.maybe(BOUNDS.len()), rng.maybe(BOUNDS.len()));
                    // Mostly valid ranges; one in eight keeps start > end, which both refuse.
                    if rng.below(8) > 0
                        && start.zip(end).is_some_and(|(s, e)| BOUNDS[s] > BOUNDS[e])
                    {
                        std::mem::swap(&mut start, &mut end);
                    }
                    let after = rng.maybe(BOUNDS.len()).filter(|_| rng.below(2) == 0);
                    Step::Scan(Scan {
                        start,
                        end,
                        after,
                        limit: LIMITS[rng.below(LIMITS.len())],
                        traverse: rng.below(3) == 0,
                    })
                }
            })
            .collect()
    }

    /// A request's raw parts: bounds as strings, so a continuation can carry any key.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Request {
        start: Option<String>,
        end: Option<String>,
        after: Option<String>,
        limit: u64,
    }

    impl Request {
        fn of(scan: Scan) -> Self {
            let bound = |at: Option<usize>| at.map(|at| BOUNDS[at].clone());
            Self {
                start: bound(scan.start),
                end: bound(scan.end),
                after: bound(scan.after),
                limit: scan.limit,
            }
        }

        fn continued(&self, after: String) -> Self {
            Self {
                after: Some(after),
                ..self.clone()
            }
        }
    }

    /// One side's answer, in common terms. A page's `next_after` is `Some` exactly when
    /// `has_more` (the model's flag is checked against it as it is read).
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Answer {
        /// The request is invalid (`400 invalid_request`).
        Refused,
        /// A page: `(key, value, version)` items and `next_after`.
        Page {
            items: Vec<(String, String, u64)>,
            next_after: Option<String>,
        },
    }

    fn brief(answer: &Answer) -> String {
        match answer {
            Answer::Refused => "refused".to_owned(),
            Answer::Page { items, next_after } => {
                let items: Vec<String> = items
                    .iter()
                    .map(|(key, value, version)| format!("{key:?}={}B@{version}", value.len()))
                    .collect();
                format!("[{}] next_after={next_after:?}", items.join(", "))
            }
        }
    }

    /// How a step's results compare: a write's decision, as both sides name it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Dec {
        Empty,
        Set(u64),
        Deleted(bool),
        Other(String),
    }

    /// A broken variant of the state machine's SCAN answer (test-local, as in W10.3).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mutant {
        /// The real state machine.
        None,
        /// A page of two or more items drops its last one and continues from the new last.
        PageOneShort,
        /// `has_more` is lost: no page continues.
        HasMoreLost,
        /// A continued page of two or more items names its first key as `next_after`.
        NextAfterFirst,
        /// The page comes from the state before the last applied entry.
        StaleView,
        /// Every item's version is one more than stored.
        VersionOffByOne,
        /// The page holds one more item than the limit allows (limit + 1, up to 128).
        LimitOneLoose,
    }

    /// The state machine under test, with the state before its last entry for
    /// [`Mutant::StaleView`].
    struct Machine {
        state: StateMachine,
        before: Option<StateMachine>,
        mutant: Mutant,
    }

    fn node() -> NodeId {
        NodeId::new(1).unwrap()
    }

    fn key(raw: &str) -> Key {
        Key::parse(raw.to_owned(), KeyValueLimits::MAXIMA).unwrap()
    }

    /// Wide quotas: capacity is DIFFERENTIAL's concern, pages are this test's.
    const LIVE_KEYS: u64 = 10_000;
    const LIVE_BYTES: u64 = 64 << 20;

    /// A fresh request ID for the write at `index`.
    fn request_bytes(index: u64) -> [u8; 16] {
        let mut bytes = [0x3C; 16];
        bytes[8..].copy_from_slice(&index.to_be_bytes());
        bytes
    }

    impl Machine {
        fn new(mutant: Mutant) -> Self {
            let limits = StateLimits {
                key_values: KeyValueLimits::MAXIMA,
                max_live_keys: ItemCount::new(LIVE_KEYS),
                max_live_bytes: ByteCount::new(LIVE_BYTES),
            };
            Self {
                state: StateMachine::new(node(), limits),
                before: None,
                mutant,
            }
        }

        fn apply(&mut self, index: u64, step: Step) -> Dec {
            let mutation = match step {
                Step::Put { key: k, value } => Some(Mutation::Put {
                    key: key(&KEYS[k]),
                    value: Value::parse(value_text(value), KeyValueLimits::MAXIMA).unwrap(),
                }),
                Step::Delete { key: k } => Some(Mutation::Delete { key: key(&KEYS[k]) }),
                Step::Empty | Step::Scan(_) => None,
            };
            let payload = mutation.map_or(EntryPayload::Empty, |mutation| {
                EntryPayload::Command(codec::encode(&Command {
                    request_id: RequestId::from_bytes(request_bytes(index)),
                    mutation,
                }))
            });
            if self.mutant == Mutant::StaleView {
                self.before = Some(self.state.clone());
            }
            let applied = self.state.apply(
                &Entry {
                    index: EntryIndex::new(index).unwrap(),
                    term: Term::new(1).unwrap(),
                    payload,
                },
                LogIndex::new(MAX_SAFE_INTEGER).unwrap(),
            );
            match &applied {
                Applied::Empty => Dec::Empty,
                Applied::Executed { result, .. } => match result.decision() {
                    Decision::Set => Dec::Set(index),
                    Decision::Deleted { existed } => Dec::Deleted(existed),
                    decision @ (Decision::VersionMismatch { .. }
                    | Decision::CapacityExceeded { .. }) => Dec::Other(format!("{decision:?}")),
                },
                Applied::Replayed { .. } | Applied::Conflict { .. } => {
                    Dec::Other(format!("{applied:?}"))
                }
            }
        }

        fn scan(&self, request: &Request) -> Answer {
            let loose = self.mutant == Mutant::LimitOneLoose && (1..=127).contains(&request.limit);
            let limit = if loose {
                request.limit + 1
            } else {
                request.limit
            };
            let Ok(limit) = PageSize::new(limit) else {
                return Answer::Refused;
            };
            let Ok(range) = ScanRange::new(
                request.start.as_deref().map(key),
                request.end.as_deref().map(key),
                request.after.as_deref().map(key),
                limit,
            ) else {
                return Answer::Refused;
            };
            let state = if self.mutant == Mutant::StaleView {
                self.before.as_ref().unwrap_or(&self.state)
            } else {
                &self.state
            };
            let page = state.scan(
                &range,
                ServedBy {
                    node: node(),
                    applied: state.applied_index(),
                },
            );
            let (items, next_after, _) = page.into_parts();
            let mut items: Vec<(String, String, u64)> = items
                .into_iter()
                .map(|item| {
                    (
                        item.key.into_string(),
                        item.value.into_string(),
                        item.version.get(),
                    )
                })
                .collect();
            let mut next_after = next_after.map(Key::into_string);
            match self.mutant {
                Mutant::PageOneShort => {
                    if items.len() >= 2 {
                        items.pop();
                        next_after = items.last().map(|(key, _, _)| key.clone());
                    }
                }
                Mutant::HasMoreLost => next_after = None,
                Mutant::NextAfterFirst => {
                    if items.len() >= 2 && next_after.is_some() {
                        next_after = items.first().map(|(key, _, _)| key.clone());
                    }
                }
                Mutant::VersionOffByOne => {
                    for item in &mut items {
                        item.2 += 1;
                    }
                }
                Mutant::None | Mutant::StaleView | Mutant::LimitOneLoose => {}
            }
            Answer::Page { items, next_after }
        }
    }

    /// The reference model, in the same terms.
    struct Oracle(model::Model);

    impl Oracle {
        fn new() -> Self {
            let limits = model::Limits::new(
                model::MAX_KEY_BYTES,
                model::MAX_VALUE_BYTES,
                LIVE_KEYS,
                LIVE_BYTES,
            )
            .unwrap();
            Self(model::Model::new(limits))
        }

        fn apply(&mut self, index: u64, step: Step) -> Result<Dec, String> {
            let request_id = model::RequestId::from_bytes(request_bytes(index));
            let entry = match step {
                Step::Put { key, value } => model::Entry::Command(model::Command {
                    request_id,
                    op: model::Op::Put {
                        key: model::Key::new(KEYS[key].clone()).unwrap(),
                        value: model::Value::new(value_text(value)).unwrap(),
                    },
                }),
                Step::Delete { key } => model::Entry::Command(model::Command {
                    request_id,
                    op: model::Op::Delete {
                        key: model::Key::new(KEYS[key].clone()).unwrap(),
                    },
                }),
                Step::Empty | Step::Scan(_) => model::Entry::Empty,
            };
            let applied = self
                .0
                .apply(model::Index::new(index).unwrap(), &entry)
                .map_err(|error| format!("the model refused entry {index}: {error:?}"))?;
            Ok(match &applied {
                model::Applied::Empty => Dec::Empty,
                model::Applied::Executed(result) => match result.decision {
                    model::Decision::Set { version } => Dec::Set(version.get()),
                    model::Decision::Deleted { existed } => Dec::Deleted(existed),
                    decision @ (model::Decision::VersionMismatch { .. }
                    | model::Decision::CapacityExceeded { .. }) => {
                        Dec::Other(format!("{decision:?}"))
                    }
                },
                model::Applied::Replayed(_) | model::Applied::Conflict { .. } => {
                    Dec::Other(format!("{applied:?}"))
                }
            })
        }

        fn scan(&self, request: &Request) -> Result<Answer, String> {
            let bound = |raw: &Option<String>| {
                raw.as_ref()
                    .map(|raw| model::Key::new(raw.clone()).unwrap())
            };
            let Ok(scan) = model::ScanRequest::new(
                bound(&request.start),
                bound(&request.end),
                bound(&request.after),
                Some(request.limit),
            ) else {
                return Ok(Answer::Refused);
            };
            let page = self
                .0
                .scan(&scan)
                .map_err(|error| format!("the model refused a valid bound: {error:?}"))?;
            if page.has_more != page.next_after.is_some() {
                return Err(format!(
                    "the model's has_more {} disagrees with its next_after {:?}",
                    page.has_more, page.next_after
                ));
            }
            Ok(Answer::Page {
                items: page
                    .items
                    .into_iter()
                    .map(|item| {
                        (
                            item.key.as_str().to_owned(),
                            item.value.as_str().to_owned(),
                            item.version.get(),
                        )
                    })
                    .collect(),
                next_after: page.next_after.map(|key| key.as_str().to_owned()),
            })
        }
    }

    /// Which comparison caught a divergence.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Path {
        /// A write's result.
        Apply,
        /// A SCAN's first page (or refusal).
        Page,
        /// A later page of a continued traversal.
        Traverse,
    }

    #[derive(Clone, Debug)]
    struct Divergence {
        step: usize,
        path: Path,
        what: String,
    }

    impl fmt::Display for Divergence {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "step {} ({:?}): {}", self.step, self.path, self.what)
        }
    }

    /// What the run reached, for the coverage floors: how each page ended, as the model
    /// answered it.
    #[derive(Clone, Copy, Debug, Default)]
    struct Coverage {
        refused: u64,
        empty: u64,
        exhausted: u64,
        by_limit: u64,
        by_bytes: u64,
        after_governs: u64,
        deleted_existing: u64,
        long_traversals: u64,
    }

    impl Coverage {
        fn page(&mut self, answer: &Answer, request: &Request) {
            match answer {
                Answer::Refused => self.refused += 1,
                Answer::Page { items, next_after } => {
                    let limit = usize::try_from(request.limit).unwrap();
                    match (items.len(), next_after) {
                        (0, _) => self.empty += 1,
                        (_, None) => self.exhausted += 1,
                        (count, Some(_)) if count == limit => self.by_limit += 1,
                        (_, Some(_)) => self.by_bytes += 1,
                    }
                    if request.after.is_some()
                        && (request.start.is_none() || request.after >= request.start)
                    {
                        self.after_governs += 1;
                    }
                }
            }
        }
    }

    /// Compares one SCAN step, and its traversal when asked.
    fn compare_scan(
        at: usize,
        scan: Scan,
        machine: &Machine,
        oracle: &Oracle,
        coverage: &mut Coverage,
    ) -> Result<(), Divergence> {
        let diverged = |path, what| Divergence {
            step: at,
            path,
            what,
        };
        let mut request = Request::of(scan);
        let mut path = Path::Page;
        for pages in 0..=KEYS.len() {
            let expected = oracle.scan(&request).map_err(|what| diverged(path, what))?;
            let observed = machine.scan(&request);
            coverage.page(&expected, &request);
            if observed != expected {
                return Err(diverged(
                    path,
                    format!(
                        "SCAN {request:?}: the state machine answers {}, the model {}",
                        brief(&observed),
                        brief(&expected)
                    ),
                ));
            }
            let Answer::Page {
                next_after: Some(next),
                ..
            } = expected
            else {
                if pages >= 2 {
                    coverage.long_traversals += 1;
                }
                return Ok(());
            };
            if !scan.traverse {
                return Ok(());
            }
            request = request.continued(next);
            path = Path::Traverse;
        }
        Err(diverged(
            Path::Traverse,
            format!("the traversal did not end within {} pages", KEYS.len() + 1),
        ))
    }

    /// Runs `log` on both sides, comparing every write and every SCAN.
    fn run(log: &[Step], mutant: Mutant) -> Result<Coverage, Divergence> {
        let mut machine = Machine::new(mutant);
        let mut oracle = Oracle::new();
        let mut coverage = Coverage::default();
        let mut index = 0;
        for (at, &step) in log.iter().enumerate() {
            if let Step::Scan(scan) = step {
                compare_scan(at, scan, &machine, &oracle, &mut coverage)?;
                continue;
            }
            index += 1;
            let observed = machine.apply(index, step);
            let expected = oracle.apply(index, step).map_err(|what| Divergence {
                step: at,
                path: Path::Apply,
                what,
            })?;
            if expected == Dec::Deleted(true) {
                coverage.deleted_existing += 1;
            }
            if observed != expected {
                return Err(Divergence {
                    step: at,
                    path: Path::Apply,
                    what: format!(
                        "{step:?} at entry {index}: the state machine says {observed:?}, the \
                         model {expected:?}"
                    ),
                });
            }
        }
        Ok(coverage)
    }

    /// [`run`], with a panic on either side reported as a divergence.
    fn guarded(log: &[Step], mutant: Mutant) -> Result<Coverage, Divergence> {
        panic::catch_unwind(AssertUnwindSafe(|| run(log, mutant))).unwrap_or_else(|cause| {
            let what = cause
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| cause.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            Err(Divergence {
                step: log.len(),
                path: Path::Apply,
                what: format!("a panic: {what}"),
            })
        })
    }

    /// Simpler versions of a SCAN request: a smaller valid limit, a bound or `after`
    /// dropped, no traversal.
    fn simpler(scan: Scan) -> Vec<Scan> {
        let mut simpler: Vec<Scan> = LIMITS
            .iter()
            .filter(|&&limit| (1..scan.limit).contains(&limit))
            .map(|&limit| Scan { limit, ..scan })
            .collect();
        if scan.start.is_some() {
            simpler.push(Scan {
                start: None,
                ..scan
            });
        }
        if scan.end.is_some() {
            simpler.push(Scan { end: None, ..scan });
        }
        if scan.after.is_some() {
            simpler.push(Scan {
                after: None,
                ..scan
            });
        }
        if scan.traverse {
            simpler.push(Scan {
                traverse: false,
                ..scan
            });
        }
        simpler
    }

    /// Shrinks a diverging log: removes steps one at a time, then simplifies SCAN requests,
    /// for as long as the log still diverges.
    fn shrink(log: &[Step], mutant: Mutant) -> Vec<Step> {
        let mut log = log.to_vec();
        loop {
            let before = log.clone();
            let mut at = 0;
            while at < log.len() {
                let mut fewer = log.clone();
                fewer.remove(at);
                if guarded(&fewer, mutant).is_err() {
                    log = fewer;
                } else {
                    at += 1;
                }
            }
            for at in 0..log.len() {
                let Step::Scan(scan) = log[at] else {
                    continue;
                };
                for candidate in simpler(scan) {
                    let mut tried = log.clone();
                    tried[at] = Step::Scan(candidate);
                    if guarded(&tried, mutant).is_err() {
                        log = tried;
                        break;
                    }
                }
            }
            if log == before {
                return log;
            }
        }
    }

    /// The first seed whose log diverges under `mutant`, its shrunk log and divergence.
    fn first_failure(mutant: Mutant) -> Option<(u64, Vec<Step>, Divergence)> {
        REGRESSION_SEEDS
            .iter()
            .copied()
            .chain(0..SEEDS)
            .find_map(|seed| {
                let log = generate(seed);
                guarded(&log, mutant).err().map(|_| {
                    let shrunk = shrink(&log, mutant);
                    let divergence = guarded(&shrunk, mutant).unwrap_err();
                    (seed, shrunk, divergence)
                })
            })
    }

    #[test]
    fn scan_pages_match_the_model_on_seeded_logs() {
        if let Some((seed, shrunk, divergence)) = first_failure(Mutant::None) {
            panic!(
                "rule: every SCAN page, refusal and continued traversal matches the reference \
                 model (T-7, F-4, §8.5, §7.11); seed {seed}: {divergence}\n  shrunk log ({} \
                 steps): {shrunk:?}\n  re-run: {RERUN}",
                shrunk.len()
            );
        }
    }

    #[test]
    fn the_seeded_logs_reach_every_way_a_page_ends() {
        let mut total = Coverage::default();
        for seed in 0..SEEDS {
            let reached = run(&generate(seed), Mutant::None).unwrap();
            total.refused += reached.refused;
            total.empty += reached.empty;
            total.exhausted += reached.exhausted;
            total.by_limit += reached.by_limit;
            total.by_bytes += reached.by_bytes;
            total.after_governs += reached.after_governs;
            total.deleted_existing += reached.deleted_existing;
            total.long_traversals += reached.long_traversals;
        }
        let floors = [
            ("refused requests", total.refused, 20),
            ("empty pages", total.empty, 20),
            ("exhausted pages", total.exhausted, 20),
            ("pages stopped by the item limit", total.by_limit, 20),
            ("pages stopped by the byte cap", total.by_bytes, 10),
            (
                "pages where after governs the start",
                total.after_governs,
                20,
            ),
            ("DELETEs of a live key", total.deleted_existing, 20),
            (
                "traversals of three or more pages",
                total.long_traversals,
                10,
            ),
        ];
        let short: Vec<String> = floors
            .iter()
            .filter(|(_, reached, floor)| reached < floor)
            .map(|(what, reached, floor)| format!("{what}: {reached} < {floor}"))
            .collect();
        assert!(
            short.is_empty(),
            "rule: the seeded logs reach every way a page ends, so the comparison is not \
             vacuous (P10); short of the floors: {short:?}; reached {total:?}; re-run: ./dev \
             check DIFFERENTIAL-SCAN --case \
             differential_scan::the_seeded_logs_reach_every_way_a_page_ends"
        );
    }

    /// The log whose first 16 in-order items hold `N + delta` decoded bytes, followed by a
    /// probe key, then one whole-range SCAN traversed to its end.
    fn boundary_log(delta: i64) -> Vec<Step> {
        let at = |name: &str| KEYS.iter().position(|key| key == name).unwrap();
        // In key order: a, a0..a4, b, b0..b4, e+U+0301, then e+U+0301 with 0, 1, 2 (16 keys,
        // 37 bytes); the probe is e+U+0301 with 3.
        let mut names: Vec<String> = Vec::new();
        for stem in ["a", "b"] {
            names.push(stem.to_owned());
            names.extend((0..5).map(|digit| format!("{stem}{digit}")));
        }
        names.push("e\u{301}".to_owned());
        names.extend((0..3).map(|digit| format!("e\u{301}{digit}")));
        let keys: usize = names.iter().map(String::len).sum();
        let filler =
            usize::try_from(i64::try_from(N - 15 * 65_536 - keys).unwrap() + delta).unwrap();
        let mut log: Vec<Step> = names[..15]
            .iter()
            .map(|name| Step::Put {
                key: at(name),
                value: 5,
            })
            .collect();
        log.push(Step::Put {
            key: at(&names[15]),
            value: FILLER + filler,
        });
        log.push(Step::Put {
            key: at("e\u{301}3"),
            value: 1,
        });
        log.push(Step::Scan(Scan {
            start: None,
            end: None,
            after: None,
            limit: 128,
            traverse: true,
        }));
        log
    }

    /// §7.11's byte cap, N, in decoded bytes.
    const N: usize = 1 << 20;

    #[test]
    fn the_byte_cap_boundary_agrees_with_the_model_at_n_minus_1_n_and_n_plus_1() {
        for (delta, page_items) in [(-1, 16), (0, 16), (1, 15)] {
            let log = boundary_log(delta);
            let mut oracle = Oracle::new();
            for (index, &step) in (1..).zip(&log[..log.len() - 1]) {
                oracle.apply(index, step).unwrap();
            }
            let Step::Scan(scan) = log[log.len() - 1] else {
                panic!("fixture: the boundary log ends with its SCAN");
            };
            let first = oracle.scan(&Request::of(scan)).unwrap();
            let Answer::Page { items, next_after } = &first else {
                panic!("fixture: the whole-range SCAN is valid");
            };
            assert!(
                items.len() == page_items && next_after.is_some(),
                "fixture: with 16 items at N{delta:+} bytes the model's first page holds \
                 {page_items} items and continues; got {}",
                brief(&first)
            );
            if let Err(divergence) = guarded(&log, Mutant::None) {
                panic!(
                    "rule: at the 1 MiB byte cap (N{delta:+} bytes in 16 items) the state \
                     machine's pages match the model's (§7.11, T-12); {divergence}; re-run: \
                     ./dev check DIFFERENTIAL-SCAN --case \
                     differential_scan::the_byte_cap_boundary_agrees_with_the_model_at_n_minus_1_n_and_n_plus_1"
                );
            }
        }
    }

    /// Asserts `mutant` is caught, by a SCAN comparison, with a shrunk log of at most
    /// `most_steps` steps.
    fn assert_caught(mutant: Mutant, most_steps: usize) {
        let Some((seed, shrunk, divergence)) = first_failure(mutant) else {
            panic!("rule: the {mutant:?} mutant is caught within {SEEDS} seeds (W28.2)");
        };
        assert!(
            divergence.path != Path::Apply && shrunk.len() <= most_steps,
            "rule: the {mutant:?} mutant is caught by a SCAN comparison, shrinking to at most \
             {most_steps} steps; seed {seed}: {divergence}; shrunk {shrunk:?}"
        );
    }

    #[test]
    fn a_page_one_item_short_is_caught() {
        assert_caught(Mutant::PageOneShort, 3);
    }

    #[test]
    fn a_lost_has_more_is_caught() {
        assert_caught(Mutant::HasMoreLost, 3);
    }

    #[test]
    fn a_next_after_that_is_not_the_last_returned_key_is_caught() {
        assert_caught(Mutant::NextAfterFirst, 4);
    }

    #[test]
    fn a_page_from_before_the_last_entry_is_caught() {
        assert_caught(Mutant::StaleView, 2);
    }

    #[test]
    fn a_version_off_by_one_is_caught() {
        assert_caught(Mutant::VersionOffByOne, 2);
    }

    #[test]
    fn a_limit_one_too_loose_is_caught() {
        assert_caught(Mutant::LimitOneLoose, 3);
    }
}
