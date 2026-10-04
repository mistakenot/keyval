//! The serial controller (PLAN.md §7.2, §7.4, §7.7, §7.10–§7.12): one node's core, driven
//! one input at a time through its single entry point, [`Inbound::step`].

use journal::{JournalError, Recovered};
use raft_adapter::{
    Apply, ConstructError, DropCounts, Fed, IdentityError, LoopError, PeerCodec, RaftInput,
    RaftNode, ReadyLoop, StepRefusal,
};
use statemachine::{StateLimits, StateMachine};
use types::{
    AttemptId, Config, DroppedInput, Effect, Entropy, Entry, EntryIndex, Inbound, Incarnation,
    Input, JournalRead, Lifecycle, Limit, LogEvent, LogIndex, NodeId, Observation, OutboundMessage,
    ProposeError, Refusal, Response, Role, Status, StorageOp, SyncSeq, Term, Tick, invariant,
};

use crate::deadlines::Deadlines;
use crate::queue::{InboundQueue, Queued};
use crate::requests::{Applying, Core, Proposal, Requests, View};

#[cfg(test)]
mod invariant_fixtures;

/// The most ticks one turn processes (§7.10).
///
/// After a pause, overdue ticks are processed in batches of at most 8, interleaved with
/// other inputs, so network and read work is not starved. The same policy runs in
/// simulation and production (P3).
pub const TICKS_PER_TURN: u64 = 8;

/// What a node starts from (§7.7 steps 5 to 7).
#[derive(Debug)]
pub struct Boot<'a, R, E, Q> {
    /// The validated configuration.
    pub config: &'a Config,
    /// This node.
    pub node: NodeId,
    /// The recovered journal: the verified genesis and the logical log (§7.7 steps 1–4).
    pub recovered: Recovered,
    /// The journal's read port.
    pub reader: R,
    /// The entropy port, for the library's election timeouts (P4).
    pub entropy: E,
    /// This incarnation: every storage completion it is sent carries it (§7.2).
    pub incarnation: Incarnation,
    /// The first sync number this incarnation uses.
    pub first_sync: SyncSeq,
    /// The request lifecycle (W14.2, W14.3).
    pub requests: Q,
}

/// Why a node could not start. Nothing was emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    /// The configuration has no voter `node`.
    NotAVoter {
        /// The node asked for.
        node: NodeId,
    },
    /// A committed entry could not be read back for replay (§7.7 step 5).
    Replay {
        /// The entry's index.
        index: u64,
        /// Why it could not be read.
        error: JournalError,
    },
    /// The library refused the recovered state (§7.7 step 6).
    Construct(ConstructError),
    /// The sync numbers ran out at `u64::MAX`.
    SyncSeqExhausted,
}

/// Why a node failed (DM-7, §7.11). Failure is final for the incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The Ready loop stopped: the journal's hard stop, or a failed read under the library.
    Loop(LoopError),
    /// A storage effect failed with an unexpected I/O error.
    Storage(StorageOp),
}

/// What the controller dropped or refused, by reason (§7.11, §8.2: dropped and counted,
/// never silent). Each count saturates at `u64::MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Peer envelopes dropped because the queue was full.
    pub peers_queue_full: u64,
    /// Client attempts answered `busy` because the queue was full.
    pub clients_queue_full: u64,
    /// Peer messages refused after their envelope opened ([`StepRefusal`]).
    pub peers_refused: u64,
    /// Peer envelopes dropped because the node is failed (DM-7): at admission, or
    /// already queued when it failed.
    pub peers_failed: u64,
    /// Completions from another incarnation, ignored (§7.2).
    pub stale_completions: u64,
}

