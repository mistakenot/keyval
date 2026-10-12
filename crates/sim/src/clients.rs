//! Seeded simulation clients (PLAN.md §10.3, §10.2; W36.1): the workload's clients, which
//! drive a [`Cluster`]'s inbound ports with domain requests and retry exactly as the CLI does.
//!
//! **The plan.** From the clients stream of the master seed, [`Plan::generate`] draws:
//! - 1 to 8 clients and 4 to 32 primary keys;
//! - a hot subset, the first quarter of the keys (at least one), which takes at least half of
//!   the writes, so writes contend;
//! - keys from §10.3's edge cases, as far as the configured limits allow: ordering (`a`, `aa`,
//!   `b`), non-ASCII UTF-8, one text both composed and decomposed, the URL-hostile `/ ? & %`,
//!   and the longest key in ASCII and in two-byte characters;
//! - values that are empty, the same as the key's last write (a repeated same-value write),
//!   the longest allowed, or heavy to encode as JSON (quotes, backslashes and control
//!   characters);
//! - each client's operations under a [`Profile`], each with a think time.
//!
//! **One identity per logical write.** Every planned write carries its own request ID, unique
//! in the run. Its exact command is fixed when it starts, and every send of it, retries and
//! redirects included, carries that same ID and command (§10.3, §8.7). A compare-and-swap
//! fixes its expectation then: the version the client last saw for the key, from a read or
//! a write of its own, or absent if it saw none. Read-modify-write races follow (§10.3).
//!
//! **Retries.** Each logical operation follows a [`Retry`], the policy the CLI shares
//! (`types::retry`): redirects, backoff, same-ID retries of ambiguous writes, status-lookup
//! resolution and §7.11's bounds. A response that does not arrive within the response
//! timeout is silence: lost, or still in flight. An answer to an earlier attempt than the
//! one a client waits on is stale and is ignored, as an abandoned HTTP request would be.
//!
//! **Driving.** [`Clients::run`] advances the cluster in steps of [`POLL_MS`] of virtual
//! time. After each step every client hears the answers that reached it, then the
//! timeouts that passed, then starts its next planned operation if it is idle and its think
//! time is over. Every choice comes from the clients stream, so a seed decides the whole
//! workload (P4), and observing the run draws nothing.
//!
//! **The misuse stream (§10.3, W36.2).** Opt-in with [`Clients::with_misuse`]: after a
//! logical write ends with an observed terminal result, a client may reuse its request ID once
//! with a different command (a PUT or CAS becomes a DELETE, a DELETE becomes a PUT of the
//! empty value), sent to the node that answered. The system must answer `request_id_conflict`
//! with the original's index, and never execute it. A misuse attempt is flagged
//! ([`SentAttempt::misuse`]), is never a logical write (it has no [`Outcome`] and counts in no
//! operation's sends), and is never retried. With no misuse the stream draws nothing, so
//! existing seeds keep their workloads.
//!
//! Every attempt sent is kept in [`Clients::sends`], and every logical operation's end in
//! [`Clients::outcomes`], for the observation recording ([`crate::observe`], W36.2).

use std::collections::BTreeMap;

use types::{
    AttemptId, ClientOp, Config, Exhausted, ExpectedVersion, Heard, Key, KeyValueLimits, Mutation,
    Next, NodeId, OpKind, PageSize, RequestId, RequestState, Response, Retry, RetryBounds,
    ScanRange, Value, Version,
};

use crate::cluster::{Cluster, ClusterError};
use crate::drain::Certified;
use crate::entropy::SplitMix64;
use crate::scheduler::VirtualTime;
use crate::swarm::FaultKind;
use crate::trace::ClientId;

/// The clients' reaction step, in virtual milliseconds.
pub const POLL_MS: u64 = 10;

/// The most clients a run has (§10.3).
pub const MAX_CLIENTS: u64 = 8;

/// The fewest and most primary keys a run has (§10.3).
pub const KEYS: (u64, u64) = (4, 32);

/// The fewest and most operations a client plans.
pub const OPS: (u64, u64) = (6, 20);

/// The longest think time between a client's operations, in milliseconds.
pub const MAX_THINK_MS: u64 = 300;

/// The fewest and most operations a capacity profile's client plans: enough writes, with
/// the small test limits, to reach a live-quota or history limit (§10.2).
const CAPACITY_OPS: (u64, u64) = (40, 80);

/// The longest think time of a capacity profile's client, in milliseconds.
pub const CAPACITY_THINK_MS: u64 = 40;

/// How a run's heal finishes (§10.2, D-53).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealMode {
    /// A new, uniquely identified PUT applied everywhere proves convergence.
    Marker,
    /// The marker key was created in the baseline and is overwritten with an equal-size value,
    /// so a full live quota cannot refuse it (DM-6).
    PreCreatedMarker,
    /// No marker: a certified drain, then every outstanding write resolved by status lookup
    /// on every recoverable node (DM-7).
    Drain,
}

impl HealMode {
    /// The name the run manifest records (`keyval.observation/v1`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Marker => "marker",
            Self::PreCreatedMarker => "precreated_marker",
            Self::Drain => "drain",
        }
    }
}

/// The operation mix a run's clients draw from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// PUT and GET only: W36's acceptance profile (§12.2). Three writes for every two reads.
    PutGet,
    /// §10.3's initial mix: 30% PUT, 15% DELETE, 25% CAS, 20% GET, 10% SCAN.
    Mixed,
    /// CAS contention: every client's writes go to one key, 55% CAS, so version mismatches
    /// and conflicts are the common case.
    CasContention,
    /// Read isolation: 40% GET and 30% SCAN beside a light write load, so reads run against
    /// writes in flight.
    ReadIsolation,
    /// Boundary values: the mixed operations over only the largest, empty and escape-heavy
    /// values (§10.3) and the key corpus.
    BoundaryValues,
    /// Misuse: the mixed operations, with the misuse stream on (a request ID reused with a
    /// different command; §10.3).
    Misuse,
    /// Delayed retries: the mixed operations under message loss and delay, so clients retry
    /// and look their requests up.
    DelayedRetries,
    /// All-node restart: the mixed operations under whole-cluster crashes.
    AllNodeRestart,
    /// Asymmetric links: the mixed operations under one-way link failures.
    AsymmetricLinks,
    /// Long pauses: the mixed operations while nodes stop taking turns.
    LongPauses,
    /// Live capacity (DM-6): many writes of distinct keys against small live-key and
    /// live-byte limits, healed by an equal-size overwrite of a pre-created marker.
    LiveCapacity,
    /// History capacity (DM-7): many writes against small log-entry and history-byte limits,
    /// healed by a certified drain and status lookups, never a marker.
    HistoryCapacity,
}

