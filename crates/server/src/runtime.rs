//! The production node (PLAN.md §7.12): one serial controller task owning the core, fed
//! through one bounded hand-off, with its file I/O, clock and log on threads of their own.
//!
//! [`start`] takes a data directory that passed the preflight ([`crate::run::prepare`]):
//! - the core: `node::Controller` with the node's request lifecycle, a second (unlocked)
//!   handle on the journal for its bounded reads, boot-seeded entropy, and an incarnation
//!   derived from the seed;
//! - the **controller thread**, the only owner of the core. On each wake it takes everything
//!   pending in the hand-off ([`crate::inbox`]), admits all but the last input and steps the
//!   last, routes the effects, and publishes a [`Snapshot`];
//! - the **I/O thread** ([`crate::io`]), the **ticker** ([`crate::clock`]) and the **log
//!   writer** ([`crate::logs`]).
//!
//! Effects go where §7.2 says: storage effects to the I/O thread, in order; `Respond` and
//! `Send` to the [`Outputs`] the HTTP adapters supply (W18.3); `Log` and `Observe` to the log.
//! Adapters only translate (CD-1): admission, deadlines and every answer are the core's.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fs_storage::FsStorage;
use node::{Boot, Controller, RequestLifecycle, StartError};
use serde_json::json;
use types::{
    AttemptId, ClientOp, Config, Effect, Inbound, Incarnation, Input, Lifecycle, Limit, LogIndex,
    LogLevel, NodeId, Response, Role, SyncSeq, Term,
};

use crate::clock;
use crate::entropy::{BootEntropy, boot_seed};
use crate::inbox::{Inbox, Refused};
use crate::io::{Io, MAX_QUEUED_EFFECTS};
use crate::logs::{Log, base, lifecycle_name};
use crate::run::Prepared;

/// Where the core's answers and peer messages go: supplied by the HTTP adapters (W18.3).
/// Both are called on the controller thread, so they must hand off quickly.
pub struct Outputs {
    /// Completes the client attempt `attempt` with `response`.
    pub respond: Box<dyn FnMut(AttemptId, Response) + Send>,
    /// Sends a sealed peer envelope to a peer.
    pub send: Box<dyn FnMut(NodeId, Vec<u8>) + Send>,
}

/// How to run the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The structured log file (O-3).
    pub log_file: PathBuf,
}

/// The node as last published by the controller, for health and `/v1/cluster` (O-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The node.
    pub node: NodeId,
    /// Its lifecycle.
    pub lifecycle: Lifecycle,
    /// Its Raft role.
    pub role: Role,
    /// Its term.
    pub term: Term,
    /// The leader it believes in.
    pub leader: Option<NodeId>,
    /// Its commit index.
    pub commit: LogIndex,
    /// Its durable last index.
    pub last: LogIndex,
    /// Its applied index.
    pub applied: LogIndex,
    /// The ticks its clock has processed since boot (§7.10).
    pub ticks: u64,
    /// Client attempts the hand-off refused (answered `busy`).
    pub clients_refused: u64,
    /// Peer messages the hand-off dropped.
    pub peers_dropped: u64,
    /// Log records dropped because the log queue was full (O-3).
    pub log_records_dropped: u64,
}

/// Why the node could not start. Nothing was written to the journal.
#[derive(Debug)]
pub enum StartRefusal {
    /// The boot seed or a thread or the log file could not be had from the system.
    System {
        /// What was needed.
        what: &'static str,
        /// The operating system's error.
        error: std::io::Error,
    },
    /// The journal's read handle could not be opened.
    Reader(fs_storage::Failure),
    /// The core refused to start.
    Core(StartError),
}

/// A running node.
pub struct Node {
    id: NodeId,
    seed: u64,
    inbox: Arc<Inbox>,
    snapshot: Arc<Mutex<Snapshot>>,
    log: Log,
    counts: Arc<Mutex<(u64, u64)>>,
    controller: JoinHandle<()>,
    ticker: JoinHandle<()>,
    io: JoinHandle<FsStorage>,
    log_writer: JoinHandle<std::io::Result<()>>,
}

