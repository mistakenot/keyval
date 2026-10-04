//! Test support for the controller: an in-memory journal, seeded entropy, the committed
//! fixture configs, a recording [`Requests`] double and a member harness that executes
//! effects. Test-only (rule 17).
#![cfg(test)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;

use journal::{FrameKind, Genesis, LogBounds, RaftBatch, Recovered};
use statemachine::Applied;
use statemachine::codec::{self, Command};
use types::{
    AttemptId, ByteCount, ClientOp, Config, Effect, Entropy, EntropyStream, Entry, EntryIndex,
    EntryPayload, HardState, Inbound, Incarnation, Input, InstanceId, JournalRead, Key,
    KeyValueLimits, Lifecycle, LogEvent, LogIndex, Mutation, NodeId, ProposeError, ReadLen,
    Refusal, RequestId, Response, StorageError, SyncSeq, Term, Tick, Value,
};

use crate::{Applying, Boot, Controller, Proposal, Requests, View, WritePath};

/// The cluster instance every test journal is initialised with.
pub const INSTANCE: &str = "00112233445566778899aabbccddeeff";

/// The incarnation every test node runs in, unless a test restarts it.
pub const INCARNATION: Incarnation = Incarnation::new(3);

/// The first sync number every test node is given.
pub const FIRST_SYNC: SyncSeq = SyncSeq::new(1);

/// The committed configs these tests use.
const CONFIGS: [(&str, &str); 3] = [
    (
        "one-voter",
        include_str!("../../../config/fixtures/one-voter.toml"),
    ),
    (
        "small-limits",
        include_str!("../../../config/fixtures/small-limits.toml"),
    ),
    (
        "three-voters",
        include_str!("../../../config/fixtures/three-voters.toml"),
    ),
];

/// The committed config called `name`, parsed.
pub fn config(name: &str) -> Config {
    let (_, text) = CONFIGS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap();
    Config::parse(text, Path::new("config/fixtures")).unwrap()
}

/// A seeded entropy source (P4).
#[derive(Debug)]
pub struct Seeded(u64);

impl Seeded {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }
}

impl Entropy for Seeded {
    fn next_u64(&mut self, stream: EntropyStream) -> u64 {
        match stream {
            EntropyStream::ElectionTimeout => {
                self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            }
        }
    }
}

/// An in-memory journal file, shared between a member and its controller's read port.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    bytes: Rc<RefCell<Vec<u8>>>,
    failing: Rc<Cell<bool>>,
}

impl Journal {
    pub fn push(&self, bytes: &[u8]) {
        self.bytes.borrow_mut().extend_from_slice(bytes);
    }

    pub fn truncate(&self, len: u64) {
        self.bytes
            .borrow_mut()
            .truncate(usize::try_from(len).unwrap());
    }

    pub fn byte_len(&self) -> u64 {
        u64::try_from(self.bytes.borrow().len()).unwrap()
    }

    /// Makes every later read fail with `StorageError::Failed`.
    pub fn fail_reads(&self) {
        self.failing.set(true);
    }
}

impl JournalRead for Journal {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        if self.failing.get() {
            return Err(StorageError::Failed);
        }
        let bytes = self.bytes.borrow();
        let start = usize::try_from(offset.get()).map_err(|_| StorageError::Failed)?;
        let wanted = usize::try_from(len.get()).map_err(|_| StorageError::Failed)?;
        let available = bytes.get(start..).unwrap_or_default();
        if available.len() < wanted {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(available.len()).unwrap_or(u32::MAX),
            });
        }
        Ok(available[..wanted].to_vec())
    }
}

/// A journal holding only `node`'s genesis frame, as `init` leaves it (§7.7 step 0).
pub fn genesis_journal(config: &Config, node: NodeId) -> Journal {
    let genesis = Genesis::new(config, node, InstanceId::parse(INSTANCE).unwrap()).unwrap();
    let frame = journal::encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap();
    let journal = Journal::default();
    journal.push(&frame);
    journal
}