/// One node's serial core.
///
/// Every input enters through [`Inbound::step`], which admits it and then runs turns
/// until nothing more can progress. A turn processes at most [`TICKS_PER_TURN`] pending
/// ticks, then one input: if a peer envelope is queued, the oldest queued input the node
/// can take; otherwise a waiting proposal, or else the oldest queued input.
/// A burst, such as the ticks overdue after a pause, is admitted with
/// [`admit`](Self::admit) before its last input is stepped, so its ticks are interleaved
/// with the queued inputs (§7.10).
///
/// - **Admission (§7.11):** client attempts and peer envelopes share one bounded queue of
///   `controller_queue_inputs` inputs. When it is full, a peer envelope is dropped and
///   counted, and a client attempt is answered `busy`. A tick merges into the pending tick
///   count and takes no place. A completion (`SyncDone`, `StorageFailed`) is handled at
///   once, so it is never dropped, and one from another incarnation is ignored (§7.2).
/// - **The clock (§7.10, §7.12):** each processed tick advances the node's logical time
///   and fires every deadline due, even while a sync is outstanding. The library's tick is
///   owed until the loop can take it.
/// - **Raft (§7.4):** while a sync is outstanding the library takes no input, so peer
///   envelopes stay queued and client attempts are still handed over: reads and replays
///   are answered at once. The start-up truncation's sync counts too, so nothing is
///   appended over a truncation that is not yet durable (§7.6). Peer envelopes are opened
///   here ([`PeerCodec::open`]), and every message the loop sends is sealed before it
///   leaves the core (§8.2).
/// - **Failure (DM-7, §7.11):** a loop error or a failed storage effect moves the node to
///   `failed`. It stops feeding the library; the peer envelopes already queued are dropped
///   and counted at once, and later ones at admission;
///   ticks and client attempts still reach [`Requests`], which answers `recovering`.
///
/// Invariant: the lifecycle only moves forward; the loop is fed only while it is idle.
pub struct Controller<R: JournalRead, Q: Requests> {
    /// The node's state a [`Requests`] call may see.
    core: Core,
    /// The incarnation.
    incarnation: Incarnation,
    /// The Ready loop over the library.
    raft: ReadyLoop<R>,
    /// The peer-envelope codec.
    codec: PeerCodec,
    /// The request lifecycle.
    requests: Q,
    /// The inbound queue.
    queue: InboundQueue,
    /// Library ticks owed: ticks processed while the loop could not take them.
    raft_ticks_owed: u64,
    /// The start-up truncation's sync, until it completes (§7.7, DUR-6). While it is
    /// outstanding the library takes no input, as during a loop sync (§7.6).
    startup_sync: Option<SyncSeq>,
    /// Why the node failed, once it has.
    failure: Option<Failure>,
    /// Drops and refusals.
    counters: Counters,
    /// What [`Requests::status_changed`] was last told about.
    last_status: StatusKey,
}

/// What a status change is judged on: the lifecycle, and the library's role, term and
/// believed leader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatusKey {
    /// The node's lifecycle.
    lifecycle: Lifecycle,
    /// The library's role.
    role: Role,
    /// The library's term.
    term: Term,
    /// The leader the library believes in.
    leader: Option<NodeId>,
}