/// The operation kinds a profile's roll chooses among.
#[derive(Clone, Copy)]
enum Kind {
    Put,
    Delete,
    Cas,
    Get,
    Scan,
}

impl Profile {
    /// Every profile, in the order files and help list them.
    pub const ALL: [Self; 12] = [
        Self::PutGet,
        Self::Mixed,
        Self::CasContention,
        Self::ReadIsolation,
        Self::BoundaryValues,
        Self::Misuse,
        Self::DelayedRetries,
        Self::AllNodeRestart,
        Self::AsymmetricLinks,
        Self::LongPauses,
        Self::LiveCapacity,
        Self::HistoryCapacity,
    ];

    /// The profile's name in scenario files, manifests and `--workload`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PutGet => "put_get",
            Self::Mixed => "mixed",
            Self::CasContention => "cas_contention",
            Self::ReadIsolation => "read_isolation",
            Self::BoundaryValues => "boundary_values",
            Self::Misuse => "misuse",
            Self::DelayedRetries => "delayed_retries",
            Self::AllNodeRestart => "all_node_restart",
            Self::AsymmetricLinks => "asymmetric_links",
            Self::LongPauses => "long_pauses",
            Self::LiveCapacity => "live_capacity",
            Self::HistoryCapacity => "history_capacity",
        }
    }

    /// How a run of this profile heals (§10.2): by the plain marker, by the pre-created
    /// marker (DM-6) or by a certified drain (DM-7).
    #[must_use]
    pub const fn heal_mode(self) -> HealMode {
        match self {
            Self::LiveCapacity => HealMode::PreCreatedMarker,
            Self::HistoryCapacity => HealMode::Drain,
            Self::PutGet
            | Self::Mixed
            | Self::CasContention
            | Self::ReadIsolation
            | Self::BoundaryValues
            | Self::Misuse
            | Self::DelayedRetries
            | Self::AllNodeRestart
            | Self::AsymmetricLinks
            | Self::LongPauses => HealMode::Marker,
        }
    }

    /// The fewest and most operations a client of this profile plans.
    #[must_use]
    pub const fn ops(self) -> (u64, u64) {
        match self {
            Self::LiveCapacity | Self::HistoryCapacity => CAPACITY_OPS,
            Self::PutGet
            | Self::Mixed
            | Self::CasContention
            | Self::ReadIsolation
            | Self::BoundaryValues
            | Self::Misuse
            | Self::DelayedRetries
            | Self::AllNodeRestart
            | Self::AsymmetricLinks
            | Self::LongPauses => OPS,
        }
    }

    /// The longest think time of a client of this profile, in milliseconds.
    const fn think_ms(self) -> u64 {
        match self {
            Self::LiveCapacity | Self::HistoryCapacity => CAPACITY_THINK_MS,
            Self::PutGet
            | Self::Mixed
            | Self::CasContention
            | Self::ReadIsolation
            | Self::BoundaryValues
            | Self::Misuse
            | Self::DelayedRetries
            | Self::AllNodeRestart
            | Self::AsymmetricLinks
            | Self::LongPauses => MAX_THINK_MS,
        }
    }

    /// The profile called `name`, if there is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|profile| profile.name() == name)
    }

    /// The fault types a fault-dedicated profile enables, or none for a profile that leaves
    /// the swarm to the seed.
    #[must_use]
    pub const fn dedicated_faults(self) -> &'static [FaultKind] {
        match self {
            Self::DelayedRetries => &[FaultKind::MessageLoss, FaultKind::DelayReorder],
            Self::AllNodeRestart => &[FaultKind::AllNodeCrash],
            Self::AsymmetricLinks => &[FaultKind::AsymmetricPartition],
            Self::LongPauses => &[FaultKind::Pause],
            Self::PutGet
            | Self::Mixed
            | Self::CasContention
            | Self::ReadIsolation
            | Self::BoundaryValues
            | Self::Misuse
            | Self::LiveCapacity
            | Self::HistoryCapacity => &[],
        }
    }

    /// The misuse rate, in parts per million, the profile's clients run with: zero unless
    /// the profile is [`Self::Misuse`].
    #[must_use]
    pub const fn misuse_ppm(self) -> u32 {
        match self {
            Self::Misuse => 300_000,
            Self::PutGet
            | Self::Mixed
            | Self::CasContention
            | Self::ReadIsolation
            | Self::BoundaryValues
            | Self::DelayedRetries
            | Self::AllNodeRestart
            | Self::AsymmetricLinks
            | Self::LongPauses
            | Self::LiveCapacity
            | Self::HistoryCapacity => 0,
        }
    }

    /// The operation a percentile `roll` (0 to 99) picks under the profile.
    const fn kind(self, roll: u64) -> Kind {
        match self {
            Self::PutGet => {
                if roll < 60 {
                    Kind::Put
                } else {
                    Kind::Get
                }
            }
            Self::CasContention => match roll {
                0..15 => Kind::Put,
                15..25 => Kind::Delete,
                25..80 => Kind::Cas,
                80..95 => Kind::Get,
                _ => Kind::Scan,
            },
            Self::ReadIsolation => match roll {
                0..15 => Kind::Put,
                15..20 => Kind::Delete,
                20..30 => Kind::Cas,
                30..70 => Kind::Get,
                _ => Kind::Scan,
            },
            // Write-heavy: the capacity limits are reached by writes (§10.2).
            Self::LiveCapacity | Self::HistoryCapacity => match roll {
                0..75 => Kind::Put,
                75..85 => Kind::Delete,
                85..95 => Kind::Cas,
                _ => Kind::Get,
            },
            Self::Mixed
            | Self::BoundaryValues
            | Self::Misuse
            | Self::DelayedRetries
            | Self::AllNodeRestart
            | Self::AsymmetricLinks
            | Self::LongPauses => match roll {
                0..30 => Kind::Put,
                30..45 => Kind::Delete,
                45..70 => Kind::Cas,
                70..90 => Kind::Get,
                _ => Kind::Scan,
            },
        }
    }
}