/// What recovery reads back from `journal` (§7.7 steps 1–4).
pub fn recover(config: &Config, node: NodeId, journal: &Journal) -> Recovered {
    journal::recover(
        journal,
        ByteCount::new(journal.byte_len()),
        config,
        node,
        LogBounds::of(config),
    )
    .unwrap()
}

/// Appends one frame holding `entries` and `hard_state` to `journal`, as a node would
/// have written it before a restart.
pub fn write_frame(
    config: &Config,
    node: NodeId,
    journal: &Journal,
    entries: Vec<Entry>,
    hard_state: HardState,
) {
    let (_, mut log) = recover(config, node, journal).into_parts();
    let frame = log
        .append(RaftBatch::new(entries, Some(hard_state)).unwrap())
        .unwrap();
    journal.push(&frame);
}

pub fn request(n: u8) -> RequestId {
    RequestId::from_bytes([n; 16])
}

pub fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

pub fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

/// A PUT `k = v` with request ID `n`.
pub fn put(n: u8, k: &str, v: &str) -> ClientOp {
    ClientOp::Write {
        request_id: request(n),
        mutation: Mutation::Put {
            key: key(k),
            value: value(v),
        },
    }
}

/// The committed entry at `index`, in `term`, holding the PUT `k = v` with request ID `n`.
pub fn put_entry(index: u64, term: u64, n: u8, k: &str, v: &str) -> Entry {
    let ClientOp::Write {
        request_id,
        mutation,
    } = put(n, k, v)
    else {
        panic!("fixture: put builds a write");
    };
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: EntryPayload::Command(codec::encode(&Command {
            request_id,
            mutation,
        })),
    }
}

/// What the controller handed the recording double, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handed {
    Attempt {
        attempt: AttemptId,
        now: Tick,
        lifecycle: Lifecycle,
    },
    Proposed {
        attempt: AttemptId,
        result: Result<(), ProposeError>,
    },
    Applied {
        index: LogIndex,
        applied: Applied,
    },
    Expired {
        attempt: AttemptId,
        now: Tick,
    },
    StatusChanged {
        role: types::Role,
        lifecycle: Lifecycle,
    },
    Shutdown,
}

/// A [`Requests`] double that records what it is handed. A write attempt on a `Ready`
/// node gets a deadline and is proposed; anything else is only recorded. It answers
/// nothing, except `busy` when the deadline book is full.
#[derive(Debug, Default)]
pub struct Recorder {
    pub handed: Vec<Handed>,
    pub waiting: VecDeque<Proposal>,
}

impl Recorder {
    pub fn attempts(&self) -> Vec<AttemptId> {
        self.handed
            .iter()
            .filter_map(|handed| match handed {
                Handed::Attempt { attempt, .. } => Some(*attempt),
                Handed::Proposed { .. }
                | Handed::Applied { .. }
                | Handed::Expired { .. }
                | Handed::StatusChanged { .. }
                | Handed::Shutdown => None,
            })
            .collect()
    }

    pub fn expired(&self) -> Vec<(AttemptId, Tick)> {
        self.handed
            .iter()
            .filter_map(|handed| match handed {
                Handed::Expired { attempt, now } => Some((*attempt, *now)),
                Handed::Attempt { .. }
                | Handed::Proposed { .. }
                | Handed::Applied { .. }
                | Handed::StatusChanged { .. }
                | Handed::Shutdown => None,
            })
            .collect()
    }

    pub fn applied(&self) -> Vec<(LogIndex, Applied)> {
        self.handed
            .iter()
            .filter_map(|handed| match handed {
                Handed::Applied { index, applied } => Some((*index, applied.clone())),
                Handed::Attempt { .. }
                | Handed::Proposed { .. }
                | Handed::Expired { .. }
                | Handed::StatusChanged { .. }
                | Handed::Shutdown => None,
            })
            .collect()
    }
}

