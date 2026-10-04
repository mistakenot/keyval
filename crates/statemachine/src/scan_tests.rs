//! Tests of SCAN over the applied state (PLAN.md §8.5, §7.11, F-4, DM-4; W28.1).
//!
//! - Range edges, `after` continuation, the item and decoded-byte caps at N − 1, N and
//!   N + 1, `has_more`/`next_after`, and the accumulation rule's fine print: the page stops
//!   rather than skips, the first item is always returned, and neither the probe item nor
//!   `next_after` counts.
//! - [`UNICODE_ORDER`], the independent byte-order fixture: keys whose UTF-8 bytes are
//!   written out by hand, listed in the order those bytes give (DM-4), never in an order a
//!   comparator computed (§11.5 rule 3).
//! - Seeded properties over random states, ranges and limits: every page is the longest
//!   run of in-range keys the caps allow, and a continued traversal returns every in-range
//!   key exactly once, in order. A failure shrinks its state and names its seed (W01.6).
#![cfg(test)]

// check: STATE-SCAN

use std::collections::BTreeMap;
use std::ops::Range;

use types::{
    ByteCount, Decision, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits, LogIndex,
    MAX_SAFE_INTEGER, MAX_SCAN_PAGE_BYTES, Mutation, NodeId, PageSize, RequestId, ScanPage,
    ScanRange, ServedBy, Term, Value,
};

use crate::{Applied, Command, StateLimits, StateMachine, codec};

/// §7.11's byte cap, N, as a length.
const N: usize = 1_048_576;

fn node() -> NodeId {
    NodeId::new(1).unwrap()
}

