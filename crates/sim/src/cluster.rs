//! The simulation composition root (PLAN.md §7.13, §10.2, W17.1): the real node cores of
//! a configured cluster, wired to the simulated disk, network, clock and entropy, in one
//! thread.
//!
//! Each node is the production core, unchanged (T-2, P3): a [`node::Controller`] with the
//! production request lifecycle ([`node::RequestLifecycle`]) over the `raft-rs` adapter and
//! the journal engine. It starts the way production does (§7.7): its journal is recovered
//! from its own [`SimDisk`], the committed log is replayed, and the library is built at the
//! replayed index. Only the adapters differ:
//! - **Disk:** every storage effect runs against the node's [`SimDisk`] through the one
//!   byte-storage port ([`types::execute`]). Appends and truncations land in the volatile
//!   image at once; a sync becomes durable later, at a seeded latency, as its own event
//!   ([`Happening::Persist`]), and its completion reaches the node after another seeded
//!   latency. So a crash can fall before a sync is durable (the unsynced bytes go to the
//!   crash model, DUR-9) or after it but before the completion (which then reaches the next
//!   incarnation as a stale one, §10.2).
//! - **Network:** every sealed peer envelope and every client response travels through the
//!   [`Network`], with its seeded delay, loss and duplication; each copy's arrival is an
//!   event.
//! - **Clock:** each node incarnation ticks every `tick_ms`, as an event.
//! - **Entropy:** each node incarnation draws from its own stream split from the master
//!   seed ([`MasterSeed::node`]).
//! - **Clients** drive the inbound port directly with domain requests (§10.2: the HTTP
//!   adapter is not simulated); a request's arrival at a node is an event.
//!
//! **Driving.** [`Cluster::run_until`] dispatches the events due by a time, looking at the
//! next event's time without scheduling anything of its own, so how often a driver polls
//! never changes the run: its events, their tie order or its trace.
//!
//! **Crash and restart.** A crash applies the disk's crash model (a seed-chosen prefix of the
//! unsynced bytes survives, DUR-9) and discards the incarnation: its controller, its handles
//! and its pending ticks. A restart is a new incarnation started from whatever bytes
//! survived, through recovery and replay (DUR-3). Completions and packets already scheduled
//! still arrive: a completion from the earlier incarnation reaches the new one, which
//! ignores it (§7.2); a packet that reaches a node while it is down is recorded as
//! undeliverable.
//!
//! **Recording.** Every dispatched event starts a step of the canonical [`Trace`]. Within
//! it are recorded the packets the network moved, the disk operations and completions,
//! every client attempt's invocation and the first copy of its response (§10.3's history:
//! client, attempt, target node, the write's request ID if it is one, operation and
//! outcome), each request's first execution, and, after every step, what the [`GodView`]
//! saw change (role, log, commit, applied and journal positions, [`Changes`]).
//!
//! **First executions** are found from the mirrored log alone, never from the core's own
//! request history (§10.5): each node incarnation walks the entries it applies, in index
//! order, and the first entry carrying a request ID is that request's first execution on
//! that node. The trace records it the first time any node applies it, and again if a node
//! finds a different index for it, which is the evidence K-2 rejects.
//!
//! **The god view's log.** The controller does not expose its Raft log, so the root keeps a
//! mirror of each node's log, hard state and acceptance terms, built only from the journal
//! frames the node itself appended successfully, decoded by the journal crate
//! (`journal::decode`, [`RaftBatch`]). A batch's entries replace the log from their first
//! index, as recovery applies them (§7.6), and each entry is accepted in the term the node
//! held when the batch was written; entries replaced that way are no longer called synced
//! until the next sync is durable. On restart the mirror is rebuilt from the recovered
//! journal's valid prefix the same way. After every step the mirror's last index and term
//! must equal the core's (a failed node, which stops appending, is exempt), and
//! [`Cluster::audit_mirrors`] checks the whole mirror against the disk.

use std::fmt;

use journal::{FrameKind, Genesis, LogBounds, LogicalLog, RaftBatch, RecoveryError};
use node::{Boot, Controller, RequestLifecycle, StartError};
use statemachine::{Command, codec};
use types::{
    AttemptId, BoundedMap, BoundedQueue, ByteCount, ByteStorage, ClientOp, Config, DataDir, Effect,
    Entry, EntryPayload, Executed, HardState, Inbound, Incarnation, Input, InstanceId, ItemCount,
    JournalRead, KeyValueLimits, Lifecycle, LogIndex, NodeId, Observation, ReadLen, RequestId,
    Response, Role, StorageError, SyncSeq, Term, execute,
};

use crate::disk::{Crash, DiskHandle, SimDisk};
use crate::entropy::{MasterSeed, SplitMix64, Subsystem};
use crate::god_view::{Changes, GodView, JournalView, NodeView};
use crate::net::{Arrival, NetConfig, NetError, Network, PacketId};
use crate::scheduler::{
    Event, EventBudget, NodeIncarnation, ScheduleError, Scheduler, Shortfall, Step, VirtualTime,
};
use crate::trace::{
    ClientId, DiskOp, Endpoint, Name, ObservationBound, Overflow, PacketFate, Record, Trace,
};

/// The first sync number of every incarnation: completions carry their incarnation, so
/// numbers may repeat across incarnations (§7.2).
const FIRST_SYNC: SyncSeq = SyncSeq::new(1);

/// The shortest and longest disk latency, in virtual microseconds: from a sync's request to
/// its durability, and again from its durability to its completion reaching the node. Each
/// draws one from the disk stream.
const DISK_LATENCY_US: (u64, u64) = (50, 500);

/// A node's first tick follows its start by this long.
const FIRST_TICK_US: u64 = 1;

/// What happens at a scheduler event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Happening {
    /// A tick of `node`'s clock. The event's node incarnation is the one whose clock it is;
    /// a tick of an earlier incarnation is dropped.
    Tick {
        /// The node.
        node: NodeId,
    },
    /// A sync (or directory sync) that `node`'s `incarnation` asked for becomes durable now,
    /// if that incarnation is still running; its completion follows.
    Persist {
        /// The node.
        node: NodeId,
        /// The incarnation that asked.
        incarnation: Incarnation,
        /// The `Sync` or `SyncDir` effect.
        effect: Effect,
    },
    /// A storage completion reaches `node`: a `SyncDone`, or a `StorageFailed`. It goes to
    /// whatever incarnation is running then, which ignores one from an earlier incarnation.
    Complete {
        /// The node.
        node: NodeId,
        /// The completion.
        input: Input,
    },
    /// A packet copy arrives.
    Arrive(PacketId),
    /// A client request reaches `node`.
    Request {
        /// The node.
        node: NodeId,
        /// The client.
        client: ClientId,
        /// The attempt.
        attempt: AttemptId,
        /// What it asks.
        op: ClientOp,
    },
    /// `node` crashes.
    Crash {
        /// The node.
        node: NodeId,
    },
    /// `node` restarts as a new incarnation from the bytes its disk kept.
    Restart {
        /// The node.
        node: NodeId,
    },
}