impl Requests for Recorder {
    fn attempt(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        op: ClientOp,
        effects: &mut Vec<Effect>,
    ) {
        self.handed.push(Handed::Attempt {
            attempt,
            now: view.now(),
            lifecycle: view.lifecycle(),
        });
        let ClientOp::Write {
            request_id,
            mutation,
        } = op
        else {
            return;
        };
        if view.lifecycle() != Lifecycle::Ready {
            return;
        }
        if view.start_deadline(attempt).is_err() {
            effects.push(Effect::Respond {
                attempt,
                response: Response::Refused(Refusal::Busy),
            });
            return;
        }
        self.waiting.push_back(Proposal {
            attempt,
            command: codec::encode(&Command {
                request_id,
                mutation,
            }),
        });
    }

    fn next_proposal(&mut self, _view: &mut View<'_>) -> Option<Proposal> {
        self.waiting.pop_front()
    }

    fn proposed(
        &mut self,
        _view: &mut View<'_>,
        attempt: AttemptId,
        result: Result<(), ProposeError>,
        _effects: &mut Vec<Effect>,
    ) {
        self.handed.push(Handed::Proposed { attempt, result });
    }

    fn applied(&mut self, applying: &mut Applying<'_>, _effects: &mut Vec<Effect>) {
        self.handed.push(Handed::Applied {
            index: applying.entry().index.index(),
            applied: applying.applied().clone(),
        });
    }

    fn status_changed(&mut self, view: &mut View<'_>, _effects: &mut Vec<Effect>) {
        self.handed.push(Handed::StatusChanged {
            role: view.status().role,
            lifecycle: view.lifecycle(),
        });
    }

    fn expired(&mut self, view: &mut View<'_>, attempt: AttemptId, _effects: &mut Vec<Effect>) {
        self.handed.push(Handed::Expired {
            attempt,
            now: view.now(),
        });
    }

    fn shutdown(&mut self, _view: &mut View<'_>, _effects: &mut Vec<Effect>) {
        self.handed.push(Handed::Shutdown);
    }
}

/// One node under test: its controller, its journal, and what its effects did.
pub struct Member<Q: Requests = Recorder> {
    pub id: NodeId,
    pub ctl: Controller<Journal, Q>,
    pub journal: Journal,
    /// Every effect, in order.
    pub effects: Vec<Effect>,
    /// Syncs requested and not completed yet, in order.
    pub syncs: VecDeque<SyncSeq>,
    /// Sealed envelopes sent and not delivered yet.
    pub outbox: VecDeque<(NodeId, Vec<u8>)>,
}

impl Member {
    /// Starts `id` over `journal`, recovering it first, with a recording double.
    pub fn start(config: &Config, id: NodeId, journal: Journal, seed: u64) -> Self {
        Self::start_with(config, id, journal, seed, Recorder::default())
    }

    /// A fresh node `id`: genesis only.
    pub fn fresh(config: &Config, id: NodeId) -> Self {
        Self::start(config, id, genesis_journal(config, id), id.get())
    }
}

impl Member<Writes> {
    /// A fresh node `id` whose writes go through the real write path.
    pub fn fresh_writes(config: &Config, id: NodeId) -> Self {
        Self::start_with(
            config,
            id,
            genesis_journal(config, id),
            id.get(),
            Writes::new(config, id),
        )
    }
}

impl<Q: Requests> Member<Q> {
    /// Starts `id` over `journal`, recovering it first.
    pub fn start_with(
        config: &Config,
        id: NodeId,
        journal: Journal,
        seed: u64,
        requests: Q,
    ) -> Self {
        let recovered = recover(config, id, &journal);
        let (ctl, effects) = Controller::start(Boot {
            config,
            node: id,
            recovered,
            reader: journal.clone(),
            entropy: Seeded::new(seed),
            incarnation: INCARNATION,
            first_sync: FIRST_SYNC,
            requests,
        })
        .unwrap();
        let mut member = Self {
            id,
            ctl,
            journal,
            effects: Vec::new(),
            syncs: VecDeque::new(),
            outbox: VecDeque::new(),
        };
        member.execute(effects);
        member
    }