/// Starts `node` of `config` from its prepared data directory.
///
/// # Errors
///
/// [`StartRefusal`] if the system or the core refuses; nothing was written.
pub fn start(
    prepared: Prepared,
    config: &Config,
    node: NodeId,
    outputs: Outputs,
    options: &Options,
) -> Result<Node, StartRefusal> {
    let system = |what| move |error| StartRefusal::System { what, error };
    let seed = boot_seed().map_err(system("a boot seed"))?;
    let entropy = BootEntropy::new(seed);
    let incarnation = Incarnation::new(seed);
    let capacity = |limit| usize::try_from(config.limit(limit)).unwrap_or(usize::MAX);
    let (log, log_writer) = Log::open(&options.log_file, node, capacity(Limit::LogQueueRecords))
        .map_err(system("the log file"))?;
    let mut boot = base(LogLevel::Info, "boot");
    boot.insert("entropy_seed".to_owned(), json!(seed));
    boot.insert("incarnation".to_owned(), json!(incarnation.get()));
    boot.insert(
        "data_dir".to_owned(),
        json!(prepared.data_dir.display().to_string()),
    );
    boot.insert("tick_ms".to_owned(), json!(config.tick_ms()));
    log.record(boot);
    let reader = FsStorage::open(&prepared.data_dir).map_err(StartRefusal::Reader)?;
    let Prepared {
        storage, recovered, ..
    } = prepared;
    let (controller, effects) = Controller::start(Boot {
        config,
        node,
        recovered,
        reader,
        entropy,
        incarnation,
        first_sync: SyncSeq::new(1),
        requests: RequestLifecycle::new(config, node),
    })
    .map_err(StartRefusal::Core)?;
    let inbox = Arc::new(Inbox::new(
        capacity(Limit::ControllerQueueInputs),
        MAX_QUEUED_EFFECTS + 1,
    ));
    let (io, io_thread) = Io::spawn(storage, incarnation, Arc::clone(&inbox), log.clone())
        .map_err(system("the I/O thread"))?;
    let snapshot = Arc::new(Mutex::new(snapshot_of(node, &controller, 0, 0, 0)));
    let counts = Arc::new(Mutex::new((0, 0)));
    let mut task = Task {
        node,
        controller,
        io,
        outputs,
        log: log.clone(),
        snapshot: Arc::clone(&snapshot),
        counts: Arc::clone(&counts),
    };
    task.route(effects);
    task.publish();
    let controller_thread = {
        let inbox = Arc::clone(&inbox);
        thread::Builder::new()
            .name("keyval-controller".to_owned())
            .spawn(move || task.run(&inbox))
            .map_err(system("the controller thread"))?
    };
    let tick = Duration::from_millis(config.tick_ms());
    let ticker = clock::spawn(Arc::clone(&inbox), tick).map_err(system("the ticker"))?;
    Ok(Node {
        id: node,
        seed,
        inbox,
        snapshot,
        log,
        counts,
        controller: controller_thread,
        ticker,
        io: io_thread,
        log_writer,
    })
}

/// The controller thread's state: the core and where its effects go.
struct Task {
    node: NodeId,
    controller: Controller<FsStorage, RequestLifecycle>,
    io: Io,
    outputs: Outputs,
    log: Log,
    snapshot: Arc<Mutex<Snapshot>>,
    counts: Arc<Mutex<(u64, u64)>>,
}

impl Task {
    /// The serial loop: take everything pending, admit all but the last, step the last,
    /// route, publish; until the hand-off closes or the node has shut down.
    fn run(mut self, inbox: &Inbox) {
        loop {
            let batch = inbox.take();
            let closed = batch.closed;
            let shutdown = batch
                .unrefusable
                .iter()
                .any(|input| matches!(input, Input::Shutdown));
            let mut inputs = batch.unrefusable.into_iter().chain(batch.inputs).peekable();
            let mut ticks = batch.ticks;
            // Ticks first, merged by the core; then completions and inputs in arrival order.
            while ticks > 0 {
                ticks -= 1;
                let input = Input::Tick;
                let effects = if ticks == 0 && inputs.peek().is_none() {
                    self.controller.step(input)
                } else {
                    self.controller.admit(input)
                };
                self.route(effects);
            }
            while let Some(input) = inputs.next() {
                let effects = if inputs.peek().is_none() {
                    self.controller.step(input)
                } else {
                    self.controller.admit(input)
                };
                self.route(effects);
            }
            self.publish();
            if shutdown || closed {
                break;
            }
        }
        let mut stopped = base(LogLevel::Info, "stopped");
        stopped.insert(
            "lifecycle".to_owned(),
            json!(lifecycle_name(self.controller.lifecycle())),
        );
        self.log.record(stopped);
    }