/// One logical operation a client plans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// PUT `key = value` under `request_id`.
    Put {
        request_id: RequestId,
        key: Key,
        value: Value,
    },
    /// DELETE `key` under `request_id`.
    Delete { request_id: RequestId, key: Key },
    /// Compare-and-swap `key` to `value` under `request_id`, expecting the version the
    /// client last saw for the key when the operation starts.
    Cas {
        request_id: RequestId,
        key: Key,
        value: Value,
    },
    /// GET `key`.
    Get { key: Key },
    /// SCAN every key: one page of [`PageSize::DEFAULT`] items.
    Scan { range: ScanRange },
}

impl Intent {
    /// The request ID, for a write.
    #[must_use]
    pub const fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Put { request_id, .. }
            | Self::Delete { request_id, .. }
            | Self::Cas { request_id, .. } => Some(*request_id),
            Self::Get { .. } | Self::Scan { .. } => None,
        }
    }

    /// The key a PUT, DELETE, CAS or GET names.
    #[must_use]
    pub const fn key(&self) -> Option<&Key> {
        match self {
            Self::Put { key, .. }
            | Self::Delete { key, .. }
            | Self::Cas { key, .. }
            | Self::Get { key } => Some(key),
            Self::Scan { .. } => None,
        }
    }
}

/// A planned operation and the think time before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// What the client does.
    pub intent: Intent,
    /// How long it waits after its previous operation ends, in milliseconds.
    pub think_ms: u64,
}

/// One client's plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPlan {
    /// The client.
    pub client: ClientId,
    /// Its operations, in order.
    pub ops: Vec<Planned>,
}

/// A run's workload.
///
/// Invariants: 1 to [`MAX_CLIENTS`] clients; between [`KEYS`]'s bounds of distinct keys,
/// each valid under the limits it was generated for; `hot` is at least 1 and at most the
/// number of keys; every write's request ID is unique in the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The clients' plans.
    pub clients: Vec<ClientPlan>,
    /// The primary keys.
    pub keys: Vec<Key>,
    /// The hot subset: the first `hot` keys.
    pub hot: usize,
}

/// A number below `bound` (at least 1) from `rng`.
fn below(rng: &mut SplitMix64, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

/// An index below `len` (at least 1) from `rng`.
fn pick(rng: &mut SplitMix64, len: usize) -> usize {
    usize::try_from(below(rng, u64::try_from(len).unwrap_or(u64::MAX))).unwrap_or(0)
}

/// §10.3's key edge cases that fit `limits`, in a fixed order.
fn key_corpus(limits: KeyValueLimits) -> Vec<Key> {
    let longest = limits.max_key_bytes();
    [
        "a".to_owned(),
        "aa".to_owned(),
        "b".to_owned(),
        "\u{e9}".to_owned(),
        "e\u{301}".to_owned(),
        "\u{65e5}\u{672c}".to_owned(),
        "a/b".to_owned(),
        "q?x=1".to_owned(),
        "x&y".to_owned(),
        "100%".to_owned(),
        "/?&%".to_owned(),
        "m".repeat(longest),
        "\u{e9}".repeat(longest / 2),
    ]
    .into_iter()
    .filter_map(|text| Key::parse(text, limits).ok())
    .collect()
}

/// A value of a kind drawn from `rng`, for a write to `key` whose last planned value is
/// `last` (§10.3's value edge cases, as far as `limits` allow).
fn value(
    rng: &mut SplitMix64,
    limits: KeyValueLimits,
    last: Option<&Value>,
    boundary: bool,
) -> Value {
    let longest = limits.max_value_bytes();
    // Boundary values draw only the empty, longest and escape-heavy kinds.
    let roll = if boundary {
        [0, 5, 6][pick(rng, 3)]
    } else {
        below(rng, 20)
    };
    let text = match roll {
        0..=1 => String::new(),
        2..=4 => match last {
            Some(last) => return last.clone(),
            None => format!("v{}", below(rng, 1_000)),
        },
        5 => "z".repeat(longest),
        6..=8 => "\"\\\u{1}\n\t".repeat(1 + pick(rng, 8)),
        9..=11 => "y".repeat(40),
        _ => format!("v{}", below(rng, 1_000)),
    };
    // Every kind is cut to the limit, so the value always parses.
    let mut text = text;
    while text.len() > longest {
        text.pop();
    }
    Value::parse(text, limits).unwrap_or(Value::EMPTY)
}

impl Plan {
    /// The workload drawn from `rng` for keys and values valid under `limits`, under
    /// `profile`.
    #[must_use]
    pub fn generate(rng: &mut SplitMix64, limits: KeyValueLimits, profile: Profile) -> Self {
        let clients = 1 + below(rng, MAX_CLIENTS);
        let wanted = usize::try_from(KEYS.0 + below(rng, KEYS.1 - KEYS.0 + 1)).unwrap_or(4);
        // The corpus in a seeded order, then generated keys up to the count.
        let mut corpus = key_corpus(limits);
        let mut keys = Vec::with_capacity(wanted);
        while keys.len() < wanted && !corpus.is_empty() {
            let at = pick(rng, corpus.len());
            keys.push(corpus.remove(at));
        }
        let mut n = 0;
        while keys.len() < wanted {
            if let Ok(key) = Key::parse(format!("k{n}"), limits)
                && !keys.contains(&key)
            {
                keys.push(key);
            }
            n += 1;
        }
        let hot = (keys.len() / 4).max(1);
        let mut last: BTreeMap<Key, Value> = BTreeMap::new();
        let clients = (0..clients)
            .map(|number| {
                let (fewest, most) = profile.ops();
                let count = fewest + below(rng, most - fewest + 1);
                let ops = (0..count)
                    .map(|op| {
                        let request_id = request_id(rng, number, op);
                        let write_key = if profile == Profile::CasContention {
                            keys[0].clone()
                        } else if below(rng, 5) < 3 {
                            keys[pick(rng, hot)].clone()
                        } else {
                            keys[pick(rng, keys.len())].clone()
                        };
                        let read_key = keys[pick(rng, keys.len())].clone();
                        let boundary = profile == Profile::BoundaryValues;
                        let intent = match profile.kind(below(rng, 100)) {
                            Kind::Put => {
                                let value = value(rng, limits, last.get(&write_key), boundary);
                                last.insert(write_key.clone(), value.clone());
                                Intent::Put {
                                    request_id,
                                    key: write_key,
                                    value,
                                }
                            }
                            Kind::Delete => Intent::Delete {
                                request_id,
                                key: write_key,
                            },
                            Kind::Cas => {
                                let value = value(rng, limits, last.get(&write_key), boundary);
                                last.insert(write_key.clone(), value.clone());
                                Intent::Cas {
                                    request_id,
                                    key: write_key,
                                    value,
                                }
                            }
                            Kind::Get => Intent::Get { key: read_key },
                            Kind::Scan => ScanRange::new(None, None, None, PageSize::DEFAULT)
                                .map_or(Intent::Get { key: read_key }, |range| Intent::Scan {
                                    range,
                                }),
                        };
                        Planned {
                            intent,
                            think_ms: below(rng, profile.think_ms() + 1),
                        }
                    })
                    .collect();
                ClientPlan {
                    client: ClientId::new(number),
                    ops,
                }
            })
            .collect();
        Self { clients, keys, hot }
    }
}

/// The request ID of client `client`'s operation `op`: eight seeded bytes, then the client
/// and the operation, so it is unique in the run.
fn request_id(rng: &mut SplitMix64, client: u64, op: u64) -> RequestId {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
    bytes[8..12].copy_from_slice(&u32::try_from(client).unwrap_or(u32::MAX).to_be_bytes());
    bytes[12..].copy_from_slice(&u32::try_from(op).unwrap_or(u32::MAX).to_be_bytes());
    RequestId::from_bytes(bytes)
}

/// How a logical operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Its terminal answer: a written result, a conflict, a read's answer, or a status
    /// lookup that knew the result.
    Done(Response),
    /// A write refused before proposal by `history_full` that was never submitted.
    Refused,
    /// The retry bounds ran out first: unresolved, never failed (§8.7).
    Unresolved(Exhausted),
    /// The write never executed and never can: after a certified drain (§10.2, D-53) a status
    /// lookup on every recoverable node answered `unknown`. The attempts are those lookups,
    /// one per recoverable node, the evidence for the resolution.
    NotExecuted(Vec<AttemptId>),
}

