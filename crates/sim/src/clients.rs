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
use crate::entropy::SplitMix64;
use crate::scheduler::VirtualTime;
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

/// The operation mix a run's clients draw from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// PUT and GET only: W36's acceptance profile (§12.2). Three writes for every two reads.
    PutGet,
    /// §10.3's initial mix: 30% PUT, 15% DELETE, 25% CAS, 20% GET, 10% SCAN. W43's profiles
    /// run it; the client model is ready for it.
    Mixed,
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
fn value(rng: &mut SplitMix64, limits: KeyValueLimits, last: Option<&Value>) -> Value {
    let longest = limits.max_value_bytes();
    let text = match below(rng, 20) {
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
                let count = OPS.0 + below(rng, OPS.1 - OPS.0 + 1);
                let ops = (0..count)
                    .map(|op| {
                        let request_id = request_id(rng, number, op);
                        let write_key = if below(rng, 5) < 3 {
                            keys[pick(rng, hot)].clone()
                        } else {
                            keys[pick(rng, keys.len())].clone()
                        };
                        let read_key = keys[pick(rng, keys.len())].clone();
                        let roll = below(rng, 100);
                        let intent = match profile {
                            Profile::PutGet if roll < 60 => {
                                let value = value(rng, limits, last.get(&write_key));
                                last.insert(write_key.clone(), value.clone());
                                Intent::Put {
                                    request_id,
                                    key: write_key,
                                    value,
                                }
                            }
                            Profile::PutGet => Intent::Get { key: read_key },
                            Profile::Mixed => match roll {
                                0..30 => {
                                    let value = value(rng, limits, last.get(&write_key));
                                    last.insert(write_key.clone(), value.clone());
                                    Intent::Put {
                                        request_id,
                                        key: write_key,
                                        value,
                                    }
                                }
                                30..45 => Intent::Delete {
                                    request_id,
                                    key: write_key,
                                },
                                45..70 => {
                                    let value = value(rng, limits, last.get(&write_key));
                                    last.insert(write_key.clone(), value.clone());
                                    Intent::Cas {
                                        request_id,
                                        key: write_key,
                                        value,
                                    }
                                }
                                70..90 => Intent::Get { key: read_key },
                                _ => ScanRange::new(None, None, None, PageSize::DEFAULT)
                                    .map_or(Intent::Get { key: read_key }, |range| Intent::Scan {
                                        range,
                                    }),
                            },
                        };
                        Planned {
                            intent,
                            think_ms: below(rng, MAX_THINK_MS + 1),
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
        }
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