impl<R: JournalRead, Q: Requests> Controller<R, Q> {
    /// Starts a node from its recovered journal (§7.7 steps 5 to 7).
    ///
    /// It replays the committed entries `1..=commit` into the state machine, constructs the
    /// library with that applied index, and moves to `Ready`. If recovery found a torn tail,
    /// it first emits the `Truncate` and its `Sync` (DUR-6). Then it runs any ready work, so
    /// the returned effects are the node's first.
    ///
    /// # Errors
    ///
    /// [`StartError`] if the node is not a voter, a committed entry cannot be read back,
    /// or the library refuses the recovered state.
    pub fn start<E: Entropy + Send + 'static>(
        boot: Boot<'_, R, E, Q>,
    ) -> Result<(Self, Vec<Effect>), StartError> {
        let Boot {
            config,
            node,
            recovered,
            reader,
            entropy,
            incarnation,
            first_sync,
            requests,
        } = boot;
        let settings = config
            .consensus(node)
            .ok_or(StartError::NotAVoter { node })?;
        let truncate_to = recovered.truncate_to();
        let (genesis, mut log) = recovered.into_parts();
        let codec = PeerCodec::new(
            genesis.instance_id(),
            genesis.fingerprint(),
            node,
            config.voters(),
        )
        .map_err(|error| match error {
            IdentityError::NotAVoter { node } => StartError::NotAVoter { node },
        })?;
        let mut effects = vec![Effect::Log(LogEvent::LifecycleChanged {
            from: Lifecycle::Starting,
            to: Lifecycle::Recovering,
        })];
        let commit = log.hard_state().commit;
        let mut state = StateMachine::new(node, StateLimits::of(config));
        for raw in 1..=commit.get() {
            let index = EntryIndex::new(raw).map_err(|_| StartError::Replay {
                index: raw,
                error: JournalError::NotInLog { index: raw },
            })?;
            let entry = log
                .entry(index, &reader)
                .map_err(|error| StartError::Replay { index: raw, error })?;
            state.apply(&entry, commit);
        }
        if commit > LogIndex::ZERO {
            // The published view starts at the replayed prefix (C-3, C-4).
            effects.push(Effect::Observe(Observation::Applied { index: commit }));
        }
        let raft = RaftNode::new(&settings, config.voters(), log, reader, entropy, commit)
            .map_err(StartError::Construct)?;
        let (startup_sync, loop_first_sync) = match truncate_to {
            Some(len) => {
                effects.push(Effect::Truncate { len });
                effects.push(Effect::Sync {
                    seq: first_sync,
                    incarnation,
                });
                let next = first_sync
                    .checked_next()
                    .ok_or(StartError::SyncSeqExhausted)?;
                (Some(first_sync), next)
            }
            None => (None, first_sync),
        };
        effects.push(Effect::Log(LogEvent::LifecycleChanged {
            from: Lifecycle::Recovering,
            to: Lifecycle::Ready,
        }));
        let queue_inputs =
            usize::try_from(config.limit(Limit::ControllerQueueInputs)).unwrap_or(usize::MAX);
        let pending_attempts =
            usize::try_from(config.limit(Limit::PendingAttempts)).unwrap_or(usize::MAX);
        let mut controller = Self {
            core: Core {
                node,
                now: Tick::ZERO,
                lifecycle: Lifecycle::Ready,
                shutting_down: false,
                write_deadline: config.write_deadline(),
                deadlines: Deadlines::new(node, pending_attempts),
                state,
            },
            incarnation,
            raft: ReadyLoop::new(raft, incarnation, loop_first_sync),
            codec,
            requests,
            queue: InboundQueue::new(node, queue_inputs),
            raft_ticks_owed: 0,
            startup_sync,
            failure: None,
            counters: Counters::default(),
            last_status: StatusKey {
                lifecycle: Lifecycle::Ready,
                role: Role::Follower,
                term: Term::ZERO,
                leader: None,
            },
        };
        controller.poll(&mut effects);
        // The library starts as a follower in its hard state's term and campaigns only on
        // ticks, so this first poll changes no role: there is no role change to log (O-3).
        controller.last_status = controller.status_key();
        Ok((controller, effects))
    }

    /// The node's lifecycle.
    pub const fn lifecycle(&self) -> Lifecycle {
        self.core.lifecycle
    }

    /// Why the node failed, if it has.
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// The ticks processed since the node started: its logical time (§7.10).
    pub const fn now(&self) -> Tick {
        self.core.now
    }

    /// The client attempts and peer envelopes waiting in the queue.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The ticks that arrived and are not processed yet.
    pub const fn pending_ticks(&self) -> u64 {
        self.queue.pending_ticks()
    }

    /// Library ticks owed: processed while a sync was outstanding.
    pub const fn raft_ticks_owed(&self) -> u64 {
        self.raft_ticks_owed
    }

    /// Drops and refusals, by reason.
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// Peer envelopes the codec dropped, by reason (§8.2).
    pub const fn envelope_drops(&self) -> DropCounts {
        self.codec.dropped()
    }

    /// The library's status.
    pub const fn status(&self) -> &Status {
        self.raft.status()
    }

    /// The state machine: the published view and the request history.
    pub const fn state(&self) -> &StateMachine {
        &self.core.state
    }

    /// The request lifecycle.
    pub const fn requests(&self) -> &Q {
        &self.requests
    }

    /// The Ready loop's outstanding sync, if any.
    pub const fn outstanding_sync(&self) -> Option<SyncSeq> {
        self.raft.outstanding_sync()
    }

    /// The number of attempt deadlines held.
    pub fn deadlines(&self) -> usize {
        self.core.deadlines.len()
    }

    /// Admits `input` without running any turn, and returns what admission did: a
    /// client attempt answered `busy` or a peer envelope dropped when the queue is full, or
    /// a completion handled (§7.11).
    ///
    /// A root with a burst of inputs, such as the ticks overdue after a pause, admits all
    /// but the last and then [`step`](Inbound::step)s the last: its turns then process at
    /// most [`TICKS_PER_TURN`] ticks before each queued input (§7.10).
    pub fn admit(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.admit_into(input, &mut effects);
        self.observe_status(&mut effects);
        effects
    }

    /// Admits `input`: queued, merged, answered or handled at once (§7.11).
    fn admit_into(&mut self, input: Input, effects: &mut Vec<Effect>) {
        if input.is_stale_completion(self.incarnation) {
            self.counters.stale_completions = self.counters.stale_completions.saturating_add(1);
            return;
        }
        match input {
            Input::Client { attempt, op } => {
                if let Err((attempt, _)) = self.queue.offer_client(attempt, op) {
                    self.counters.clients_queue_full =
                        self.counters.clients_queue_full.saturating_add(1);
                    respond_busy(attempt, effects);
                }
            }
            Input::Peer { from, bytes } => {
                if self.core.lifecycle == Lifecycle::Failed {
                    self.counters.peers_failed = self.counters.peers_failed.saturating_add(1);
                    log_dropped_peer(effects);
                } else if self.queue.offer_peer(from, bytes).is_err() {
                    self.counters.peers_queue_full =
                        self.counters.peers_queue_full.saturating_add(1);
                    log_dropped_peer(effects);
                }
            }
            Input::Tick => self.queue.add_tick(),
            Input::SyncDone { seq, incarnation } => self.sync_done(seq, incarnation, effects),
            Input::StorageFailed { op, .. } => self.fail(Failure::Storage(op), effects),
            Input::Shutdown => {
                self.core.shutting_down = true;
                let mut view = View::new(&mut self.core, self.raft.status());
                self.requests.shutdown(&mut view, effects);
            }
        }
    }

    /// One turn: at most [`TICKS_PER_TURN`] ticks, then one input. Returns whether it did
    /// anything.
    fn turn(&mut self, effects: &mut Vec<Effect>) -> bool {
        let mut budget = TICKS_PER_TURN;
        let mut progressed = false;
        while budget > 0 && self.raft_ticks_owed > 0 && self.library_idle() {
            budget -= 1;
            self.raft_ticks_owed -= 1;
            progressed = true;
            self.feed(RaftInput::Tick, effects);
        }
        while budget > 0 && self.queue.take_tick() {
            budget -= 1;
            progressed = true;
            self.tick(effects);
        }
        // A queued peer envelope goes before a waiting proposal (br-m7x7). Peers queue only
        // while a sync is outstanding, so at most one sync's worth goes first; a proposal
        // first would leave a sync outstanding again, and under one write per sync the
        // followers' responses would never be taken.
        if self.library_idle() && !self.queue.has_peers() && self.propose(effects) {
            return true;
        }
        let peers_wait = self.syncing();
        match self.queue.next(peers_wait) {
            Some(Queued::Client { attempt, op }) => {
                let mut view = View::new(&mut self.core, self.raft.status());
                self.requests.attempt(&mut view, attempt, op, effects);
                true
            }
            Some(Queued::Peer { from, envelope }) => {
                self.peer(from, &envelope, effects);
                true
            }
            None => progressed,
        }
    }

    /// Processes one tick: the clock advances, the library ticks (or the tick is owed),
    /// and every deadline due fires (§7.8, §7.12).
    fn tick(&mut self, effects: &mut Vec<Effect>) {
        let now = self.core.now.checked_add(Tick::new(1));
        invariant!(
            now.is_some(),
            "NODE-CLOCK",
            "node {}: the clock would pass u64::MAX ticks",
            self.core.node
        );
        self.core.now = now.unwrap_or(self.core.now);
        if self.library_idle() {
            self.feed(RaftInput::Tick, effects);
        } else if self.core.lifecycle != Lifecycle::Failed {
            self.raft_ticks_owed = self.raft_ticks_owed.saturating_add(1);
        }
        while let Some(attempt) = self.core.deadlines.pop_due(self.core.now) {
            let mut view = View::new(&mut self.core, self.raft.status());
            self.requests.expired(&mut view, attempt, effects);
        }
    }

    /// Proposes the next waiting command as this turn's input, if there is one. Returns
    /// whether it did.
    fn propose(&mut self, effects: &mut Vec<Effect>) -> bool {
        let proposal = {
            let mut view = View::new(&mut self.core, self.raft.status());
            self.requests.next_proposal(&mut view)
        };
        let Some(Proposal { attempt, command }) = proposal else {
            return false;
        };
        // A loop error while draining after the library took the command fails the node;
        // the attempt stays submitted and resolves as unknown_outcome (A-5).
        if let Some(fed) = self.feed(RaftInput::Propose(command), effects) {
            let result = proposed(self.core.node, &fed);
            let mut view = View::new(&mut self.core, self.raft.status());
            self.requests.proposed(&mut view, attempt, result, effects);
        }
        true
    }

    /// Opens a peer envelope and steps its message (§8.2).
    ///
    /// # Panics
    ///
    /// `NODE-FAILED-PEERS` if the node is failed: a failed node drops the envelopes queued
    /// when it fails and every later one at admission, so none can reach here (br-dkov).
    fn peer(&mut self, from: NodeId, envelope: &[u8], effects: &mut Vec<Effect>) {
        invariant!(
            self.core.lifecycle != Lifecycle::Failed,
            "NODE-FAILED-PEERS",
            "node {}: a peer envelope from node {from} reached a failed node, which drops \
             queued envelopes when it fails and later ones at admission",
            self.core.node
        );
        let Ok(message) = self.codec.open(from, envelope) else {
            // Counted by the codec, under its reason.
            log_dropped_peer(effects);
            return;
        };
        let Some(fed) = self.feed(RaftInput::Peer(message), effects) else {
            return;
        };
        if stepped(self.core.node, &fed).is_err() {
            self.counters.peers_refused = self.counters.peers_refused.saturating_add(1);
            log_dropped_peer(effects);
        }
    }

    /// Handles a `SyncDone` from this incarnation: the start-up truncation's, or the
    /// loop's (§7.4 step 4).
    fn sync_done(&mut self, seq: SyncSeq, incarnation: Incarnation, effects: &mut Vec<Effect>) {
        if self.startup_sync == Some(seq) {
            self.startup_sync = None;
            return;
        }
        if self.core.lifecycle == Lifecycle::Failed {
            return;
        }
        let start = effects.len();
        let mut applier = Applier {
            node: self.core.node,
            state: &mut self.core.state,
            deadlines: &mut self.core.deadlines,
            requests: &mut self.requests,
        };
        let result = self.raft.sync_done(seq, incarnation, &mut applier, effects);
        self.seal(start, effects);
        if let Err(error) = result {
            self.fail(Failure::Loop(error), effects);
        }
    }

    /// Runs any ready work without an input (§7.7 step 7).
    fn poll(&mut self, effects: &mut Vec<Effect>) {
        let start = effects.len();
        let mut applier = Applier {
            node: self.core.node,
            state: &mut self.core.state,
            deadlines: &mut self.core.deadlines,
            requests: &mut self.requests,
        };
        let result = self.raft.poll(&mut applier, effects);
        self.seal(start, effects);
        if let Err(error) = result {
            self.fail(Failure::Loop(error), effects);
        }
    }

    /// Feeds one input to the loop, which must be idle, and seals what it sends. Returns
    /// what it did, or `None` if the loop failed (the node is now failed).
    ///
    /// # Panics
    ///
    /// `NODE-FEED-IDLE` if the loop is not idle.
    fn feed(&mut self, input: RaftInput, effects: &mut Vec<Effect>) -> Option<Fed> {
        invariant!(
            self.library_idle(),
            "NODE-FEED-IDLE",
            "node {}: an input was fed while the loop could not take it (lifecycle {:?}, \
             outstanding sync {:?}, start-up sync {:?})",
            self.core.node,
            self.core.lifecycle,
            self.raft.outstanding_sync(),
            self.startup_sync
        );
        let start = effects.len();
        let mut applier = Applier {
            node: self.core.node,
            state: &mut self.core.state,
            deadlines: &mut self.core.deadlines,
            requests: &mut self.requests,
        };
        let result = self.raft.feed(input, &mut applier, effects);
        self.seal(start, effects);
        match result {
            Ok(fed) => Some(fed),
            Err(error) => {
                self.fail(Failure::Loop(error), effects);
                None
            }
        }
    }

    /// The current [`StatusKey`].
    const fn status_key(&self) -> StatusKey {
        let status = self.raft.status();
        StatusKey {
            lifecycle: self.core.lifecycle,
            role: status.role,
            term: status.term,
            leader: status.leader,
        }
    }

    /// Tells the request lifecycle if the status changed since it was last told (§7.8),
    /// after logging a change of role, term or believed leader (O-3).
    fn observe_status(&mut self, effects: &mut Vec<Effect>) {
        let now = self.status_key();
        if now == self.last_status {
            return;
        }
        log_role_change(self.last_status, now, effects);
        self.last_status = now;
        let mut view = View::new(&mut self.core, self.raft.status());
        self.requests.status_changed(&mut view, effects);
    }

    /// Whether the loop can take an input: the node is not failed and no sync is
    /// outstanding, the start-up truncation's included.
    fn library_idle(&self) -> bool {
        self.core.lifecycle != Lifecycle::Failed && !self.syncing()
    }

    /// Whether a sync is outstanding: the start-up truncation's, or the loop's. Until it
    /// completes the library takes no input and peer envelopes wait, so nothing is
    /// appended over a truncation that is not yet durable (§7.6) and at most one sync is
    /// outstanding (§7.11, br-0fha).
    const fn syncing(&self) -> bool {
        self.startup_sync.is_some() || self.raft.outstanding_sync().is_some()
    }

    /// Seals every message the loop sent from `effects[start..]` into a peer envelope
    /// (§8.2).
    ///
    /// # Panics
    ///
    /// `NODE-SEAL` if the codec refuses one: the library addressed a node outside the
    /// voters, or sent more than one envelope can hold.
    fn seal(&self, start: usize, effects: &mut [Effect]) {
        for effect in effects.iter_mut().skip(start) {
            match effect {
                Effect::Send { to, bytes } => {
                    let sealed = self.codec.seal(&OutboundMessage {
                        to: *to,
                        bytes: std::mem::take(bytes),
                    });
                    invariant!(
                        sealed.is_ok(),
                        "NODE-SEAL",
                        "node {}: a message to node {to} cannot be sealed: {sealed:?}",
                        self.core.node
                    );
                    *bytes = sealed.unwrap_or_default();
                }
                Effect::Append { .. }
                | Effect::Truncate { .. }
                | Effect::Sync { .. }
                | Effect::CreateJournal
                | Effect::SyncDir { .. }
                | Effect::Respond { .. }
                | Effect::Log(_)
                | Effect::Observe(_) => {}
            }
        }
    }

    /// Moves the node to `failed` (DM-7, §7.11), keeping the first cause.
    ///
    /// The peer envelopes queued until now are dropped, counted and logged at once. A failed
    /// node never feeds the library again, and a sync it was waiting for never completes,
    /// so they would otherwise wait for the incarnation's lifetime, holding queue places
    /// its client attempts need to reach the request lifecycle (`recovering`, A-2; br-dkov).
    fn fail(&mut self, failure: Failure, effects: &mut Vec<Effect>) {
        if self.core.lifecycle == Lifecycle::Failed {
            return;
        }
        effects.push(Effect::Log(LogEvent::LifecycleChanged {
            from: self.core.lifecycle,
            to: Lifecycle::Failed,
        }));
        self.core.lifecycle = Lifecycle::Failed;
        self.failure = Some(failure);
        let stranded = self.queue.drain_peers();
        self.counters.peers_failed = self
            .counters
            .peers_failed
            .saturating_add(u64::try_from(stranded).unwrap_or(u64::MAX));
        for _ in 0..stranded {
            log_dropped_peer(effects);
        }
    }
}