/// How one logical operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The client.
    pub client: ClientId,
    /// Its planned operation's position.
    pub op: usize,
    /// The request ID, for a write.
    pub request_id: Option<RequestId>,
    /// The sends it took, status lookups included.
    pub sends: u64,
    /// How it ended.
    pub end: End,
    /// When, in virtual milliseconds.
    pub at_ms: u64,
    /// The attempt whose answer ended it, if an answer did: the evidence for a write's
    /// resolution (§10.2). `None` when the retry bounds ran out on silence.
    pub decided_by: Option<AttemptId>,
}

/// One attempt a client sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentAttempt {
    /// The client.
    pub client: ClientId,
    /// Its planned operation's position.
    pub op: usize,
    /// The attempt.
    pub attempt: AttemptId,
    /// Where it went.
    pub node: NodeId,
    /// When it arrives there, in virtual milliseconds.
    pub at_ms: u64,
    /// What it asked: the operation itself, or a status lookup for the write.
    pub request: ClientOp,
    /// Whether it is a misuse attempt: a request ID reused with a different command (§10.3).
    /// A misuse attempt is never part of a logical write.
    pub misuse: bool,
}

/// A logical operation in progress: its planned position, its exact request (fixed when it
/// started) and its retry sequence.
#[derive(Clone, Debug)]
struct Op {
    index: usize,
    request: ClientOp,
    retry: Retry,
    /// The attempt whose answer the policy is following now.
    answered: Option<AttemptId>,
}

impl Op {
    const fn request_id(&self) -> Option<RequestId> {
        match &self.request {
            ClientOp::Write { request_id, .. } => Some(*request_id),
            ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
        }
    }

    /// The key a write or GET names.
    const fn key(&self) -> Option<&Key> {
        match &self.request {
            ClientOp::Write { mutation, .. } => Some(mutation.key()),
            ClientOp::Get { key } => Some(key),
            ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
        }
    }
}

/// An operation waiting on its last attempt.
#[derive(Clone, Debug)]
struct Running {
    op: Op,
    attempt: AttemptId,
    timeout_ms: u64,
}

/// One client's progress through its plan.
#[derive(Clone, Debug, Default)]
struct Progress {
    next: usize,
    ready_ms: u64,
    running: Option<Running>,
    /// The version this client last saw for each key, `None` for absent.
    seen: BTreeMap<Key, Option<Version>>,
}

/// A run's clients: the plan, and each client's progress through it.
#[derive(Debug)]
pub struct Clients {
    plan: Plan,
    bounds: RetryBounds,
    nodes: Vec<NodeId>,
    rng: SplitMix64,
    response_timeout_ms: u64,
    progress: Vec<Progress>,
    heard: usize,
    now_ms: u64,
    sends: Vec<SentAttempt>,
    outcomes: Vec<Outcome>,
    misuse_ppm: u32,
    /// Writes whose retry bounds ran out, each with its client, awaiting [`Self::resume_parked`].
    parked: Vec<(usize, Op)>,
    /// Writes being resolved by status lookups on every recoverable node after a certified
    /// drain ([`Self::begin_lookups`]).
    lookups: Vec<Lookup>,
}

/// How many status lookups one node gets for one write before the resolution gives up on it
/// (CD-9): the first and the resends after silence.
pub const LOOKUP_SENDS: u64 = 6;

/// What a node's status lookup has told so far.
#[derive(Clone, Debug)]
enum Told {
    /// No usable answer yet.
    Waiting,
    /// The node stored a terminal result.
    Known(Response),
    /// The node has no result for the request ID.
    Unknown,
}

/// A status lookup on one node.
#[derive(Clone, Debug)]
struct Ask {
    node: NodeId,
    /// The latest attempt sent.
    attempt: AttemptId,
    timeout_ms: u64,
    sends: u64,
    told: Told,
}

/// A parked write being resolved by status lookups: one [`Ask`] per recoverable node.
#[derive(Clone, Debug)]
struct Lookup {
    client: usize,
    op: Op,
    asks: Vec<Ask>,
}

/// How long, in virtual milliseconds, a run lets in-flight misuse attempts land once every
/// client has finished.
const DRAIN_MS: u64 = 5_000;

/// One in a million: the unit of a misuse rate.
const MILLION: u64 = 1_000_000;