    /// Executes `effects` like a composition root: storage effects reach the journal,
    /// syncs and sends wait to be completed or delivered.
    pub fn execute(&mut self, effects: Vec<Effect>) {
        for effect in &effects {
            match effect {
                Effect::Append { frame } => self.journal.push(frame),
                Effect::Truncate { len } => self.journal.truncate(len.get()),
                Effect::Sync { seq, .. } | Effect::SyncDir { seq, .. } => {
                    self.syncs.push_back(*seq);
                }
                Effect::Send { to, bytes } => self.outbox.push_back((*to, bytes.clone())),
                Effect::CreateJournal
                | Effect::Respond { .. }
                | Effect::Log(_)
                | Effect::Observe(_) => {}
            }
        }
        self.effects.extend(effects);
    }

    /// Steps `input` and executes its effects, returning them.
    pub fn step(&mut self, input: Input) -> Vec<Effect> {
        let effects = self.ctl.step(input);
        self.execute(effects.clone());
        effects
    }

    /// Completes the oldest outstanding sync, if any.
    pub fn complete_sync(&mut self) -> bool {
        let Some(seq) = self.syncs.pop_front() else {
            return false;
        };
        self.step(Input::SyncDone {
            seq,
            incarnation: INCARNATION,
        });
        true
    }

    /// Ticks until the node asks for a sync (it campaigns), at most `limit` ticks.
    pub fn tick_until_syncing(&mut self, limit: u64) {
        for _ in 0..limit {
            if self.ctl.outstanding_sync().is_some() {
                return;
            }
            self.step(Input::Tick);
        }
        assert!(
            self.ctl.outstanding_sync().is_some(),
            "fixture: node {} asked for no sync in {limit} ticks",
            self.id
        );
    }

    /// Ticks, completing every sync, until the node leads, at most `limit` ticks.
    pub fn elect(&mut self, limit: u64) {
        for _ in 0..limit {
            while self.complete_sync() {}
            if self.ctl.status().role == types::Role::Leader {
                return;
            }
            self.step(Input::Tick);
        }
        panic!("fixture: node {} was not elected in {limit} ticks", self.id);
    }

    /// The log records the node wrote, in order.
    pub fn logs(&self) -> Vec<LogEvent> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Log(event) => Some(*event),
                Effect::Append { .. }
                | Effect::Truncate { .. }
                | Effect::Sync { .. }
                | Effect::CreateJournal
                | Effect::SyncDir { .. }
                | Effect::Send { .. }
                | Effect::Respond { .. }
                | Effect::Observe(_) => None,
            })
            .collect()
    }
}

/// The write path behind the seam, with every other operation only recorded: the
/// request lifecycle W14.2's tests drive through the controller (the read path is W14.3's).
#[derive(Debug)]
pub struct Writes {
    pub path: WritePath,
    /// Attempts that were not writes, in order.
    pub reads: Vec<AttemptId>,
}

impl Writes {
    pub fn new(config: &Config, node: NodeId) -> Self {
        Self {
            path: WritePath::new(config, node),
            reads: Vec::new(),
        }
    }
}

impl Requests for Writes {
    fn attempt(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        op: ClientOp,
        effects: &mut Vec<Effect>,
    ) {
        match op {
            ClientOp::Write {
                request_id,
                mutation,
            } => self
                .path
                .attempt(view, attempt, request_id, mutation, effects),
            ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => {
                self.reads.push(attempt);
            }
        }
    }

    fn next_proposal(&mut self, view: &mut View<'_>) -> Option<Proposal> {
        self.path.next_proposal(view)
    }

    fn proposed(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        result: Result<(), ProposeError>,
        effects: &mut Vec<Effect>,
    ) {
        self.path.proposed(view, attempt, result, effects);
    }

    fn applied(&mut self, applying: &mut Applying<'_>, effects: &mut Vec<Effect>) {
        self.path.applied(applying, effects);
    }

    fn expired(&mut self, view: &mut View<'_>, attempt: AttemptId, effects: &mut Vec<Effect>) {
        self.path.expired(view, attempt, effects);
    }

    fn status_changed(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        self.path.status_changed(view, effects);
    }

    fn shutdown(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>) {
        self.path.shutdown(view, effects);
    }
}