impl Happening {
    /// The kind written on the event's trace line.
    const fn kind(&self) -> &'static str {
        match self {
            Self::Tick { .. } => "tick",
            Self::Persist { .. } => "persist",
            Self::Complete { .. } => "complete",
            Self::Arrive(_) => "arrive",
            Self::Request { .. } => "request",
            Self::Crash { .. } => "crash",
            Self::Restart { .. } => "restart",
        }
    }
}

/// Why the cluster could not be set up or run on.
#[derive(Debug)]
pub enum ClusterError {
    /// The root's own setup failed.
    Setup(&'static str),
    /// A node's genesis frame could not be made from the configuration.
    Genesis {
        /// The node.
        node: NodeId,
    },
    /// A disk operation of the root's own (initialisation, opening, locking) failed.
    Storage {
        /// The node.
        node: NodeId,
        /// The error.
        error: StorageError,
    },
    /// A node's journal could not be recovered.
    Recovery {
        /// The node.
        node: NodeId,
        /// The error.
        error: RecoveryError,
    },
    /// A node's core refused to start.
    Start {
        /// The node.
        node: NodeId,
        /// The error.
        error: StartError,
    },
    /// A node's mirror for the god view could not follow what it appended, or disagrees
    /// with its core or its disk.
    Mirror {
        /// The node.
        node: NodeId,
        /// What was wrong.
        what: String,
    },
    /// The god view refused the nodes' views: a mirrored log and its acceptance terms
    /// differ in length.
    GodView,
    /// One of the root's collections reached its bound, the run's event budget (CD-9).
    Full {
        /// Which collection.
        what: &'static str,
        /// Its bound.
        bound: usize,
    },
    /// An incarnation number ran out.
    IncarnationsExhausted {
        /// The node.
        node: NodeId,
    },
    /// A node answered an attempt no client made.
    UnknownAttempt {
        /// The node.
        node: NodeId,
        /// The attempt.
        attempt: AttemptId,
    },
    /// A client attempt was submitted for a node outside the cluster.
    UnknownNode {
        /// The node.
        node: NodeId,
    },
    /// An event could not be scheduled.
    Schedule(ScheduleError),
    /// The network refused a packet.
    Net(NetError),
    /// A step recorded more observations than its bound; the trace is incomplete.
    Trace(Overflow),
    /// The run stopped before its end: the event budget ran out, or the queue filled.
    Incomplete {
        /// Events dispatched.
        events: u64,
        /// Events still pending.
        pending: u64,
        /// Why.
        reason: Shortfall,
    },
}

impl fmt::Display for ClusterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup(what) => write!(f, "setup: {what}"),
            Self::Genesis { node } => write!(f, "node {node}: no genesis frame for this config"),
            Self::Storage { node, error } => write!(f, "node {node}: storage: {error:?}"),
            Self::Recovery { node, error } => write!(f, "node {node}: recovery: {error:?}"),
            Self::Start { node, error } => write!(f, "node {node}: start: {error:?}"),
            Self::Mirror { node, what } => write!(f, "node {node}: god-view mirror: {what}"),
            Self::GodView => f.write_str("the god view refused the nodes' views"),
            Self::Full { what, bound } => write!(f, "{what}: full at its bound of {bound}"),
            Self::IncarnationsExhausted { node } => {
                write!(f, "node {node}: incarnation numbers ran out")
            }
            Self::UnknownAttempt { node, attempt } => write!(
                f,
                "node {node} answered attempt {}, which no client made",
                attempt.get()
            ),
            Self::UnknownNode { node } => write!(f, "node {node} is not in the cluster"),
            Self::Schedule(error) => write!(f, "schedule: {error}"),
            Self::Net(error) => write!(f, "network: {error:?}"),
            Self::Trace(overflow) => write!(f, "trace: {overflow:?}"),
            Self::Incomplete {
                events,
                pending,
                reason,
            } => write!(
                f,
                "incomplete after {events} events with {pending} pending: {reason:?}"
            ),
        }
    }
}

impl std::error::Error for ClusterError {}

/// A response a client received: which attempt it answers, the node that sent it, and when
/// it arrived. A duplicated response arrives, and is listed, once per copy; the trace
/// records only the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The client.
    pub client: ClientId,
    /// The attempt it answers.
    pub attempt: AttemptId,
    /// The node that answered.
    pub node: NodeId,
    /// The response.
    pub response: Response,
    /// When it reached the client.
    pub at: VirtualTime,
}

/// A client attempt the cluster knows of: who made it, the request ID if it is an attempt
/// of a write, and whether a response to it has reached the client.
#[derive(Clone, Copy, Debug)]
struct Asked {
    client: ClientId,
    request: Option<RequestId>,
    answered: bool,
}

/// A client response in flight: its packet copies' payload.
#[derive(Clone, Debug)]
struct Reply {
    client: ClientId,
    attempt: AttemptId,
    node: NodeId,
    response: Response,
}

/// The root's mirror of a node's Raft log for the god view, built from its journal frames.
///
/// Invariants: `log` holds consecutive indices from 1, at most `limit` of them (the
/// configured `max_log_entries`, the journal's own bound; CD-9); `accepted` has `log`'s
/// length; `synced` is at most the last index, and every entry up to it is durable as
/// mirrored.
#[derive(Clone, Debug)]
struct Mirror {
    /// The hard state of the last frame that carried one.
    hard: HardState,
    /// The logical log.
    log: Vec<Entry>,
    /// Each entry's acceptance term: the term the node held when it appended it.
    accepted: Vec<Term>,
    /// The last index a durable sync covers, of the log as it is now.
    synced: LogIndex,
    /// The most entries the log may hold.
    limit: u64,
}

impl Mirror {
    const fn empty(limit: u64) -> Self {
        Self {
            hard: HardState {
                term: Term::ZERO,
                vote: None,
                commit: LogIndex::ZERO,
            },
            log: Vec::new(),
            accepted: Vec::new(),
            synced: LogIndex::ZERO,
            limit,
        }
    }