fn key(raw: &str) -> Key {
    Key::parse(raw.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

/// A value of exactly `len` UTF-8 bytes, mostly four-byte characters, so a cap counted in
/// characters instead of bytes would admit four times as much.
fn value_of(len: usize) -> Value {
    let raw = "\u{1F600}".repeat(len / 4) + &"v".repeat(len % 4);
    Value::parse(raw, KeyValueLimits::MAXIMA).unwrap()
}

/// Generous limits: these tests are about pages, not the live-state quota.
fn limits() -> StateLimits {
    StateLimits {
        key_values: KeyValueLimits::MAXIMA,
        max_live_keys: ItemCount::new(100_000),
        max_live_bytes: ByteCount::new(1 << 32),
    }
}

/// The state after one PUT per `(key, value)`, in the order given, at entries 1, 2, ….
fn state_of(puts: &[(String, Value)]) -> StateMachine {
    let mut state = StateMachine::new(node(), limits());
    let commit = LogIndex::new(MAX_SAFE_INTEGER).unwrap();
    for (raw, (name, value)) in (1_u64..).zip(puts) {
        let mut id = [0xA5; 16];
        id[8..].copy_from_slice(&raw.to_be_bytes());
        let payload = EntryPayload::Command(codec::encode(&Command {
            request_id: RequestId::from_bytes(id),
            mutation: Mutation::Put {
                key: key(name),
                value: value.clone(),
            },
        }));
        let entry = Entry {
            index: EntryIndex::new(raw).unwrap(),
            term: Term::new(1).unwrap(),
            payload,
        };
        let Applied::Executed { result, .. } = state.apply(&entry, commit) else {
            panic!("fixture: entry {raw} is a PUT");
        };
        assert_eq!(
            result.decision(),
            Decision::Set,
            "fixture: PUT {name:?} at entry {raw} is applied"
        );
    }
    state
}

/// The state after a PUT of each `(key, value length)`.
fn state(puts: &[(&str, usize)]) -> StateMachine {
    let puts: Vec<(String, Value)> = puts
        .iter()
        .map(|&(name, len)| (name.to_owned(), value_of(len)))
        .collect();
    state_of(&puts)
}

/// A range from optional raw bounds and a limit.
fn range(start: Option<&str>, end: Option<&str>, after: Option<&str>, limit: u64) -> ScanRange {
    ScanRange::new(
        start.map(key),
        end.map(key),
        after.map(key),
        PageSize::new(limit).unwrap(),
    )
    .unwrap()
}

/// A table row's raw bounds: `start`, `end` and `after`.
#[derive(Clone, Copy, Debug)]
struct Bounds(
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

impl Bounds {
    fn range(self, limit: u64) -> ScanRange {
        range(self.0, self.1, self.2, limit)
    }
}

/// A page of `range`, served by this node at the state's applied index.
fn page(state: &StateMachine, range: &ScanRange) -> ScanPage {
    state.scan(
        range,
        ServedBy {
            node: node(),
            applied: state.applied_index(),
        },
    )
}

/// A page's keys and continuation, as plain strings for readable assertions.
fn keys(page: &ScanPage) -> (Vec<String>, Option<String>) {
    (
        page.items()
            .iter()
            .map(|item| item.key.as_str().to_owned())
            .collect(),
        page.continuation().map(|key| key.as_str().to_owned()),
    )
}

fn strings(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|&s| s.to_owned()).collect()
}

#[test]
fn start_is_inclusive_end_is_exclusive_and_omitted_bounds_are_open() {
    let state = state(&[("b", 1), ("c", 2), ("d", 3), ("e", 4)]);
    let cases: [(Option<&str>, Option<&str>, &[&str]); 9] = [
        (None, None, &["b", "c", "d", "e"]),
        (Some("c"), None, &["c", "d", "e"]),
        (None, Some("d"), &["b", "c"]),
        (Some("c"), Some("e"), &["c", "d"]),
        (Some("bb"), Some("dd"), &["c", "d"]),
        (Some("a"), Some("z"), &["b", "c", "d", "e"]),
        (Some("c"), Some("c"), &[]),
        (Some("f"), None, &[]),
        (None, Some("b"), &[]),
    ];
    for (start, end, expected) in cases {
        let page = page(&state, &range(start, end, None, 128));
        assert_eq!(
            keys(&page),
            (strings(expected), None),
            "rule: a page holds [start, end), omitted bounds open, equal bounds empty (§8.5); \
             start={start:?} end={end:?}"
        );
    }
    let page = page(&state, &range(None, None, None, 128));
    let versions: Vec<u64> = page.items().iter().map(|item| item.version.get()).collect();
    let lengths: Vec<usize> = page
        .items()
        .iter()
        .map(|item| item.value.byte_len())
        .collect();
    assert_eq!(
        (versions, lengths),
        (vec![1, 2, 3, 4], vec![1, 2, 3, 4]),
        "rule: each item carries its key's value and version, the index that set it (DM-3)"
    );
}

#[test]
fn after_is_exclusive_and_intersected_with_the_range() {
    let state = state(&[("b", 1), ("c", 1), ("d", 1), ("e", 1)]);
    let cases: [(Bounds, &[&str]); 11] = [
        (Bounds(None, None, Some("c")), &["d", "e"]),
        (Bounds(Some("c"), None, Some("c")), &["d", "e"]),
        (Bounds(Some("c"), None, Some("b")), &["c", "d", "e"]),
        (Bounds(Some("c"), None, Some("a")), &["c", "d", "e"]),
        (Bounds(None, Some("d"), Some("b")), &["c"]),
        (Bounds(None, Some("d"), Some("c")), &[]),
        (Bounds(None, Some("d"), Some("d")), &[]),
        (Bounds(None, Some("d"), Some("e")), &[]),
        (Bounds(Some("b"), Some("c"), Some("z")), &[]),
        (Bounds(None, None, Some("cc")), &["d", "e"]),
        (Bounds(Some("c"), Some("c"), Some("c")), &[]),
    ];
    for (bounds, expected) in cases {
        let page = page(&state, &bounds.range(128));
        assert_eq!(
            keys(&page),
            (strings(expected), None),
            "rule: after is an exclusive bound intersected with [start, end); after >= end \
             gives an empty page (§8.5); (start, end, after)={bounds:?}"
        );
    }
}

#[test]
fn an_empty_state_gives_an_empty_page_with_no_continuation() {
    let empty = state(&[]);
    for (start, end, after) in [
        (None, None, None),
        (Some("a"), Some("b"), None),
        (None, None, Some("a")),
    ] {
        assert_eq!(
            keys(&page(&empty, &range(start, end, after, 1))),
            (Vec::new(), None),
            "rule: an empty view has nothing in any range (§8.5)"
        );
    }
}

#[test]
fn the_item_limit_stops_the_page_and_has_more_says_whether_a_key_remains() {
    let five = state(&[("k1", 1), ("k2", 1), ("k3", 1), ("k4", 1), ("k5", 1)]);
    let cases: [(u64, &[&str], Option<&str>); 4] = [
        (1, &["k1"], Some("k1")),
        (4, &["k1", "k2", "k3", "k4"], Some("k4")),
        (5, &["k1", "k2", "k3", "k4", "k5"], None),
        (6, &["k1", "k2", "k3", "k4", "k5"], None),
    ];
    for (limit, expected, next) in cases {
        assert_eq!(
            keys(&page(&five, &range(None, None, None, limit))),
            (strings(expected), next.map(str::to_owned)),
            "rule: at most limit items; has_more and next_after (the last returned key) only \
             when a key in range remains (§8.5); limit={limit}"
        );
    }
    for (count, expected_next) in [(127_usize, None), (128, None), (129, Some("k127"))] {
        let names: Vec<String> = (0..count).map(|n| format!("k{n:03}")).collect();
        let puts: Vec<(&str, usize)> = names.iter().map(|name| (name.as_str(), 0)).collect();
        let full = state(&puts);
        let page = page(&full, &range(None, None, None, PageSize::MAX));
        assert_eq!(
            (page.items().len(), keys(&page).1),
            (count.min(128), expected_next.map(str::to_owned)),
            "rule: a page holds at most 128 items (F-4); {count} keys in range"
        );
    }
    let default = PageSize::DEFAULT.get();
    let names: Vec<String> = (0..33).map(|n| format!("k{n:02}")).collect();
    let puts: Vec<(&str, usize)> = names.iter().map(|name| (name.as_str(), 0)).collect();
    let page = page(&state(&puts), &range(None, None, None, default));
    assert_eq!(
        keys(&page),
        (names[..32].to_vec(), Some("k31".to_owned())),
        "rule: the default page is 32 items (F-4)"
    );
}

/// Keys `k00`… with 15 items of 65,536 bytes, then one more so the first 16 items hold
/// `total16` decoded bytes, then the 3-byte, empty-valued `extra` keys.
fn sixteen_then(total16: usize, extra: &[&str]) -> StateMachine {
    let fifteen = 15 * 65_536;
    let mut puts: Vec<(String, usize)> = (0..15).map(|n| (format!("k{n:02}"), 65_533)).collect();
    puts.push(("k15".to_owned(), total16 - fifteen - 3));
    puts.extend(extra.iter().map(|&name| (name.to_owned(), 0)));
    let puts: Vec<(&str, usize)> = puts
        .iter()
        .map(|(name, len)| (name.as_str(), *len))
        .collect();
    state(&puts)
}

#[test]
fn the_byte_cap_admits_n_minus_1_and_exactly_n_and_stops_before_n_plus_1() {
    assert_eq!(
        MAX_SCAN_PAGE_BYTES,
        u64::try_from(N).unwrap(),
        "fixture: the cap is §7.11's 1 MiB"
    );
    let first16: Vec<String> = (0..16).map(|n| format!("k{n:02}")).collect();
    let with = |last: &str| {
        let mut keys = first16.clone();
        keys.push(last.to_owned());
        keys
    };
    let cases = [
        ("N - 1", sixteen_then(N - 4, &["k16"]), with("k16"), None),
        ("N", sixteen_then(N - 3, &["k16"]), with("k16"), None),
        (
            "N, with a key beyond: neither the probe nor next_after counts",
            sixteen_then(N - 3, &["k16", "k17"]),
            with("k16"),
            Some("k16".to_owned()),
        ),
        (
            "N + 1",
            sixteen_then(N - 2, &["k16"]),
            first16.clone(),
            Some("k15".to_owned()),
        ),
    ];
    for (case, state, expected, next) in cases {
        let page = page(&state, &range(None, None, None, 128));
        let bytes: usize = page
            .items()
            .iter()
            .map(|item| item.key.byte_len() + item.value.byte_len())
            .sum();
        assert_eq!(
            keys(&page),
            (expected, next),
            "rule: a page holds at most 1 MiB of decoded key+value bytes and stops before the \
             item that would exceed it (§7.11); case {case}, page bytes {bytes}"
        );
    }
}

#[test]
fn the_byte_cap_counts_decoded_bytes_never_escaped_length() {
    // Quotes, backslashes and control characters: one decoded byte each, more once JSON
    // escapes them. Sixteen items of exactly 65,536 decoded bytes fill the page to N.
    let raw = "\"\\\u{1}".repeat(21_844) + "v";
    let value = Value::parse(raw, KeyValueLimits::MAXIMA).unwrap();
    assert_eq!(value.byte_len(), 65_533, "fixture: 65,533 decoded bytes");
    let puts: Vec<(String, Value)> = (0..17)
        .map(|n| {
            (
                format!("k{n:02}"),
                if n < 16 { value.clone() } else { Value::EMPTY },
            )
        })
        .collect();
    let page = page(&state_of(&puts), &range(None, None, None, 128));
    assert_eq!(
        (page.items().len(), keys(&page).1),
        (16, Some("k15".to_owned())),
        "rule: the cap counts decoded bytes (§7.11): 16 items of 65,536 bytes fill 1 MiB exactly"
    );
}

#[test]
fn the_page_stops_before_an_item_that_does_not_fit_and_never_skips_to_a_smaller_one() {
    let names: Vec<String> = (0..20).map(|n| format!("big{n:02}")).collect();
    let mut puts: Vec<(&str, usize)> = names.iter().map(|name| (name.as_str(), 60_000)).collect();
    puts.push(("tiny", 0));
    let state = state(&puts);
    let page = page(&state, &range(None, None, None, 128));
    // 17 items of 60,005 bytes = 1,020,085; an 18th would make 1,080,090 > N.
    assert_eq!(
        keys(&page),
        (names[..17].to_vec(), Some("big16".to_owned())),
        "rule: the page stops before the first item that would exceed the cap, though a later \
         smaller item (tiny) would fit (§7.11)"
    );
}

#[test]
fn the_first_item_is_always_returned_and_a_maximum_size_item_always_fits() {
    const {
        assert!(
            Key::MAX_BYTES + Value::MAX_BYTES <= N,
            "rule: one maximum-size item always fits a page (§7.11)"
        );
    }
    let max_key = |n: usize| format!("{n:02}{}", "k".repeat(Key::MAX_BYTES - 2));
    let names: Vec<String> = (0..16).map(max_key).collect();
    let puts: Vec<(&str, usize)> = names
        .iter()
        .map(|name| (name.as_str(), Value::MAX_BYTES))
        .collect();
    let one = state(&puts[..1]);
    assert_eq!(
        keys(&page(&one, &range(None, None, None, 1))),
        (names[..1].to_vec(), None),
        "rule: the first item is always returned, and a maximum-size item fits"
    );
    // 15 maximum items are 986,880 bytes; a 16th would make 1,052,672 > N.
    let sixteen = state(&puts);
    assert_eq!(
        keys(&page(&sixteen, &range(None, None, None, 128))),
        (names[..15].to_vec(), Some(names[14].clone())),
        "rule: maximum-size items fill a page 15 at a time"
    );
    assert_eq!(
        keys(&page(
            &sixteen,
            &range(None, None, Some(names[14].as_str()), 128)
        )),
        (names[15..].to_vec(), None),
        "rule: the continued page starts with the item that did not fit"
    );
}

#[test]
#[should_panic(expected = "invariant SM-SCAN-PREFIX violated")]
fn a_page_must_report_the_prefix_it_was_read_from() {
    let state = state(&[("a", 1)]);
    let _ = state.scan(
        &range(None, None, None, 1),
        ServedBy {
            node: node(),
            applied: LogIndex::new(2).unwrap(),
        },
    );
}

/// The independent byte-order fixture (DM-4, W28): each key with its UTF-8 bytes written out
/// by hand from the encoding (below U+0080 one byte; then lead bytes C2–DF, E0–EF and F0–F4
/// for two, three and four bytes), listed in the order those bytes give. The order was
/// derived from the bytes by hand, not computed by a comparator (§11.5 rule 3). It
/// disagrees with UTF-16 order (U+E000 and U+FF5E sort after U+10000 there), with
/// case-insensitive order and with normalised order (the decomposed é is a different key,
/// far from the precomposed one).
const UNICODE_ORDER: &[(&str, &[u8])] = &[
    ("!", &[0x21]),
    ("0", &[0x30]),
    ("A", &[0x41]),
    ("Z", &[0x5A]),
    ("a", &[0x61]),
    ("a!", &[0x61, 0x21]),
    ("aa", &[0x61, 0x61]),
    ("e\u{301}", &[0x65, 0xCC, 0x81]),
    ("z", &[0x7A]),
    ("~", &[0x7E]),
    ("\u{7F}", &[0x7F]),
    ("\u{80}", &[0xC2, 0x80]),
    ("\u{E9}", &[0xC3, 0xA9]),
    ("\u{FF}", &[0xC3, 0xBF]),
    ("\u{100}", &[0xC4, 0x80]),
    ("\u{7FF}", &[0xDF, 0xBF]),
    ("\u{800}", &[0xE0, 0xA0, 0x80]),
    ("\u{20AC}", &[0xE2, 0x82, 0xAC]),
    ("\u{4E2D}", &[0xE4, 0xB8, 0xAD]),
    ("\u{D7FF}", &[0xED, 0x9F, 0xBF]),
    ("\u{E000}", &[0xEE, 0x80, 0x80]),
    ("\u{FF5E}", &[0xEF, 0xBD, 0x9E]),
    ("\u{FFFD}", &[0xEF, 0xBF, 0xBD]),
    ("\u{10000}", &[0xF0, 0x90, 0x80, 0x80]),
    ("\u{1F600}", &[0xF0, 0x9F, 0x98, 0x80]),
    ("\u{10FFFF}", &[0xF4, 0x8F, 0xBF, 0xBF]),
];

/// The fixture's keys in positions `at`, as owned strings.
fn fixture(at: Range<usize>) -> Vec<String> {
    UNICODE_ORDER[at]
        .iter()
        .map(|&(key, _)| key.to_owned())
        .collect()
}

/// The state holding every fixture key, PUT in a scrambled order (position 7n mod 26).
fn unicode_state() -> StateMachine {
    let count = UNICODE_ORDER.len();
    let puts: Vec<(&str, usize)> = (0..count)
        .map(|n| (UNICODE_ORDER[n * 7 % count].0, n))
        .collect();
    state(&puts)
}

#[test]
fn the_unicode_fixture_spells_each_key_in_its_hand_written_utf8_bytes() {
    for &(key, bytes) in UNICODE_ORDER {
        assert_eq!(
            key.as_bytes(),
            bytes,
            "fixture: {key:?} is encoded as the bytes written beside it"
        );
    }
    let utf16 = |key: &str| key.encode_utf16().collect::<Vec<u16>>();
    assert!(
        utf16("\u{10000}") < utf16("\u{E000}"),
        "fixture: in UTF-16 order U+10000 sorts before U+E000, so a UTF-16 comparison fails \
         the fixture"
    );
}

#[test]
fn scan_returns_keys_in_the_hand_derived_utf8_byte_order() {
    let state = unicode_state();
    let all = fixture(0..UNICODE_ORDER.len());
    assert_eq!(
        keys(&page(&state, &range(None, None, None, 128))),
        (all.clone(), None),
        "rule: keys are ordered by the bytes of their UTF-8 encoding (DM-4)"
    );
    for limit in [1, 2, 3, 7] {
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = page(&state, &range(None, None, after.as_deref(), limit));
            let (got, next) = keys(&page);
            seen.extend(got);
            match next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(
            seen, all,
            "rule: continued pages return every key once, in byte order; limit={limit}"
        );
    }
}

#[test]
fn unicode_bounds_cut_at_their_byte_order_positions() {
    let state = unicode_state();
    let cases: [(Bounds, Range<usize>); 6] = [
        (Bounds(Some("\u{E9}"), Some("\u{20AC}"), None), 12..17),
        (Bounds(Some("e"), Some("\u{80}"), None), 7..11),
        (Bounds(None, None, Some("\u{FFFD}")), 23..26),
        (Bounds(Some("a"), Some("b"), None), 4..7),
        (Bounds(Some("\u{E000}"), Some("\u{10000}"), None), 20..23),
        (Bounds(Some("A"), Some("a"), Some("Z")), 4..4),
    ];
    for (bounds, at) in cases {
        assert_eq!(
            keys(&page(&state, &bounds.range(128))),
            (fixture(at.clone()), None),
            "rule: range bounds compare by UTF-8 bytes (DM-4); (start, end, after)={bounds:?}, \
             expected fixture positions {at:?}"
        );
    }
}

/// Seeds that once failed, replayed first on every run. Add the seed a failure names, with
/// the shrunk case it printed as a comment, in the commit that fixes it.
const REGRESSION_SEEDS: &[u64] = &[];

/// Random states per run, after the regression seeds; each is scanned with [`RANGES`]
/// random ranges.
const SEEDS: u64 = 48;

/// Random ranges per state.
const RANGES: usize = 32;

/// The re-run command a failure prints.
const RERUN: &str =
    "./dev check STATE-SCAN --case scan_tests::scan_properties_hold_on_seeded_states";

/// Key stems: ASCII, decomposed and precomposed é, and three-, four- and other four-byte
/// characters, so random keys exercise byte order.
const STEMS: &[&str] = &["a", "b", "e\u{301}", "\u{E9}", "\u{20AC}", "\u{1F600}"];

/// A seeded `SplitMix64` step: these tests' only randomness (§7.14).
fn next(rng: &mut u64) -> u64 {
    *rng = rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *rng;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn below(rng: &mut u64, bound: usize) -> usize {
    usize::try_from(next(rng) % u64::try_from(bound).unwrap()).unwrap()
}

/// A random key: a stem, sometimes with a digit after it.
fn random_key(rng: &mut u64) -> String {
    let stem = STEMS[below(rng, STEMS.len())];
    match below(rng, 11) {
        10 => stem.to_owned(),
        digit => format!("{stem}{digit}"),
    }
}

/// A generated range: optional bounds, continuation and limit.
#[derive(Clone, Debug)]
struct RawRange {
    start: Option<String>,
    end: Option<String>,
    after: Option<String>,
    limit: u64,
}

impl RawRange {
    fn range(&self) -> ScanRange {
        range(
            self.start.as_deref(),
            self.end.as_deref(),
            self.after.as_deref(),
            self.limit,
        )
    }
}

/// One generated case: the PUTs that build the state, and the ranges to scan it with.
#[derive(Clone, Debug)]
struct Case {
    puts: Vec<(String, usize)>,
    ranges: Vec<RawRange>,
}

/// A state of up to 39 PUTs, half of them large, so the byte cap often decides a page,
/// and [`RANGES`] ranges with random bounds, continuation and limit.
fn generate(seed: u64) -> Case {
    let mut rng = seed;
    let puts = (0..below(&mut rng, 40))
        .map(|_| {
            let len = if next(&mut rng).is_multiple_of(2) {
                55_000 + below(&mut rng, 10_537)
            } else {
                below(&mut rng, 8)
            };
            (random_key(&mut rng), len)
        })
        .collect();
    let ranges = (0..RANGES)
        .map(|_| {
            let bound = |rng: &mut u64| (below(rng, 3) > 0).then(|| random_key(rng));
            let (mut start, mut end) = (bound(&mut rng), bound(&mut rng));
            if start
                .as_ref()
                .zip(end.as_ref())
                .is_some_and(|(low, high)| low > high)
            {
                std::mem::swap(&mut start, &mut end);
            }
            let after = bound(&mut rng).filter(|_| next(&mut rng).is_multiple_of(2));
            let limit = [1, 2, 3, 5, 17, 32, 128][below(&mut rng, 7)];
            RawRange {
                start,
                end,
                after,
                limit,
            }
        })
        .collect();
    Case { puts, ranges }
}

/// How a page ended, for the coverage test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    /// Nothing in range.
    Empty,
    /// Every in-range key was returned.
    Exhausted,
    /// The item limit.
    Limit,
    /// The byte cap.
    Bytes,
}

/// Checks one page of `state` and its continued traversal against the in-range keys, found
/// by filtering the whole map with the range's own `contains`: independent of how the scan
/// finds its first key.
fn check(state: &StateMachine, range: &ScanRange) -> Result<Stop, String> {
    let in_range: Vec<(String, usize, u64)> = state
        .iter()
        .filter(|(key, _)| range.contains(key))
        .map(|(key, stored)| {
            (
                key.as_str().to_owned(),
                key.byte_len() + stored.value().byte_len(),
                stored.version().get(),
            )
        })
        .collect();
    let first = page(state, range);
    let got: Vec<(String, usize, u64)> = first
        .items()
        .iter()
        .map(|item| {
            (
                item.key.as_str().to_owned(),
                item.key.byte_len() + item.value.byte_len(),
                item.version.get(),
            )
        })
        .collect();
    let count = got.len();
    if got[..] != in_range[..count.min(in_range.len())] {
        return Err(format!(
            "the page is not the first {count} in-range keys in order: got {:?}",
            keys(&first).0
        ));
    }
    let bytes: usize = got.iter().map(|&(_, size, _)| size).sum();
    let limit = usize::try_from(range.limit().get()).unwrap();
    if count > limit || bytes > N {
        return Err(format!("{count} items and {bytes} bytes exceed the caps"));
    }
    let stop = if count == in_range.len() {
        if count == 0 {
            Stop::Empty
        } else {
            Stop::Exhausted
        }
    } else if count == 0 {
        return Err(
            "an empty page while a key is in range: the first item is always returned".to_owned(),
        );
    } else if count == limit {
        Stop::Limit
    } else if bytes + in_range[count].1 > N {
        Stop::Bytes
    } else {
        return Err(format!(
            "the page stopped after {count} items and {bytes} bytes though the next item ({} \
             bytes) fits",
            in_range[count].1
        ));
    };
    let expected_next = (count < in_range.len()).then(|| got[count - 1].0.clone());
    if keys(&first).1 != expected_next {
        return Err(format!(
            "next_after is {:?}, expected {expected_next:?} (has_more exactly when a key remains)",
            keys(&first).1
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut after = range.after().cloned();
    for _ in 0..=in_range.len() {
        let next_range = ScanRange::new(
            range.start().cloned(),
            range.end().cloned(),
            after,
            range.limit(),
        )
        .map_err(|err| format!("{err:?}"))?;
        let (got, next) = keys(&page(state, &next_range));
        seen.extend(got);
        let Some(next) = next else {
            let all: Vec<String> = in_range.iter().map(|(key, _, _)| key.clone()).collect();
            return if seen == all {
                Ok(stop)
            } else {
                Err(format!(
                    "the continued traversal returned {seen:?}, not every in-range key once in \
                     order {all:?}"
                ))
            };
        };
        after = Some(key(&next));
    }
    Err(format!(
        "the traversal did not end within {} pages",
        in_range.len() + 1
    ))
}

/// Builds the state of `puts` and checks one range on it.
fn holds(puts: &[(String, usize)], raw: &RawRange) -> Result<Stop, String> {
    let named: Vec<(&str, usize)> = puts
        .iter()
        .map(|(name, len)| (name.as_str(), *len))
        .collect();
    check(&state(&named), &raw.range())
}

/// Removes PUTs one at a time while the range still fails, so a failure prints a minimal
/// state.
fn shrink(mut puts: Vec<(String, usize)>, raw: &RawRange) -> Vec<(String, usize)> {
    let mut at = 0;
    while at < puts.len() {
        let mut fewer = puts.clone();
        fewer.remove(at);
        if holds(&fewer, raw).is_err() {
            puts = fewer;
        } else {
            at += 1;
        }
    }
    puts
}

/// Runs every case of `seed`, returning each page's stop or the first failure, shrunk.
fn run(seed: u64) -> Result<Vec<Stop>, String> {
    let case = generate(seed);
    let named: Vec<(&str, usize)> = case
        .puts
        .iter()
        .map(|(name, len)| (name.as_str(), *len))
        .collect();
    let state = state(&named);
    let mut stops = Vec::with_capacity(case.ranges.len());
    for raw in &case.ranges {
        match check(&state, &raw.range()) {
            Ok(stop) => stops.push(stop),
            Err(failure) => {
                let shrunk = shrink(case.puts.clone(), raw);
                return Err(format!(
                    "seed {seed}: {failure}\n  range = {raw:?}\n  shrunk state (key, value \
                     bytes) = {shrunk:?}\n  re-run: {RERUN}"
                ));
            }
        }
    }
    Ok(stops)
}

#[test]
fn scan_properties_hold_on_seeded_states() {
    for seed in REGRESSION_SEEDS.iter().copied().chain(0..SEEDS) {
        if let Err(failure) = run(seed) {
            panic!(
                "rule: a page is the longest in-range run the caps allow, and continued \
                    pages return every in-range key once (§8.5, §7.11); {failure}"
            );
        }
    }
}

#[test]
fn the_seeded_cases_reach_every_way_a_page_ends() {
    let mut reached: BTreeMap<Stop, usize> = BTreeMap::new();
    let mut after_governs = 0;
    for seed in 0..SEEDS {
        for stop in run(seed).unwrap() {
            *reached.entry(stop).or_default() += 1;
        }
        after_governs += generate(seed)
            .ranges
            .iter()
            .filter(|raw| raw.after.is_some() && (raw.start.is_none() || raw.after >= raw.start))
            .count();
    }
    let all = [Stop::Empty, Stop::Exhausted, Stop::Limit, Stop::Bytes];
    assert!(
        all.iter()
            .all(|stop| reached.get(stop).copied().unwrap_or(0) > 0)
            && after_governs > 0,
        "rule: the generator reaches every way a page ends, and ranges where after governs \
         the lower bound, so the properties are not vacuous (P10); reached={reached:?} \
         after_governs={after_governs}"
    );
}