impl<R: JournalRead, Q: Requests> Inbound for Controller<R, Q> {
    /// Admits `input`, then runs turns until nothing more can progress: the queue is
    /// empty, or holds only peer envelopes waiting for an outstanding sync, and no tick is
    /// pending.
    fn step(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.admit_into(input, &mut effects);
        self.observe_status(&mut effects);
        while self.turn(&mut effects) {
            self.observe_status(&mut effects);
        }
        effects
    }
}

/// The loop's [`Apply`]: the state machine applies each committed entry, and the request
/// lifecycle learns what it decided (§7.4 step 5, §7.5).
struct Applier<'a, Q> {
    /// This node.
    node: NodeId,
    /// The state machine.
    state: &'a mut StateMachine,
    /// The attempt deadlines.
    deadlines: &'a mut Deadlines,
    /// The request lifecycle.
    requests: &'a mut Q,
}

impl<Q: Requests> Apply for Applier<'_, Q> {
    fn apply(&mut self, entry: &Entry, commit: LogIndex, effects: &mut Vec<Effect>) {
        let applied = self.state.apply(entry, commit);
        let mut applying = Applying {
            node: self.node,
            entry,
            applied: &applied,
            commit,
            state: &*self.state,
            deadlines: &mut *self.deadlines,
        };
        self.requests.applied(&mut applying, effects);
    }
}