    /// The mirror of a recovered journal: every frame of its valid prefix `bytes`, all of
    /// it durable.
    fn of_journal(bytes: &[u8], limit: u64) -> Result<Self, String> {
        let mut mirror = Self::empty(limit);
        let mut offset = 0;
        while offset < bytes.len() {
            let frame = journal::decode(bytes.get(offset..).unwrap_or_default())
                .map_err(|error| format!("recovered frame at byte {offset}: {error:?}"))?;
            mirror.apply(frame.header().kind(), frame.payload())?;
            offset += frame.header().frame_len();
        }
        mirror.synced = mirror.last_index();
        Ok(mirror)
    }

    /// Applies one frame that landed in the journal.
    fn append(&mut self, frame: &[u8]) -> Result<(), String> {
        let frame = journal::decode(frame).map_err(|error| format!("appended frame: {error:?}"))?;
        self.apply(frame.header().kind(), frame.payload())
    }

    fn apply(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), String> {
        match kind {
            FrameKind::Genesis => Ok(()),
            FrameKind::RaftBatch => {
                let (entries, hard) = RaftBatch::decode(payload)
                    .map_err(|error| format!("raft batch: {error:?}"))?
                    .into_parts();
                if let Some(hard) = hard {
                    self.hard = hard;
                }
                let Some(first) = entries.first().map(|entry| entry.index.get()) else {
                    return Ok(());
                };
                let kept = usize::try_from(first.saturating_sub(1)).unwrap_or(usize::MAX);
                if kept > self.log.len() {
                    return Err(format!(
                        "a batch from index {first} leaves a gap after index {}",
                        self.log.len()
                    ));
                }
                let total = u64::try_from(kept.saturating_add(entries.len())).unwrap_or(u64::MAX);
                if total > self.limit {
                    return Err(format!(
                        "a batch from index {first} takes the log to {total} entries, past \
                         max_log_entries ({})",
                        self.limit
                    ));
                }
                self.log.truncate(kept);
                self.accepted.truncate(kept);
                // The entries from `first` on are replaced. What a sync made durable there is
                // not what the log holds now, and this frame is volatile until the next sync.
                if self.synced.get() > self.last_index().get() {
                    self.synced = self.last_index();
                }
                for entry in entries {
                    self.log.push(entry);
                    self.accepted.push(self.hard.term);
                }
                Ok(())
            }
        }
    }

    fn last_index(&self) -> LogIndex {
        self.log
            .last()
            .map_or(LogIndex::ZERO, |entry| entry.index.index())
    }

    /// The entry at `index`, if the log holds it.
    fn entry(&self, index: u64) -> Option<&Entry> {
        let position = usize::try_from(index.checked_sub(1)?).ok()?;
        self.log.get(position)
    }
}

/// A running incarnation of a node.
struct Running {
    /// The core.
    ctl: Controller<DiskHandle, RequestLifecycle>,
    /// The locked handle storage effects run through.
    storage: DiskHandle,
    /// The god view's mirror of its log.
    mirror: Mirror,
    /// The last applied index whose entry has been walked for first executions.
    walked: LogIndex,
    /// The request IDs this incarnation has applied, each at its first index.
    seen: BoundedMap<RequestId, LogIndex>,
}

/// A node: its disk, its incarnation, and the core if it is running.
struct Slot {
    disk: SimDisk,
    incarnation: Incarnation,
    running: Option<Running>,
}

/// The trace's event kinds, made once.
struct Kinds(std::collections::BTreeMap<&'static str, Name>);

impl Kinds {
    fn new() -> Option<Self> {
        let all = [
            "tick", "persist", "complete", "arrive", "request", "crash", "restart",
        ];
        let mut kinds = std::collections::BTreeMap::new();
        for kind in all {
            kinds.insert(kind, Name::new(kind)?);
        }
        Some(Self(kinds))
    }

    fn of(&self, kind: &str) -> Option<&Name> {
        self.0.get(kind)
    }
}

/// A journal image held in memory, read through the journal-read port: for auditing a
/// mirror against the bytes on the disk.
struct Image(Vec<u8>);

impl JournalRead for Image {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let start = usize::try_from(offset.get()).map_err(|_| StorageError::Failed)?;
        let wanted = usize::try_from(len.get()).map_err(|_| StorageError::Failed)?;
        let available = self.0.get(start..).unwrap_or_default();
        if available.len() < wanted {
            let got = u32::try_from(available.len()).map_err(|_| StorageError::Failed)?;
            return Err(StorageError::ShortRead { wanted: len, got });
        }
        Ok(available.iter().take(wanted).copied().collect())
    }
}

/// The simulation composition root: every voter of `config` as a real core over the
/// simulated adapters, driven by one seeded scheduler (§10.2).
///
/// Every collection here is bounded by the run's event budget (CD-9), and a full one fails
/// the run ([`ClusterError::Full`]): each grows at most once per scheduled or dispatched
/// event (an attempt per request event, a reply per packet copy, an answer per arrival, a
/// crash per crash event, a first execution per applied entry).
pub struct Cluster {
    config: Config,
    instance: InstanceId,
    seed: MasterSeed,
    scheduler: Scheduler<Happening>,
    net: Network,
    trace: Trace,
    changes: Changes,
    kinds: Kinds,
    disk_rng: SplitMix64,
    tick_us: u64,
    bound: usize,
    horizon: VirtualTime,
    nodes: std::collections::BTreeMap<NodeId, Slot>,
    asked: BoundedMap<AttemptId, Asked>,
    replies: BoundedMap<PacketId, Reply>,
    answers: BoundedQueue<Answer>,
    executed: BoundedMap<RequestId, LogIndex>,
    crashes: BoundedQueue<(NodeId, Incarnation, Crash)>,
    next_attempt: u64,
}