    /// Sends each effect where §7.2 says, in emission order.
    fn route(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Append { .. }
                | Effect::Truncate { .. }
                | Effect::Sync { .. }
                | Effect::CreateJournal
                | Effect::SyncDir { .. } => self.io.push(effect),
                Effect::Send { to, bytes } => (self.outputs.send)(to, bytes),
                Effect::Respond { attempt, response } => (self.outputs.respond)(attempt, response),
                Effect::Log(event) => self.log.event(event),
                Effect::Observe(observation) => self.log.observation(observation),
            }
        }
    }

    /// Publishes the node's state for readers outside the controller.
    fn publish(&self) {
        let (clients_refused, peers_dropped) =
            *self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = snapshot_of(
            self.node,
            &self.controller,
            clients_refused,
            peers_dropped,
            self.log.dropped(),
        );
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
    }
}

const fn snapshot_of(
    node: NodeId,
    controller: &Controller<FsStorage, RequestLifecycle>,
    clients_refused: u64,
    peers_dropped: u64,
    log_records_dropped: u64,
) -> Snapshot {
    let status = controller.status();
    Snapshot {
        node,
        lifecycle: controller.lifecycle(),
        role: status.role,
        term: status.term,
        leader: status.leader,
        commit: status.commit,
        last: status.last,
        applied: controller.state().applied_index(),
        ticks: controller.now().get(),
        clients_refused,
        peers_dropped,
        log_records_dropped,
    }
}

/// Why a client attempt was not handed to the core: the adapter answers it `busy` (not
/// submitted) itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

impl Node {
    /// This node.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// The boot seed, as logged.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Hands a client attempt to the core; its answer comes through
    /// [`Outputs::respond`].
    ///
    /// # Errors
    ///
    /// [`Busy`] when the hand-off holds its bound or the node is stopping: the attempt was
    /// not submitted, and the caller answers it `busy` (§7.11).
    pub fn client(&self, attempt: AttemptId, op: ClientOp) -> Result<(), Busy> {
        self.inbox
            .offer(Input::Client { attempt, op })
            .map_err(|(why, _)| {
                if why == Refused::Full {
                    self.count(|counts| counts.0 += 1);
                }
                Busy
            })
    }

    /// Hands a peer envelope to the core, or drops and counts it when the hand-off is
    /// full (§7.11). Returns whether it was handed over.
    #[must_use]
    pub fn peer(&self, from: NodeId, bytes: Vec<u8>) -> bool {
        let offered = self.inbox.offer(Input::Peer { from, bytes });
        if offered.is_err() {
            self.count(|counts| counts.1 += 1);
            let mut record = base(LogLevel::Warn, "input_dropped");
            record.insert("input".to_owned(), json!("peer_message"));
            record.insert("bound".to_owned(), json!("controller_queue_inputs"));
            self.log.record(record);
        }
        offered.is_ok()
    }

    fn count(&self, bump: impl FnOnce(&mut (u64, u64))) {
        bump(&mut self.counts.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// The node as the controller last published it.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops the node: the core answers what waits (§7.8 shutdown), the controller
    /// stops, then the clock, the I/O thread and the log writer. Returns the last
    /// snapshot.
    ///
    /// # Errors
    ///
    /// The name of a thread that panicked, or the log writer's error.
    #[must_use = "a thread that panicked is reported here"]
    pub fn shutdown(self) -> Result<Snapshot, String> {
        self.inbox.complete(Input::Shutdown);
        let mut failed: Vec<String> = Vec::new();
        if self.controller.join().is_err() {
            failed.push("controller".to_owned());
        }
        self.inbox.close();
        if self.ticker.join().is_err() {
            failed.push("ticker".to_owned());
        }
        if self.io.join().is_err() {
            failed.push("io".to_owned());
        }
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        drop(self.log);
        match self.log_writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failed.push(format!("log writer: {error}")),
            Err(_) => failed.push("log writer".to_owned()),
        }
        if failed.is_empty() {
            Ok(snapshot)
        } else {
            Err(failed.join(", "))
        }
    }
}