impl Clients {
    /// The clients of `plan` against `config`'s voters, drawing their run-time choices from
    /// `rng`. A response not heard within the write deadline and one more second is
    /// silence.
    #[must_use]
    pub fn new(plan: Plan, config: &Config, rng: SplitMix64) -> Self {
        let progress = vec![Progress::default(); plan.clients.len()];
        Self {
            bounds: RetryBounds::of(config),
            nodes: config.voters().collect(),
            response_timeout_ms: config
                .write_deadline()
                .get()
                .saturating_mul(config.tick_ms())
                .saturating_add(1_000),
            plan,
            rng,
            progress,
            heard: 0,
            now_ms: 0,
            sends: Vec::new(),
            outcomes: Vec::new(),
            misuse_ppm: 0,
            parked: Vec::new(),
            lookups: Vec::new(),
        }
    }

    /// Holds every client of the plan back: none starts an operation until
    /// [`Self::release_plan`]. Clients added later are not held.
    pub fn hold_plan(&mut self) {
        for progress in &mut self.progress {
            progress.ready_ms = u64::MAX;
        }
    }

    /// Lets the plan's clients start, their think times counted from `now_ms`.
    pub fn release_plan(&mut self, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
        let planned = self.plan.clients.len().min(self.progress.len());
        for progress in self.progress.iter_mut().take(planned) {
            if progress.ready_ms == u64::MAX {
                progress.ready_ms = now_ms;
            }
        }
    }

    /// Adds a client with `ops`, starting at `now_ms`, and returns it. Its operations are
    /// recorded with the plan's, so the run's observation covers them.
    pub fn add_client(&mut self, ops: Vec<Planned>, now_ms: u64) -> ClientId {
        let client = ClientId::new(u64::try_from(self.progress.len()).unwrap_or(u64::MAX));
        self.progress.push(Progress {
            ready_ms: now_ms,
            ..Progress::default()
        });
        self.plan.clients.push(ClientPlan { client, ops });
        client
    }

    /// Makes every retry sequence started from now on choose among `nodes` only; sequences
    /// already running keep theirs. The heal uses it to point resumed writes and the marker at
    /// the nodes that can still answer (§10.2). `nodes` must be voters of the cluster.
    pub fn retry_among(&mut self, nodes: &[NodeId]) {
        let chosen: Vec<NodeId> = self
            .nodes
            .iter()
            .copied()
            .filter(|node| nodes.contains(node))
            .collect();
        types::invariant!(
            !chosen.is_empty(),
            "CLIENTS-NODES",
            "a client needs a node to retry on: asked for {nodes:?} among {:?}",
            self.nodes
        );
        self.nodes = chosen;
    }

    /// Stops every client starting operations: each plan keeps only the operations already
    /// started, so those in flight run to their ends and nothing new begins (§10.2: client
    /// writes stop at the heal).
    pub fn stop_starting(&mut self) {
        for (plan, progress) in self.plan.clients.iter_mut().zip(&self.progress) {
            plan.ops.truncate(progress.next);
        }
    }