impl fmt::Debug for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cluster")
            .field("seed", &self.seed.get())
            .field("now", &self.now())
            .field("nodes", &self.nodes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Cluster {
    /// Every voter of `config`, freshly initialised (a genesis frame each, durable) and
    /// started at virtual time zero, with `net` as the network and `budget` as the run's
    /// event budget. Each draws from the streams of `seed` (§10.2).
    ///
    /// # Errors
    ///
    /// [`ClusterError`] if a node cannot be initialised or started.
    pub fn new(
        config: Config,
        seed: MasterSeed,
        net: NetConfig,
        budget: EventBudget,
    ) -> Result<Self, ClusterError> {
        let tick_us = config.tick_ms().saturating_mul(1_000);
        let bound = usize::try_from(budget.get()).unwrap_or(usize::MAX);
        let mut instance = [0u8; 16];
        instance[..8].copy_from_slice(&seed.get().to_be_bytes());
        instance[8..].copy_from_slice(b"keyvalsm");
        let kinds = Kinds::new().ok_or(ClusterError::Setup(
            "an event kind is not a valid trace name",
        ))?;
        let mut cluster = Self {
            instance: InstanceId::from_bytes(instance),
            seed,
            scheduler: Scheduler::new(seed.stream(Subsystem::Scheduler), budget),
            net: Network::new(net, seed.stream(Subsystem::Network)),
            trace: Trace::new(ObservationBound::DEFAULT),
            changes: Changes::new(),
            kinds,
            disk_rng: seed.stream(Subsystem::Disk),
            tick_us,
            bound,
            horizon: VirtualTime::ZERO,
            nodes: std::collections::BTreeMap::new(),
            asked: BoundedMap::new(bound),
            replies: BoundedMap::new(bound),
            answers: BoundedQueue::with_items(bound),
            executed: BoundedMap::new(bound),
            crashes: BoundedQueue::with_items(bound),
            next_attempt: 1,
            config,
        };
        let voters: Vec<NodeId> = cluster.config.voters().collect();
        for node in voters {
            let disk = cluster.initialise(node)?;
            cluster.nodes.insert(
                node,
                Slot {
                    disk,
                    incarnation: Incarnation::FIRST,
                    running: None,
                },
            );
            cluster.boot(node)?;
        }
        cluster.observe()?;
        Ok(cluster)
    }

    /// A fresh data directory for `node`, as `init` leaves it (§7.7 step 0): a journal
    /// holding its genesis frame, durable, with its directory entry synced.
    fn initialise(&self, node: NodeId) -> Result<SimDisk, ClusterError> {
        let genesis = Genesis::new(&self.config, node, self.instance)
            .ok_or(ClusterError::Genesis { node })?;
        let frame = journal::encode(FrameKind::Genesis, 1, &genesis.encode())
            .map_err(|_| ClusterError::Genesis { node })?;
        let disk = SimDisk::new();
        let storage = |error| ClusterError::Storage { node, error };
        let mut handle = disk.open();
        handle.lock().map_err(storage)?;
        handle.create_journal().map_err(storage)?;
        handle.sync_dir().map_err(storage)?;
        handle.append(&frame).map_err(storage)?;
        handle.sync().map_err(storage)?;
        Ok(disk)
    }

    /// Starts `node`'s current incarnation from its disk (§7.7 steps 1–7): recovery, then
    /// the core, whose first effects run at once; its clock starts ticking.
    fn boot(&mut self, node: NodeId) -> Result<(), ClusterError> {
        let storage_error = |error| ClusterError::Storage { node, error };
        let bound = self.bound;
        let slot = self
            .nodes
            .get_mut(&node)
            .ok_or(ClusterError::UnknownNode { node })?;
        let incarnation = slot.incarnation;
        let mut storage = slot.disk.open();
        storage.lock().map_err(storage_error)?;
        let reader = slot.disk.open();
        let len = storage.journal_len().map_err(storage_error)?;
        let recovered = journal::recover(
            &reader,
            len,
            &self.config,
            node,
            LogBounds::of(&self.config),
        )
        .map_err(|error| ClusterError::Recovery { node, error })?;
        let end = usize::try_from(recovered.log().end()).unwrap_or(usize::MAX);
        let bytes = slot.disk.journal().unwrap_or_default();
        let limit = self.config.max_log_entries().get();
        let mirror = Mirror::of_journal(bytes.get(..end).unwrap_or_default(), limit)
            .map_err(|what| ClusterError::Mirror { node, what })?;
        let (ctl, effects) = Controller::start(Boot {
            config: &self.config,
            node,
            recovered,
            reader,
            entropy: self.seed.node(node, incarnation),
            incarnation,
            first_sync: FIRST_SYNC,
            requests: RequestLifecycle::new(&self.config, node),
        })
        .map_err(|error| ClusterError::Start { node, error })?;
        slot.running = Some(Running {
            ctl,
            storage,
            mirror,
            walked: LogIndex::ZERO,
            seen: BoundedMap::new(bound),
        });
        self.execute(node, effects)?;
        let at = self.after(FIRST_TICK_US);
        self.schedule(at, Some(node), Happening::Tick { node })?;
        Ok(())
    }

    /// The current virtual time: that of the event dispatched last, or the time the last
    /// [`Self::run_until`] ran to, whichever is later.
    #[must_use]
    pub fn now(&self) -> VirtualTime {
        self.scheduler.now().max(self.horizon)
    }

    /// The events dispatched so far.
    #[must_use]
    pub const fn events(&self) -> ItemCount {
        self.scheduler.dispatched()
    }

    /// The canonical trace so far.
    #[must_use]
    pub const fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The network, for partitions and profile changes.
    pub const fn net_mut(&mut self) -> &mut Network {
        &mut self.net
    }

    /// The network.
    #[must_use]
    pub const fn net(&self) -> &Network {
        &self.net
    }

    /// The responses clients received, in arrival order, every copy.
    pub fn answers(&self) -> impl Iterator<Item = &Answer> {
        self.answers.iter()
    }

    /// The voters, in ID order.
    pub fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    /// `node`'s core, if it is running.
    #[must_use]
    pub fn controller(&self, node: NodeId) -> Option<&Controller<DiskHandle, RequestLifecycle>> {
        self.nodes
            .get(&node)?
            .running
            .as_ref()
            .map(|running| &running.ctl)
    }

    /// `node`'s current incarnation.
    #[must_use]
    pub fn incarnation(&self, node: NodeId) -> Option<Incarnation> {
        self.nodes.get(&node).map(|slot| slot.incarnation)
    }

    /// Every crash so far, in order: the node, the incarnation that crashed, and what its
    /// disk kept.
    pub fn crashes(&self) -> impl Iterator<Item = &(NodeId, Incarnation, Crash)> {
        self.crashes.iter()
    }

    /// `node`'s simulated disk.
    #[must_use]
    pub fn disk(&self, node: NodeId) -> Option<&SimDisk> {
        self.nodes.get(&node).map(|slot| &slot.disk)
    }

    /// The running node that leads in the highest term, if any.
    #[must_use]
    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter_map(|(id, slot)| {
                let status = slot.running.as_ref()?.ctl.status();
                (status.role == Role::Leader).then_some((status.term, *id))
            })
            .max()
            .map(|(_, id)| id)
    }

    /// Schedules client `client`'s request `op` to reach `node` at `at`, and returns the
    /// attempt it is. Its invocation is recorded when it reaches the node, whether or not
    /// the node is up then.
    ///
    /// # Errors
    ///
    /// [`ClusterError::UnknownNode`], [`ClusterError::Schedule`] (also for a time before
    /// [`Self::now`]) or [`ClusterError::Full`].
    pub fn submit(
        &mut self,
        at: VirtualTime,
        client: ClientId,
        node: NodeId,
        op: ClientOp,
    ) -> Result<AttemptId, ClusterError> {
        if !self.nodes.contains_key(&node) {
            return Err(ClusterError::UnknownNode { node });
        }
        let attempt = AttemptId::new(self.next_attempt);
        let request = match &op {
            ClientOp::Write { request_id, .. } => Some(*request_id),
            ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
        };
        let asked = Asked {
            client,
            request,
            answered: false,
        };
        self.asked
            .insert(attempt, asked)
            .map_err(|_| self.full("client attempts"))?;
        let happening = Happening::Request {
            node,
            client,
            attempt,
            op,
        };
        if let Err(error) = self.schedule(at, Some(node), happening) {
            self.asked.remove(&attempt);
            return Err(error);
        }
        self.next_attempt += 1;
        Ok(attempt)
    }

    /// Schedules `node`'s crash at `at`.
    ///
    /// # Errors
    ///
    /// [`ClusterError::Schedule`], also for a time before [`Self::now`].
    pub fn crash_at(&mut self, at: VirtualTime, node: NodeId) -> Result<(), ClusterError> {
        self.schedule(at, Some(node), Happening::Crash { node })
    }

    /// Schedules `node`'s restart at `at`; it does nothing if the node is running then.
    ///
    /// # Errors
    ///
    /// [`ClusterError::Schedule`], also for a time before [`Self::now`].
    pub fn restart_at(&mut self, at: VirtualTime, node: NodeId) -> Result<(), ClusterError> {
        self.schedule(at, Some(node), Happening::Restart { node })
    }

    /// Dispatches every event due by `at`, those the handling schedules meanwhile included,
    /// and then counts `at` as now. It only looks at the next event's time, drawing nothing
    /// and recording nothing of its own, so the run is the same however a driver splits it.
    ///
    /// # Errors
    ///
    /// [`ClusterError`] from any event's handling, or [`ClusterError::Incomplete`] if the
    /// run's budget or queue ran out first.
    pub fn run_until(&mut self, at: VirtualTime) -> Result<(), ClusterError> {
        while self.scheduler.next_time().is_some_and(|next| next <= at) {
            match self.scheduler.step() {
                Step::Event(event) => self.dispatch(&event)?,
                Step::Complete { .. } => break,
                Step::Incomplete {
                    events,
                    pending,
                    reason,
                } => {
                    return Err(ClusterError::Incomplete {
                        events: events.get(),
                        pending: pending.get(),
                        reason,
                    });
                }
            }
        }
        self.horizon = self.horizon.max(at);
        Ok(())
    }

    /// Checks every running node's mirror against its disk in full:
    /// - the journal as written recovers to exactly the mirrored log (every entry's term)
    ///   and the mirrored hard state;
    /// - the journal as durable holds every entry the mirror calls synced, in the same term.
    ///
    /// The check after every step compares only the last index and term; this one recovers
    /// whole journals, for the end of a run and for tests.
    ///
    /// # Errors
    ///
    /// [`ClusterError::Mirror`] naming the first disagreement, or [`ClusterError::Recovery`]
    /// if a journal cannot be recovered.
    pub fn audit_mirrors(&self) -> Result<(), ClusterError> {
        self.nodes
            .keys()
            .try_for_each(|node| self.audit_mirror(*node))
    }

    /// [`Self::audit_mirrors`] for one node; nothing if it is down.
    ///
    /// # Errors
    ///
    /// As [`Self::audit_mirrors`].
    pub fn audit_mirror(&self, node: NodeId) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get(&node) else {
            return Err(ClusterError::UnknownNode { node });
        };
        let Some(running) = slot.running.as_ref() else {
            return Ok(());
        };
        let mirror = &running.mirror;
        let fault = |what: String| ClusterError::Mirror { node, what };
        let written = self.recover_image(node, slot.disk.journal().unwrap_or_default())?;
        if written.last_index() != mirror.last_index() || written.hard_state() != mirror.hard {
            return Err(fault(format!(
                "the journal as written recovers to last index {} and {:?}; the mirror has \
                 last index {} and {:?}",
                written.last_index().get(),
                written.hard_state(),
                mirror.last_index().get(),
                mirror.hard
            )));
        }
        if let Some(entry) = mirror
            .log
            .iter()
            .find(|entry| written.term(entry.index) != Some(entry.term))
        {
            return Err(fault(format!(
                "entry {} is mirrored in term {}; the journal as written holds {:?}",
                entry.index.get(),
                entry.term.get(),
                written.term(entry.index)
            )));
        }
        let durable = self.recover_image(node, slot.disk.durable().unwrap_or_default())?;
        if let Some(entry) = mirror
            .log
            .iter()
            .take_while(|entry| entry.index.get() <= mirror.synced.get())
            .find(|entry| durable.term(entry.index) != Some(entry.term))
        {
            return Err(fault(format!(
                "entry {} is called synced (up to {}) in term {}; the durable journal holds \
                 {:?}",
                entry.index.get(),
                mirror.synced.get(),
                entry.term.get(),
                durable.term(entry.index)
            )));
        }
        Ok(())
    }

    /// The logical log a journal image recovers to.
    fn recover_image(&self, node: NodeId, bytes: Vec<u8>) -> Result<LogicalLog, ClusterError> {
        let len = ByteCount::new(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let recovered = journal::recover(
            &Image(bytes),
            len,
            &self.config,
            node,
            LogBounds::of(&self.config),
        )
        .map_err(|error| ClusterError::Recovery { node, error })?;
        Ok(recovered.into_parts().1)
    }

    /// Handles one event and records its step.
    fn dispatch(&mut self, event: &Event<Happening>) -> Result<(), ClusterError> {
        let kind = self
            .kinds
            .of(event.payload.kind())
            .ok_or(ClusterError::Setup("an event kind has no trace name"))?;
        self.trace.event(event, kind);
        match &event.payload {
            Happening::Tick { node } => self.tick(*node, event.node)?,
            Happening::Persist {
                node,
                incarnation,
                effect,
            } => self.persist(*node, *incarnation, effect)?,
            Happening::Complete { node, input } => self.complete(*node, input.clone())?,
            Happening::Arrive(packet) => self.arrive(*packet)?,
            Happening::Request {
                node,
                client,
                attempt,
                op,
            } => self.request(*node, *client, *attempt, op.clone())?,
            Happening::Crash { node } => self.crash(*node)?,
            Happening::Restart { node } => self.restart(*node)?,
        }
        self.observe()
    }

    /// A tick of `node`'s clock, if it is the running incarnation's; the next follows.
    fn tick(&mut self, node: NodeId, of: Option<NodeIncarnation>) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get(&node) else {
            return Ok(());
        };
        let current =
            slot.running.is_some() && of.is_some_and(|of| of.incarnation == slot.incarnation);
        if !current {
            return Ok(());
        }
        self.step(node, Input::Tick)?;
        let at = self.after(self.tick_us);
        self.schedule(at, Some(node), Happening::Tick { node })
    }

    /// A storage completion reaches `node`, if it is running.
    fn complete(&mut self, node: NodeId, input: Input) -> Result<(), ClusterError> {
        let Some(incarnation) = self.running_incarnation(node) else {
            return Ok(());
        };
        let op = match &input {
            Input::SyncDone {
                seq,
                incarnation: requested_by,
            } => Some(DiskOp::SyncDone {
                seq: *seq,
                requested_by: *requested_by,
            }),
            Input::StorageFailed {
                op,
                incarnation: requested_by,
            } => Some(DiskOp::Failed {
                op: *op,
                requested_by: *requested_by,
            }),
            Input::Client { .. } | Input::Peer { .. } | Input::Tick | Input::Shutdown => None,
        };
        if let Some(op) = op {
            self.record(&Record::Disk {
                node: NodeIncarnation { node, incarnation },
                op,
            })?;
        }
        self.step(node, input)
    }

    /// A packet copy arrives: a peer envelope goes to its node if that node is running (or
    /// is recorded as undeliverable), a response to its client.
    fn arrive(&mut self, packet: PacketId) -> Result<(), ClusterError> {
        match self.net.arrive(packet) {
            Arrival::Delivered(delivery, record) => match (delivery.from, delivery.to) {
                (Endpoint::Node(from), Endpoint::Node(to)) => {
                    if self.running_incarnation(to).is_some() {
                        self.record(&record)?;
                        self.step(
                            to,
                            Input::Peer {
                                from,
                                bytes: delivery.bytes,
                            },
                        )
                    } else {
                        self.record(&undeliverable(record))
                    }
                }
                (Endpoint::Node(_), Endpoint::Client(_)) => {
                    self.record(&record)?;
                    self.answer(packet)
                }
                (Endpoint::Client(_), _) => self.record(&record),
            },
            Arrival::Dropped(record) => {
                self.replies.remove(&packet);
                self.record(&record)
            }
            Arrival::Unknown => Ok(()),
        }
    }

    /// A client response reaches its client. Only the first copy to arrive for an attempt
    /// is recorded as its response; every copy is listed in [`Self::answers`].
    fn answer(&mut self, packet: PacketId) -> Result<(), ClusterError> {
        let Some(reply) = self.replies.remove(&packet) else {
            return Ok(());
        };
        let first = self.asked.get_mut(&reply.attempt).and_then(|asked| {
            let first = !asked.answered;
            asked.answered = true;
            first.then_some(asked.request)
        });
        if let Some(request) = first {
            self.record(&Record::Responded {
                client: reply.client,
                attempt: reply.attempt,
                request,
                result: response_bytes(&reply.response),
            })?;
        }
        let answer = Answer {
            client: reply.client,
            attempt: reply.attempt,
            node: reply.node,
            response: reply.response,
            at: self.now(),
        };
        self.answers
            .push(answer, 0)
            .map_err(|_| self.full("client answers"))
    }

    /// A client request reaches `node`, which is its invocation; it is lost if the node is
    /// down.
    fn request(
        &mut self,
        node: NodeId,
        client: ClientId,
        attempt: AttemptId,
        op: ClientOp,
    ) -> Result<(), ClusterError> {
        let request = self.asked.get(&attempt).and_then(|asked| asked.request);
        self.record(&Record::Invoked {
            client,
            attempt,
            node,
            request,
            op: op_bytes(&op),
        })?;
        if self.running_incarnation(node).is_none() {
            self.trace.diagnostic(&format!(
                "request {} of {client} lost: node {node} is down",
                attempt.get()
            ));
            return Ok(());
        }
        self.step(node, Input::Client { attempt, op })
    }

    /// `node` crashes: the disk's crash model runs, and the incarnation is gone.
    fn crash(&mut self, node: NodeId) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get_mut(&node) else {
            return Ok(());
        };
        if slot.running.is_none() {
            return Ok(());
        }
        let kept = slot.disk.crash(&mut self.disk_rng);
        slot.running = None;
        let incarnation = slot.incarnation;
        self.trace
            .diagnostic(&format!("node {node} crashed: {kept:?}"));
        self.crashes
            .push((node, incarnation, kept), 0)
            .map_err(|_| self.full("crashes"))
    }

    /// `node` restarts as its next incarnation, if it is down.
    fn restart(&mut self, node: NodeId) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get_mut(&node) else {
            return Ok(());
        };
        if slot.running.is_some() {
            return Ok(());
        }
        slot.incarnation = slot
            .incarnation
            .checked_next()
            .ok_or(ClusterError::IncarnationsExhausted { node })?;
        self.boot(node)
    }

    /// Steps `input` into `node`'s core and executes its effects.
    fn step(&mut self, node: NodeId, input: Input) -> Result<(), ClusterError> {
        let Some(running) = self
            .nodes
            .get_mut(&node)
            .and_then(|slot| slot.running.as_mut())
        else {
            return Ok(());
        };
        let effects = running.ctl.step(input);
        self.execute(node, effects)
    }

    /// Executes `node`'s effects, in order, as a composition root does (§7.12, §10.2).
    fn execute(&mut self, node: NodeId, effects: Vec<Effect>) -> Result<(), ClusterError> {
        for effect in effects {
            match effect {
                Effect::Send { to, bytes } => {
                    self.send(node, Endpoint::Node(to), &bytes, None)?;
                }
                Effect::Respond { attempt, response } => {
                    let client = self
                        .asked
                        .get(&attempt)
                        .map(|asked| asked.client)
                        .ok_or(ClusterError::UnknownAttempt { node, attempt })?;
                    let bytes = response_bytes(&response);
                    let reply = Reply {
                        client,
                        attempt,
                        node,
                        response,
                    };
                    self.send(node, Endpoint::Client(client), &bytes, Some(&reply))?;
                }
                Effect::Observe(Observation::Applied { index }) => self.applied(node, index)?,
                Effect::Observe(Observation::Responded { .. }) | Effect::Log(_) => {}
                Effect::Sync { .. } | Effect::SyncDir { .. } => self.defer(node, effect)?,
                Effect::Append { .. } | Effect::Truncate { .. } | Effect::CreateJournal => {
                    self.store(node, &effect)?;
                }
            }
        }
        Ok(())
    }

    /// A sync or directory sync of `node`'s becomes durable later, as a
    /// [`Happening::Persist`] at a seeded latency.
    fn defer(&mut self, node: NodeId, effect: Effect) -> Result<(), ClusterError> {
        let Some(incarnation) = self.running_incarnation(node) else {
            return Ok(());
        };
        let latency = self.disk_latency();
        let at = self.after(latency);
        self.schedule(
            at,
            Some(node),
            Happening::Persist {
                node,
                incarnation,
                effect,
            },
        )
    }

    /// Runs an append, a truncation or a journal creation of `node`'s: it lands in the
    /// volatile image at once, and if it did it is recorded (the mirror follows an append).
    /// A failure is recorded when its completion, scheduled at once, arrives.
    fn store(&mut self, node: NodeId, effect: &Effect) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get_mut(&node) else {
            return Ok(());
        };
        let incarnation = slot.incarnation;
        let Some(running) = slot.running.as_mut() else {
            return Ok(());
        };
        let before = running.storage.journal_len().unwrap_or(ByteCount::ZERO);
        let executed = execute(&mut running.storage, effect, incarnation);
        let landed = match &executed {
            Executed::Done => true,
            Executed::NotStorage | Executed::Completion(_) => false,
        };
        let op = match effect {
            Effect::Append { frame } => {
                if landed {
                    running
                        .mirror
                        .append(frame)
                        .map_err(|what| ClusterError::Mirror { node, what })?;
                }
                Some(DiskOp::Append {
                    offset: before,
                    len: ByteCount::new(u64::try_from(frame.len()).unwrap_or(u64::MAX)),
                })
            }
            Effect::Truncate { len } => Some(DiskOp::Truncate { len: *len }),
            Effect::CreateJournal => Some(DiskOp::CreateJournal),
            Effect::Sync { .. }
            | Effect::SyncDir { .. }
            | Effect::Send { .. }
            | Effect::Respond { .. }
            | Effect::Log(_)
            | Effect::Observe(_) => None,
        };
        if let Some(op) = op.filter(|_| landed) {
            self.record(&Record::Disk {
                node: NodeIncarnation { node, incarnation },
                op,
            })?;
        }
        self.completion(node, executed, 0)
    }

    /// The sync `effect` that `node`'s `incarnation` asked for runs now, if that incarnation
    /// is still running (a crash first leaves the unsynced bytes to the crash model). If it
    /// succeeds, what it covers is durable and recorded so. Its completion reaches the node
    /// after a seeded latency, or, if the node crashes and restarts first, reaches the next
    /// incarnation as a stale one.
    fn persist(
        &mut self,
        node: NodeId,
        incarnation: Incarnation,
        effect: &Effect,
    ) -> Result<(), ClusterError> {
        let Some(slot) = self.nodes.get_mut(&node) else {
            return Ok(());
        };
        if slot.incarnation != incarnation {
            return Ok(());
        }
        let Some(running) = slot.running.as_mut() else {
            return Ok(());
        };
        let executed = execute(&mut running.storage, effect, incarnation);
        let durable = match &executed {
            Executed::Completion(Input::SyncDone { .. }) => true,
            Executed::Completion(
                Input::StorageFailed { .. }
                | Input::Client { .. }
                | Input::Peer { .. }
                | Input::Tick
                | Input::Shutdown,
            )
            | Executed::Done
            | Executed::NotStorage => false,
        };
        let upto = running.storage.journal_len().unwrap_or(ByteCount::ZERO);
        let op = match effect {
            Effect::Sync { seq, .. } => durable.then(|| {
                running.mirror.synced = running.mirror.last_index();
                DiskOp::Sync { seq: *seq, upto }
            }),
            Effect::SyncDir { seq, .. } => durable.then_some(DiskOp::SyncDir { seq: *seq }),
            Effect::Append { .. }
            | Effect::Truncate { .. }
            | Effect::CreateJournal
            | Effect::Send { .. }
            | Effect::Respond { .. }
            | Effect::Log(_)
            | Effect::Observe(_) => None,
        };
        if let Some(op) = op {
            self.record(&Record::Disk {
                node: NodeIncarnation { node, incarnation },
                op,
            })?;
        }
        let latency = self.disk_latency();
        self.completion(node, executed, latency)
    }

    /// Schedules a storage effect's completion, if it has one, `latency` from now.
    fn completion(
        &mut self,
        node: NodeId,
        executed: Executed,
        latency: u64,
    ) -> Result<(), ClusterError> {
        match executed {
            Executed::NotStorage | Executed::Done => Ok(()),
            Executed::Completion(input) => {
                let at = self.after(latency);
                self.schedule(at, Some(node), Happening::Complete { node, input })
            }
        }
    }

    /// Sends `bytes` from `node` to `to` over the network, records what the network did,
    /// and schedules each copy's arrival; a client reply is remembered per copy.
    fn send(
        &mut self,
        node: NodeId,
        to: Endpoint,
        bytes: &[u8],
        reply: Option<&Reply>,
    ) -> Result<(), ClusterError> {
        let now = self.now();
        let sent = self
            .net
            .send(now, Endpoint::Node(node), to, bytes)
            .map_err(ClusterError::Net)?;
        for record in &sent.records {
            self.record(record)?;
        }
        for (at, packet) in sent.arrivals {
            if let Some(reply) = reply {
                self.replies
                    .insert(packet, reply.clone())
                    .map_err(|_| self.full("client replies in flight"))?;
            }
            self.schedule(at, None, Happening::Arrive(packet))?;
        }
        Ok(())
    }

    /// `node` applied every entry up to `index`. Walks the entries it has not walked yet,
    /// in index order, from its mirror alone: the first entry carrying a request ID is that
    /// request's first execution on this incarnation. The trace records it the first time
    /// any node applies it, and again whenever a node finds a different index for it (§10.2
    /// first executions; K-2 judges them).
    fn applied(&mut self, node: NodeId, index: LogIndex) -> Result<(), ClusterError> {
        let Some(running) = self
            .nodes
            .get_mut(&node)
            .and_then(|slot| slot.running.as_mut())
        else {
            return Ok(());
        };
        let fault = |what: String| ClusterError::Mirror { node, what };
        let mut firsts = Vec::new();
        for at in running.walked.get() + 1..=index.get() {
            let entry = running.mirror.entry(at).ok_or_else(|| {
                fault(format!(
                    "entry {at} is applied but the mirror ends at {}",
                    running.mirror.last_index().get()
                ))
            })?;
            let request = match &entry.payload {
                EntryPayload::Empty => continue,
                EntryPayload::Command(bytes) => {
                    codec::decode(bytes, KeyValueLimits::MAXIMA)
                        .map_err(|error| {
                            fault(format!("applied entry {at} holds no command: {error:?}"))
                        })?
                        .request_id
                }
            };
            if running.seen.get(&request).is_none() {
                let first = entry.index.index();
                running
                    .seen
                    .insert(request, first)
                    .map_err(|full| ClusterError::Full {
                        what: "first executions of a node",
                        bound: full.limit,
                    })?;
                firsts.push((request, first));
            }
        }
        if index.get() > running.walked.get() {
            running.walked = index;
        }
        for (request, index) in firsts {
            match self.executed.get(&request) {
                Some(known) if *known == index => {}
                Some(_) => self.record(&Record::FirstExecution { request, index })?,
                None => {
                    self.executed
                        .insert(request, index)
                        .map_err(|_| self.full("first executions"))?;
                    self.record(&Record::FirstExecution { request, index })?;
                }
            }
        }
        Ok(())
    }

    /// Records what the god view saw change in this step.
    fn observe(&mut self) -> Result<(), ClusterError> {
        self.check_mirrors()?;
        let views: Vec<NodeView<'_>> = self
            .nodes
            .iter()
            .filter_map(|(id, slot)| {
                let running = slot.running.as_ref()?;
                let status = running.ctl.status();
                Some(NodeView {
                    node: NodeIncarnation {
                        node: *id,
                        incarnation: slot.incarnation,
                    },
                    role: status.role,
                    hard: HardState {
                        term: running.mirror.hard.term,
                        vote: running.mirror.hard.vote,
                        commit: status.commit,
                    },
                    log: &running.mirror.log,
                    accepted: &running.mirror.accepted,
                    journal: JournalView {
                        appended: running.mirror.last_index(),
                        synced: running.mirror.synced,
                    },
                    applied: running.ctl.state().applied_index(),
                    state: running.ctl.state().digest(),
                })
            })
            .collect();
        let view = GodView::new(views).ok_or(ClusterError::GodView)?;
        let records = self.changes.since_last(&view);
        for record in &records {
            self.record(record)?;
        }
        Ok(())
    }

    /// Checks, after every step, that each running node's mirror agrees with what its core
    /// reports: the same last log index and term, and a persisted commit index no greater
    /// than the core's. A mirror that drifted would make the god view describe a log the
    /// node does not have. A failed node is exempt: its core may count a frame it never
    /// wrote (DM-7's hard stop refuses the append after the library moved on).
    fn check_mirrors(&self) -> Result<(), ClusterError> {
        for (id, slot) in &self.nodes {
            let Some(running) = slot.running.as_ref() else {
                continue;
            };
            let failed = match running.ctl.lifecycle() {
                Lifecycle::Failed => true,
                Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Ready => false,
            };
            if failed {
                continue;
            }
            let status = running.ctl.status();
            let mirror = &running.mirror;
            let mirrored = (mirror.last_index(), mirror.hard.term);
            if mirrored != (status.last, status.term)
                || mirror.hard.commit.get() > status.commit.get()
            {
                return Err(ClusterError::Mirror {
                    node: *id,
                    what: format!(
                        "the mirror has last index {} in term {} with commit {}, the core {} in \
                         term {} with commit {}",
                        mirrored.0.get(),
                        mirrored.1.get(),
                        mirror.hard.commit.get(),
                        status.last.get(),
                        status.term.get(),
                        status.commit.get()
                    ),
                });
            }
        }
        Ok(())
    }

    fn record(&mut self, record: &Record) -> Result<(), ClusterError> {
        self.trace.record(record).map_err(ClusterError::Trace)
    }

    const fn full(&self, what: &'static str) -> ClusterError {
        ClusterError::Full {
            what,
            bound: self.bound,
        }
    }

    /// Schedules `happening` at `at`, concerning `node`'s current incarnation if any.
    /// Nothing is scheduled before [`Self::now`], the later of the last event and the time
    /// the last [`Self::run_until`] ran to: handled earlier than the time its handling
    /// reads, such an event would time its sends, syncs and ticks from the wrong instant.
    fn schedule(
        &mut self,
        at: VirtualTime,
        node: Option<NodeId>,
        happening: Happening,
    ) -> Result<(), ClusterError> {
        let now = self.now();
        if at < now {
            return Err(ClusterError::Schedule(ScheduleError::InThePast { now, at }));
        }
        let node = node.and_then(|node| {
            self.nodes.get(&node).map(|slot| NodeIncarnation {
                node,
                incarnation: slot.incarnation,
            })
        });
        self.scheduler
            .schedule(at, node, happening)
            .map(|_| ())
            .map_err(ClusterError::Schedule)
    }

    fn after(&self, micros: u64) -> VirtualTime {
        VirtualTime::from_micros(self.now().as_micros().saturating_add(micros))
    }

    /// A seeded disk latency: from a sync's request to its durability, and from its
    /// durability to its completion.
    const fn disk_latency(&mut self) -> u64 {
        let (low, high) = DISK_LATENCY_US;
        low + self.disk_rng.next_u64() % (high - low + 1)
    }

    fn running_incarnation(&self, node: NodeId) -> Option<Incarnation> {
        let slot = self.nodes.get(&node)?;
        slot.running.as_ref().map(|_| slot.incarnation)
    }
}