/// What the loop answered a peer message: stepped, or refused with its reason.
///
/// # Panics
///
/// `NODE-FED` if the loop answered something other than a step.
fn stepped(node: NodeId, fed: &Fed) -> Result<(), StepRefusal> {
    let result = match fed {
        Fed::Stepped(result) => Some(*result),
        Fed::Ticked | Fed::Proposed(_) | Fed::Deferred(_) => None,
    };
    invariant!(
        result.is_some(),
        "NODE-FED",
        "node {node}: a peer message was answered {fed:?}"
    );
    result.unwrap_or(Ok(()))
}

/// What the loop answered a proposal: taken, or refused with nothing appended.
///
/// # Panics
///
/// `NODE-FED` if the loop answered something other than a proposal.
fn proposed(node: NodeId, fed: &Fed) -> Result<(), ProposeError> {
    let result = match fed {
        Fed::Proposed(result) => Some(*result),
        Fed::Ticked | Fed::Stepped(_) | Fed::Deferred(_) => None,
    };
    invariant!(
        result.is_some(),
        "NODE-FED",
        "node {node}: a proposal was answered {fed:?}"
    );
    result.unwrap_or(Err(ProposeError::Dropped))
}

/// Answers `attempt` with `429 busy`: a local admission bound was hit (§7.11). The attempt
/// was never submitted.
fn respond_busy(attempt: AttemptId, effects: &mut Vec<Effect>) {
    effects.push(Effect::Respond {
        attempt,
        response: Response::Refused(Refusal::Busy),
    });
}
/// Logs a role change (O-3, br-qb4p) when the library's role, term or believed leader
/// differs between `last` and `now`: once per observed change, in effect order. A change
/// of lifecycle alone is not one; [`LogEvent::LifecycleChanged`] records that.
fn log_role_change(last: StatusKey, now: StatusKey, effects: &mut Vec<Effect>) {
    if (now.role, now.term, now.leader) != (last.role, last.term, last.leader) {
        effects.push(Effect::Log(LogEvent::RoleChanged {
            role: now.role,
            term: now.term,
            leader: now.leader,
        }));
    }
}

/// Logs a dropped peer message (§8.2, CD-9: dropped and counted, never silent).
fn log_dropped_peer(effects: &mut Vec<Effect>) {
    effects.push(Effect::Log(LogEvent::Dropped {
        input: DroppedInput::PeerMessage,
    }));
}