    /// How many operations are waiting on an attempt now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.progress
            .iter()
            .filter(|progress| progress.running.is_some())
            .count()
    }

    /// How many writes ended unresolved (their retry bounds ran out) and have not been
    /// resumed.
    #[must_use]
    pub const fn parked(&self) -> usize {
        self.parked.len()
    }

    /// Resumes parked writes: each idle client with a parked write retries it with a fresh
    /// retry sequence, the same request ID and the same command (§10.2's heal). The write's
    /// unresolved outcome is superseded by the one its resumed sequence reaches. Returns how
    /// many were resumed; the others wait until their client is idle again.
    ///
    /// # Errors
    ///
    /// [`ClusterError`] from scheduling the attempts.
    pub fn resume_parked(&mut self, cluster: &mut Cluster) -> Result<usize, ClusterError> {
        self.now_ms = self.now_ms.max(cluster.now().as_micros() / 1_000);
        let mut resumed = 0;
        for at in 0..self.progress.len() {
            if self.progress[at].running.is_some() {
                continue;
            }
            let Some(position) = self.parked.iter().position(|(client, _)| *client == at) else {
                continue;
            };
            let (_, parked) = self.parked.remove(position);
            let client = ClientId::new(u64::try_from(at).unwrap_or(u64::MAX));
            if let Some(ended) = self
                .outcomes
                .iter()
                .position(|outcome| outcome.client == client && outcome.op == parked.index)
            {
                self.outcomes.remove(ended);
            }
            let first = self.nodes[pick(&mut self.rng, self.nodes.len())];
            let (retry, next) =
                Retry::start(self.bounds, OpKind::Write, &self.nodes, first, self.now_ms);
            let op = Op {
                retry,
                answered: None,
                ..parked
            };
            self.apply(cluster, at, op, next, None)?;
            resumed += 1;
        }
        Ok(resumed)
    }

    /// How many writes are still being resolved by status lookups.
    #[must_use]
    pub const fn lookups_open(&self) -> usize {
        self.lookups.len()
    }

    /// Starts resolving every parked write by a status lookup on each of `nodes` — the
    /// recoverable nodes — once `drained` certifies the cluster (§10.2, D-53). A write
    /// resolves as executed when any node stores its result, and as not executed when every
    /// node answers `unknown`; lookups lost to silence are resent up to [`LOOKUP_SENDS`]
    /// times per node. Returns how many writes it started on. A write that never gets every
    /// answer stays unresolved.
    ///
    /// # Errors
    ///
    /// [`ClusterError`] from scheduling the lookups.
    pub fn begin_lookups(
        &mut self,
        cluster: &mut Cluster,
        drained: &Certified,
        nodes: &[NodeId],
    ) -> Result<usize, ClusterError> {
        types::invariant!(
            drained.quiet_ticks() > 0 && !nodes.is_empty(),
            "CLIENTS-LOOKUPS",
            "lookups need a certified drain over a quiet period and at least one node: \
             quiet_ticks={} nodes={nodes:?}",
            drained.quiet_ticks()
        );
        self.now_ms = self.now_ms.max(cluster.now().as_micros() / 1_000);
        let parked = std::mem::take(&mut self.parked);
        let started = parked.len();
        for (client, op) in parked {
            let mut asks = Vec::new();
            for node in nodes {
                let (attempt, timeout_ms) = self.ask(cluster, client, &op, *node)?;
                asks.push(Ask {
                    node: *node,
                    attempt,
                    timeout_ms,
                    sends: 1,
                    told: Told::Waiting,
                });
            }
            self.lookups.push(Lookup { client, op, asks });
        }
        Ok(started)
    }

    /// Sends one status lookup of `op`'s write to `node`; returns the attempt and its
    /// silence deadline.
    fn ask(
        &mut self,
        cluster: &mut Cluster,
        client: usize,
        op: &Op,
        node: NodeId,
    ) -> Result<(AttemptId, u64), ClusterError> {
        let Some(request_id) = op.request_id() else {
            types::invariant!(
                false,
                "LOOKUP-REQUEST",
                "a lookup is only asked for a write with a request id: client={client} op={}",
                op.index
            );
            return Err(ClusterError::Setup("a lookup needs a request id"));
        };
        let request = ClientOp::Status { request_id };
        let id = ClientId::new(u64::try_from(client).unwrap_or(u64::MAX));
        let attempt = cluster.submit(
            VirtualTime::from_micros(self.now_ms.saturating_mul(1_000)),
            id,
            node,
            request.clone(),
        )?;
        self.sends.push(SentAttempt {
            client: id,
            op: op.index,
            attempt,
            node,
            at_ms: self.now_ms,
            request,
            misuse: false,
        });
        Ok((
            attempt,
            self.now_ms.saturating_add(self.response_timeout_ms),
        ))
    }

    /// Records `response` to the lookup attempt `attempt`, if a lookup waits on it. Returns
    /// whether one did.
    fn hear_lookup(&mut self, attempt: AttemptId, response: &Response) -> bool {
        // A non-answer waits a full response timeout before the resend, not a busy loop.
        let retry_at = self.now_ms.saturating_add(self.response_timeout_ms);
        for lookup in &mut self.lookups {
            let Some(ask) = lookup.asks.iter_mut().find(|ask| ask.attempt == attempt) else {
                continue;
            };
            match ask.told {
                Told::Waiting => match response {
                    Response::Status {
                        state: RequestState::Known(_),
                        ..
                    } => ask.told = Told::Known(response.clone()),
                    Response::Status {
                        state: RequestState::Unknown,
                        ..
                    } => ask.told = Told::Unknown,
                    // Not an answer about the request (refused, unavailable, ...): ask again
                    // at the next look.
                    Response::Written { .. }
                    | Response::Conflict { .. }
                    | Response::Found { .. }
                    | Response::NotFound { .. }
                    | Response::Page(_)
                    | Response::Refused(_) => ask.timeout_ms = retry_at,
                },
                Told::Known(_) | Told::Unknown => {}
            }
            return true;
        }
        false
    }

    /// Resends the lookups whose answer did not come in time.
    fn hear_lookup_silences(&mut self, cluster: &mut Cluster) -> Result<(), ClusterError> {
        for at in 0..self.lookups.len() {
            for ask_at in 0..self.lookups[at].asks.len() {
                let ask = &self.lookups[at].asks[ask_at];
                let silent = matches!(ask.told, Told::Waiting)
                    && ask.timeout_ms <= self.now_ms
                    && ask.sends < LOOKUP_SENDS;
                if !silent {
                    continue;
                }
                let node = ask.node;
                let client = self.lookups[at].client;
                let op = self.lookups[at].op.clone();
                let (attempt, timeout_ms) = self.ask(cluster, client, &op, node)?;
                let ask = &mut self.lookups[at].asks[ask_at];
                ask.attempt = attempt;
                ask.timeout_ms = timeout_ms;
                ask.sends += 1;
            }
        }
        Ok(())
    }

    /// Settles every lookup that has its answer: any stored result resolves the write as
    /// executed; every node saying `unknown` resolves it as not executed.
    fn settle_lookups(&mut self) {
        let mut open = Vec::new();
        for lookup in std::mem::take(&mut self.lookups) {
            let known = lookup.asks.iter().find_map(|ask| match &ask.told {
                Told::Known(response) => Some((ask.attempt, response.clone())),
                Told::Waiting | Told::Unknown => None,
            });
            let all_unknown = lookup
                .asks
                .iter()
                .all(|ask| matches!(ask.told, Told::Unknown));
            let sends: u64 = lookup.asks.iter().map(|ask| ask.sends).sum();
            let (end, decided_by) = match known {
                Some((attempt, response)) => (End::Done(response), Some(attempt)),
                None if all_unknown => (
                    End::NotExecuted(lookup.asks.iter().map(|ask| ask.attempt).collect()),
                    None,
                ),
                None => {
                    open.push(lookup);
                    continue;
                }
            };
            let client = ClientId::new(u64::try_from(lookup.client).unwrap_or(u64::MAX));
            let found = self
                .outcomes
                .iter_mut()
                .find(|outcome| outcome.client == client && outcome.op == lookup.op.index);
            types::invariant!(
                found.is_some(),
                "LOOKUP-OUTCOME",
                "a settled lookup has its outcome record: client={client:?} op={}",
                lookup.op.index
            );
            if let Some(ended) = found {
                ended.end = end;
                ended.decided_by = decided_by;
                ended.sends += sends;
                ended.at_ms = self.now_ms;
            }
        }
        self.lookups = open;
    }

    /// These clients with the misuse stream on: each write that ends with a terminal result
    /// is followed by one misuse attempt with probability `ppm` in a million (capped at all
    /// of them).
    #[must_use]
    pub fn with_misuse(mut self, ppm: u32) -> Self {
        self.misuse_ppm = ppm.min(1_000_000);
        self
    }

    /// The plan.
    #[must_use]
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Every attempt sent so far, in order.
    #[must_use]
    pub fn sends(&self) -> &[SentAttempt] {
        &self.sends
    }

    /// Every logical operation's end so far, in order.
    #[must_use]
    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }

    /// Whether every client has ended every planned operation.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.progress
            .iter()
            .zip(&self.plan.clients)
            .all(|(progress, plan)| progress.running.is_none() && progress.next >= plan.ops.len())
            && self.lookups.is_empty()
    }

    /// Runs the cluster and the clients until `until`, or until every client has finished.
    ///
    /// # Errors
    ///
    /// [`ClusterError`] from driving the cluster.
    pub fn run(&mut self, cluster: &mut Cluster, until: VirtualTime) -> Result<(), ClusterError> {
        while cluster.now() < until && !self.finished() {
            let step = VirtualTime::from_micros(
                cluster
                    .now()
                    .as_micros()
                    .saturating_add(POLL_MS * 1_000)
                    .min(until.as_micros()),
            );
            cluster.run_until(step)?;
            self.now_ms = step.as_micros() / 1_000;
            self.hear_answers(cluster)?;
            self.hear_silences(cluster)?;
            self.hear_lookup_silences(cluster)?;
            self.settle_lookups();
            self.start_ready(cluster)?;
        }
        if self.misuse_ppm > 0 {
            // A misuse attempt sent by the last operation is still in flight: let it land,
            // so the stream records its invocation and its answer (W36.2 review).
            let end = cluster
                .now()
                .as_micros()
                .saturating_add(DRAIN_MS * 1_000)
                .min(until.as_micros());
            cluster.run_until(VirtualTime::from_micros(end))?;
        }
        Ok(())
    }

    /// Feeds every answer that reached a client since the last step to the operation that
    /// waits on it. Answers to other attempts are stale and are ignored.
    fn hear_answers(&mut self, cluster: &mut Cluster) -> Result<(), ClusterError> {
        let fresh: Vec<(ClientId, AttemptId, Response)> = cluster
            .answers()
            .skip(self.heard)
            .map(|answer| (answer.client, answer.attempt, answer.response.clone()))
            .collect();
        self.heard += fresh.len();
        for (client, attempt, response) in fresh {
            if self.hear_lookup(attempt, &response) {
                continue;
            }
            let Some(at) = usize::try_from(client.get())
                .ok()
                .filter(|at| *at < self.progress.len())
            else {
                continue;
            };
            let waiting = self.progress[at]
                .running
                .as_ref()
                .is_some_and(|running| running.attempt == attempt);
            if waiting {
                self.advance(cluster, at, Some(&response))?;
            }
        }
        Ok(())
    }

    /// Feeds silence to every operation whose attempt's response timeout has passed.
    fn hear_silences(&mut self, cluster: &mut Cluster) -> Result<(), ClusterError> {
        for at in 0..self.progress.len() {
            let silent = self.progress[at]
                .running
                .as_ref()
                .is_some_and(|running| running.timeout_ms <= self.now_ms);
            if silent {
                self.advance(cluster, at, None)?;
            }
        }
        Ok(())
    }

    /// Starts the next planned operation of every idle client whose think time is over.
    fn start_ready(&mut self, cluster: &mut Cluster) -> Result<(), ClusterError> {
        for at in 0..self.progress.len() {
            let progress = &self.progress[at];
            let Some(planned) = self.plan.clients[at].ops.get(progress.next) else {
                continue;
            };
            if progress.running.is_some()
                || progress.ready_ms.saturating_add(planned.think_ms) > self.now_ms
            {
                continue;
            }
            let (request, kind) = self.request(at, &planned.intent);
            let first = self.nodes[pick(&mut self.rng, self.nodes.len())];
            let (retry, next) = Retry::start(self.bounds, kind, &self.nodes, first, self.now_ms);
            let index = self.progress[at].next;
            self.progress[at].next += 1;
            let op = Op {
                index,
                request,
                retry,
                answered: None,
            };
            self.apply(cluster, at, op, next, None)?;
        }
        Ok(())
    }

    /// The exact request client `at` sends for `intent`, fixed now for every retry, and its
    /// kind. A CAS expects the version the client last saw for its key, or absence.
    fn request(&self, at: usize, intent: &Intent) -> (ClientOp, OpKind) {
        match intent {
            Intent::Put {
                request_id,
                key,
                value,
            } => (
                ClientOp::Write {
                    request_id: *request_id,
                    mutation: Mutation::Put {
                        key: key.clone(),
                        value: value.clone(),
                    },
                },
                OpKind::Write,
            ),
            Intent::Delete { request_id, key } => (
                ClientOp::Write {
                    request_id: *request_id,
                    mutation: Mutation::Delete { key: key.clone() },
                },
                OpKind::Write,
            ),
            Intent::Cas {
                request_id,
                key,
                value,
            } => (
                ClientOp::Write {
                    request_id: *request_id,
                    mutation: Mutation::Cas {
                        key: key.clone(),
                        expected: self.progress[at]
                            .seen
                            .get(key)
                            .copied()
                            .flatten()
                            .map_or(ExpectedVersion::Absent, ExpectedVersion::Version),
                        value: value.clone(),
                    },
                },
                OpKind::Write,
            ),
            Intent::Get { key } => (ClientOp::Get { key: key.clone() }, OpKind::Read),
            Intent::Scan { range } => (
                ClientOp::Scan {
                    range: range.clone(),
                },
                OpKind::Read,
            ),
        }
    }

    /// Feeds client `at`'s running operation what it heard (an answer, or silence) and
    /// follows the policy.
    fn advance(
        &mut self,
        cluster: &mut Cluster,
        at: usize,
        answer: Option<&Response>,
    ) -> Result<(), ClusterError> {
        let Some(Running {
            mut op, attempt, ..
        }) = self.progress[at].running.take()
        else {
            return Ok(());
        };
        op.answered = answer.map(|_| attempt);
        let draw = self.rng.next_u64();
        let next = op.retry.heard(
            answer.map_or(Heard::Silence, Heard::Answer),
            self.now_ms,
            draw,
        );
        if let Some(response) = answer {
            self.learn(at, &op, response);
        }
        self.apply(cluster, at, op, next, answer)
    }

    /// Follows the policy's `next` step for client `at`'s operation `op`, whose last answer,
    /// if any, was `answer`.
    ///
    /// # Panics
    ///
    /// `SIM-CLIENT-DONE` if the policy calls an operation done without an answer: only an
    /// answer can end it.
    fn apply(
        &mut self,
        cluster: &mut Cluster,
        at: usize,
        op: Op,
        next: Next,
        answer: Option<&Response>,
    ) -> Result<(), ClusterError> {
        match next {
            Next::Send { node, at_ms } => self.send(cluster, at, op, (node, at_ms), false),
            Next::AskStatus { node, at_ms } => self.send(cluster, at, op, (node, at_ms), true),
            Next::Done => {
                types::invariant!(
                    answer.is_some(),
                    "SIM-CLIENT-DONE",
                    "client {at}: operation {} ended done with no answer",
                    op.index
                );
                answer.map_or(Ok(()), |response| {
                    self.end(at, &op, End::Done(response.clone()));
                    self.misuse(cluster, at, &op, response)
                })
            }
            Next::Refused => {
                self.end(at, &op, End::Refused);
                Ok(())
            }
            Next::Unresolved(exhausted) => {
                self.end(at, &op, End::Unresolved(exhausted));
                if op.request_id().is_some() {
                    self.parked.push((at, op));
                    // CD-9: each planned write parks at most once at a time.
                    let planned: usize = self.plan.clients.iter().map(|plan| plan.ops.len()).sum();
                    types::invariant!(
                        self.parked.len() <= planned,
                        "CLIENTS-PARKED-BOUND",
                        "parked writes {} exceed the {planned} planned operations",
                        self.parked.len()
                    );
                }
                Ok(())
            }
        }
    }

    /// Sends client `at`'s operation `op` (a status lookup for its request ID if `ask`) to
    /// `node`, arriving at `at_ms`, and waits on it.
    fn send(
        &mut self,
        cluster: &mut Cluster,
        at: usize,
        op: Op,
        (node, at_ms): (NodeId, u64),
        ask: bool,
    ) -> Result<(), ClusterError> {
        let request = match (ask, op.request_id()) {
            (true, Some(request_id)) => ClientOp::Status { request_id },
            (true | false, _) => op.request.clone(),
        };
        let at_ms = at_ms.max(self.now_ms);
        let client = ClientId::new(u64::try_from(at).unwrap_or(u64::MAX));
        let attempt = cluster.submit(
            VirtualTime::from_micros(at_ms.saturating_mul(1_000)),
            client,
            node,
            request.clone(),
        )?;
        self.sends.push(SentAttempt {
            client,
            op: op.index,
            attempt,
            node,
            at_ms,
            request,
            misuse: false,
        });
        self.progress[at].running = Some(Running {
            op,
            attempt,
            timeout_ms: at_ms.saturating_add(self.response_timeout_ms),
        });
        Ok(())
    }

    /// Ends client `at`'s operation `op` with `end`: recorded, and the client's think time
    /// starts now.
    fn end(&mut self, at: usize, op: &Op, end: End) {
        self.outcomes.push(Outcome {
            client: ClientId::new(u64::try_from(at).unwrap_or(u64::MAX)),
            op: op.index,
            request_id: op.request_id(),
            sends: op.retry.sends(),
            end,
            at_ms: self.now_ms,
            decided_by: op.answered,
        });
        self.progress[at].ready_ms = self.now_ms;
    }

    /// After client `at`'s write `op` ended with `response`, maybe reuses its request ID with
    /// a different command, once, with no retry (§10.3). Only an observed terminal result
    /// earns a misuse attempt, so the system's answer is decided: `request_id_conflict`.
    fn misuse(
        &mut self,
        cluster: &mut Cluster,
        at: usize,
        op: &Op,
        response: &Response,
    ) -> Result<(), ClusterError> {
        let terminal = match response {
            Response::Written { .. }
            | Response::Status {
                state: RequestState::Known(_),
                ..
            } => true,
            Response::Status {
                state: RequestState::Unknown,
                ..
            }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Refused(_) => false,
        };
        let ClientOp::Write {
            request_id,
            mutation,
        } = &op.request
        else {
            return Ok(());
        };
        if self.misuse_ppm == 0
            || !terminal
            || below(&mut self.rng, MILLION) >= u64::from(self.misuse_ppm)
        {
            return Ok(());
        }
        let other = match mutation {
            Mutation::Put { key, .. } | Mutation::Cas { key, .. } => {
                Mutation::Delete { key: key.clone() }
            }
            Mutation::Delete { key } => Mutation::Put {
                key: key.clone(),
                value: Value::EMPTY,
            },
        };
        let answered_by = match response {
            Response::Written { served_by, .. } | Response::Status { served_by, .. } => {
                Some(served_by.node)
            }
            Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Refused(_) => None,
        };
        let node = answered_by
            .filter(|node| self.nodes.contains(node))
            .unwrap_or_else(|| self.nodes[pick(&mut self.rng, self.nodes.len())]);
        let request = ClientOp::Write {
            request_id: *request_id,
            mutation: other,
        };
        let client = ClientId::new(u64::try_from(at).unwrap_or(u64::MAX));
        let attempt = cluster.submit(
            VirtualTime::from_micros(self.now_ms.saturating_mul(1_000)),
            client,
            node,
            request.clone(),
        )?;
        self.sends.push(SentAttempt {
            client,
            op: op.index,
            attempt,
            node,
            at_ms: self.now_ms,
            request,
            misuse: true,
        });
        Ok(())
    }

    /// What `response` to client `at`'s operation `op` tells it about the op's key's
    /// version, for a later CAS (§10.3).
    fn learn(&mut self, at: usize, op: &Op, response: &Response) {
        let Some(key) = op.key() else {
            return;
        };
        let learned = match response {
            Response::Found { version, .. } => Learned::At(*version),
            Response::NotFound { .. } => Learned::Absent,
            Response::Written { result, .. } => Learned::after(result),
            Response::Status { state, .. } => match state {
                RequestState::Known(result) => Learned::after(result),
                RequestState::Unknown => Learned::Nothing,
            },
            Response::Conflict { .. } | Response::Page(_) | Response::Refused(_) => {
                Learned::Nothing
            }
        };
        let seen = match learned {
            Learned::At(version) => Some(version),
            Learned::Absent => None,
            Learned::Nothing => return,
        };
        self.progress[at].seen.insert(key.clone(), seen);
    }
}

/// What an answer says about a key's version.
#[derive(Clone, Copy, Debug)]
enum Learned {
    /// The key is at this version.
    At(Version),
    /// The key is absent.
    Absent,
    /// Nothing.
    Nothing,
}

impl Learned {
    /// What a write's terminal result says: set (at the execution index), deleted (absent),
    /// or a mismatch's current version; a capacity refusal says nothing.
    const fn after(result: &types::TerminalResult) -> Self {
        match result.decision() {
            types::Decision::Set => match result.version() {
                Some(version) => Self::At(version),
                None => Self::Nothing,
            },
            types::Decision::Deleted { .. }
            | types::Decision::VersionMismatch { current: None } => Self::Absent,
            types::Decision::VersionMismatch {
                current: Some(version),
            } => Self::At(version),
            types::Decision::CapacityExceeded { .. } => Self::Nothing,
        }
    }
}