/// The network's record of a peer envelope it delivered, as undeliverable: the node it
/// reached was down.
fn undeliverable(record: Record) -> Record {
    match record {
        Record::Packet {
            copy,
            from,
            to,
            bytes,
            ..
        } => Record::Packet {
            fate: PacketFate::Undeliverable,
            copy,
            from,
            to,
            bytes,
        },
        Record::Disk { .. }
        | Record::Role { .. }
        | Record::Log { .. }
        | Record::Commit { .. }
        | Record::Applied { .. }
        | Record::Journal { .. }
        | Record::Invoked { .. }
        | Record::Responded { .. }
        | Record::FirstExecution { .. }
        | Record::Verdict { .. } => record,
    }
}

/// An operation's bytes for the trace: a write's canonical command (§7.5), otherwise its
/// debug form. Debug output is fixed for a given build, which is all the canonical trace
/// promises (T-3: same build, same seed, same bytes).
fn op_bytes(op: &ClientOp) -> Vec<u8> {
    match op {
        ClientOp::Write {
            request_id,
            mutation,
        } => codec::encode(&Command {
            request_id: *request_id,
            mutation: mutation.clone(),
        }),
        ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => {
            format!("{op:?}").into_bytes()
        }
    }
}

/// A response's bytes on the client path and in the trace: its debug form, fixed for a
/// given build (T-3). The slice has no response codec; the HTTP adapter's JSON is not
/// simulated (§10.2).
fn response_bytes(response: &Response) -> Vec<u8> {
    format!("{response:?}").into_bytes()
}
