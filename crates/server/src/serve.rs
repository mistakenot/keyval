//! The client API server (PLAN.md §7.12 "Mapping"): a listener, one thread per connection,
//! and the responders that complete each HTTP response with the core's `Respond` effect.
//!
//! Each request is parsed by [`crate::api::route`]. A client attempt gets a fresh attempt ID
//! and a one-shot channel registered under it, and is handed to the node. The controller's
//! `Respond` effect sends the core's answer down that channel, and the connection renders
//! it. A lost connection after the hand-over is simply an unresolved attempt; nothing is
//! rolled back (§7.12).
//!
//! Bounds (CD-9):
//! - Connections are at most `pending_attempts` (256), so the responders, at most one
//!   waiting attempt per connection (no pipelining), are bounded too. A connection past
//!   the bound is answered `429 busy` and closed.
//! - An idle connection closes after [`IDLE_TIMEOUT`].
//! - A failing `accept()` is retried after a pause that doubles from [`ACCEPT_BACKOFF_MIN`]
//!   to at most [`ACCEPT_BACKOFF_MAX`] and resets on the next success, so a persistent
//!   error (for example no file descriptors left) costs about a hundred cheap wakeups a second
//!   (it polls the stop flag every 10 ms), not a spin.
//!   Each failure is counted in [`Serving::accept_errors`] (a saturating `u64`).

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use types::{AttemptId, ClientOp, Config, Limit, NodeId, Response as CoreResponse};

use crate::api::{self, Attempt, Routed, View};
use crate::http::{self, ReadError, Response};
use crate::peer::Peers;
use crate::runtime::{Busy, Node, Outputs};

/// How long an idle keep-alive connection stays open.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a connection waits for the core's answer beyond the write deadline before
/// answering a write `unknown_outcome` and a read `unavailable`. The core answers every
/// attempt it took by its deadline (§7.8); this only guards against a stopped node.
const ANSWER_SLACK: Duration = Duration::from_secs(5);

/// The pause after the first failed `accept()`; it doubles with each further failure in a row.
pub const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);

/// The longest pause between failed `accept()` calls.
pub const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// How often a backing-off accept loop looks at the stop flag.
const ACCEPT_BACKOFF_SLICE: Duration = Duration::from_millis(10);

/// A source of client connections: the listener, or a stand-in that fails on demand.
pub trait Accept: Send + 'static {
    /// Waits for the next connection.
    ///
    /// # Errors
    ///
    /// The operating system's error, which may persist (no descriptors left, say).
    fn accept(&self) -> io::Result<TcpStream>;
}

impl Accept for TcpListener {
    fn accept(&self) -> io::Result<TcpStream> {
        Self::accept(self).map(|(stream, _)| stream)
    }
}

/// The attempts waiting for the core's answer, by attempt ID.
#[derive(Debug, Default)]
pub struct Responders {
    waiting: Mutex<BTreeMap<AttemptId, SyncSender<CoreResponse>>>,
    next: AtomicU64,
}

impl Responders {
    /// No attempt waits yet.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The runtime's outputs for a node without peers: each `Respond` completes its waiting
    /// attempt, and a peer message, which a single voter never sends, is dropped.
    #[must_use]
    pub fn outputs(self: &Arc<Self>) -> Outputs {
        self.outputs_to(|_, _| {})
    }

    /// The runtime's outputs: each `Respond` completes its waiting attempt, and each sealed
    /// peer envelope goes to `send` (the peer transport's queues, W21.1).
    #[must_use]
    pub fn outputs_sending(self: &Arc<Self>, peers: Arc<Peers>) -> Outputs {
        self.outputs_to(move |to, envelope| peers.send(to, envelope))
    }

    fn outputs_to(self: &Arc<Self>, send: impl FnMut(NodeId, Vec<u8>) + Send + 'static) -> Outputs {
        let responders = Arc::clone(self);
        Outputs {
            respond: Box::new(move |attempt, response| {
                let waiting = responders
                    .waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&attempt);
                if let Some(answer) = waiting {
                    // The connection may have given up; its attempt is simply unresolved.
                    let _ = answer.try_send(response);
                }
            }),
            send: Box::new(send),
        }
    }

    /// A fresh attempt ID, and where its answer will arrive.
    fn register(&self) -> (AttemptId, Receiver<CoreResponse>) {
        let attempt = AttemptId::new(self.next.fetch_add(1, Ordering::Relaxed));
        let (answer, answered) = mpsc::sync_channel(1);
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(attempt, answer);
        (attempt, answered)
    }

    /// Stops waiting for `attempt`.
    fn forget(&self, attempt: AttemptId) {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&attempt);
    }

    /// The attempts waiting.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// What every connection shares.
struct Shared {
    node: Arc<Node>,
    responders: Arc<Responders>,
    config: Config,
    answer_within: Duration,
    max_connections: usize,
    connections: AtomicUsize,
    stop: AtomicBool,
    /// Failed `accept()` calls so far.
    accept_errors: AtomicU64,
}

/// A running server.
pub struct Serving {
    address: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

/// Serves the client API on `listener` for `node`.
///
/// # Errors
///
/// The operating system's error if the listener's address or the accept thread cannot be
/// had.
pub fn serve(
    listener: TcpListener,
    node: Arc<Node>,
    responders: Arc<Responders>,
    config: &Config,
) -> io::Result<Serving> {
    let address = listener.local_addr()?;
    serve_on(listener, address, node, responders, config)
}

/// Serves the client API on connections from `acceptor`, reporting `address` as where it
/// listens: [`serve`] with the connection source replaceable, so a test can make `accept()`
/// fail or panic.
///
/// # Errors
///
/// The operating system's error if the accept thread cannot be had.
pub fn serve_on<A: Accept>(
    acceptor: A,
    address: SocketAddr,
    node: Arc<Node>,
    responders: Arc<Responders>,
    config: &Config,
) -> io::Result<Serving> {
    let tick = Duration::from_millis(config.tick_ms());
    let deadline =
        tick.saturating_mul(u32::try_from(config.write_deadline().get()).unwrap_or(u32::MAX));
    let shared = Arc::new(Shared {
        node,
        responders,
        config: config.clone(),
        answer_within: deadline + ANSWER_SLACK,
        max_connections: usize::try_from(config.limit(Limit::PendingAttempts))
            .unwrap_or(usize::MAX),
        connections: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        accept_errors: AtomicU64::new(0),
    });
    // Under the node's watch: a dead accept thread halts the node rather than leave it
    // serving no one (br-7tjt).
    let accept = {
        let accepting = Arc::clone(&shared);
        shared
            .node
            .spawn_watched("keyval-accept", move || accept_loop(&acceptor, &accepting))?
    };
    Ok(Serving {
        address,
        shared,
        accept,
    })
}

impl Serving {
    /// The address it listens on.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// How many `accept()` calls have failed so far.
    #[must_use]
    pub fn accept_errors(&self) -> u64 {
        self.shared.accept_errors.load(Ordering::SeqCst)
    }

    /// Waits until the server stops (it never does on its own).
    ///
    /// # Errors
    ///
    /// If the accept thread panicked.
    pub fn wait(self) -> Result<(), String> {
        self.accept
            .join()
            .map_err(|_| "the accept thread panicked".to_owned())
    }

    /// Stops accepting, waits up to `grace` for open connections to finish, and returns
    /// the node.
    ///
    /// # Errors
    ///
    /// If the accept thread panicked.
    pub fn stop(self, grace: Duration) -> Result<Arc<Node>, String> {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop with a connection of our own.
        let _ = TcpStream::connect(self.address);
        self.accept
            .join()
            .map_err(|_| "the accept thread panicked".to_owned())?;
        let start = std::time::Instant::now();
        while self.shared.connections.load(Ordering::SeqCst) > 0 && start.elapsed() < grace {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(Arc::clone(&self.shared.node))
    }
}

/// The process's exit status when the node halts.
pub const HALT_EXIT_STATUS: u8 = 1;

/// Blocks until the node halts and returns the process's exit status.
///
/// A panic on any watched thread of `node` (the accept thread included) halts it. The report
/// goes to `out`, and the process then stops rather than serve half-alive (CD-7, br-7tjt).
pub fn exit_on_halt(running: &Node, node: NodeId, out: &mut impl io::Write) -> u8 {
    let halt = running.wait_halted();
    let _ = writeln!(
        out,
        "keyval-server: node {node} halted: the {} thread panicked: {}",
        halt.thread, halt.message
    );
    HALT_EXIT_STATUS
}

/// The pause after `failures` failed `accept()` calls in a row (at least one): doubling from
/// [`ACCEPT_BACKOFF_MIN`], never above [`ACCEPT_BACKOFF_MAX`].
#[must_use]
pub fn accept_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    ACCEPT_BACKOFF_MIN
        .saturating_mul(1 << doublings)
        .min(ACCEPT_BACKOFF_MAX)
}

/// Sleeps for `pause`, or until the server is told to stop.
fn pause_unless_stopped(shared: &Shared, pause: Duration) {
    let mut left = pause;
    while !left.is_zero() && !shared.stop.load(Ordering::SeqCst) {
        let slice = left.min(ACCEPT_BACKOFF_SLICE);
        thread::sleep(slice);
        left = left.saturating_sub(slice);
    }
}

fn accept_loop<A: Accept>(acceptor: &A, shared: &Arc<Shared>) {
    let mut failures: u32 = 0;
    loop {
        let stream = acceptor.accept();
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            // Counted, and retried after a growing pause: a persistent error must not spin.
            failures = failures.saturating_add(1);
            let _ = shared
                .accept_errors
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    Some(count.saturating_add(1))
                });
            pause_unless_stopped(shared, accept_backoff(failures));
            continue;
        };
        failures = 0;
        if shared.connections.fetch_add(1, Ordering::SeqCst) >= shared.max_connections {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            let mut refusal = api::busy_connection();
            refusal.close = true;
            let mut writer = &stream;
            let _ = http::write_response(&mut writer, &refusal);
            continue;
        }
        let shared_conn = Arc::clone(shared);
        let spawned = thread::Builder::new()
            .name("keyval-conn".to_owned())
            .spawn(move || {
                let _ = connection(&stream, &shared_conn);
                shared_conn.connections.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The most request bytes a connection that is closing reads and discards, so that a client
/// still sending a body the server refused (§7.11: a body over 512 KiB is not read) can read the
/// refusal. Past it the connection closes regardless: the bound is per connection, held on its
/// thread's stack, and a client that sends more gets the reset it earned.
const CLOSE_DRAIN_BYTES: u64 = 4 * 1024 * 1024;

/// How long a closing connection waits for the client's next bytes while it drains.
const CLOSE_DRAIN_WAIT: Duration = Duration::from_secs(2);

/// Closes a connection after its last response without resetting it: sends the FIN, then reads
/// and drops what the client still sends, within [`CLOSE_DRAIN_BYTES`] and a total of
/// [`CLOSE_DRAIN_WAIT`] (CD-9: a trickling client cannot hold the thread). Closing with unread
/// request bytes makes the kernel send an RST that can overtake the response in the client's
/// receive queue.
fn close_gracefully(stream: &TcpStream, reader: &mut impl Read) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + CLOSE_DRAIN_WAIT;
    let mut remaining = CLOSE_DRAIN_BYTES;
    let mut scratch = [0_u8; 16 * 1024];
    while remaining > 0 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        let want = scratch
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        match reader.read(&mut scratch[..want]) {
            Ok(0) | Err(_) => return,
            Ok(read) => remaining = remaining.saturating_sub(read as u64),
        }
    }
}

/// Serves one connection's requests in order until it closes.
fn connection(stream: &TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
    let mut reader = BufReader::new(stream);
    let mut writer = stream;
    loop {
        let mut continue_to = stream;
        let request = match http::read_request(&mut reader, &mut continue_to) {
            Ok(request) => request,
            Err(ReadError::Closed | ReadError::Io(_)) => return Ok(()),
            Err(ReadError::Malformed(why)) => {
                let written = http::write_response(&mut writer, &api::malformed(&why));
                close_gracefully(stream, &mut reader);
                return written;
            }
        };
        let mut response = answer(&request, shared);
        response.close |= request.close;
        http::write_response(&mut writer, &response)?;
        if response.close {
            close_gracefully(stream, &mut reader);
            return Ok(());
        }
    }
}

/// The response to one request.
fn answer(request: &http::Request, shared: &Shared) -> Response {
    let view = View {
        config: &shared.config,
    };
    match api::route(request, &shared.config) {
        Err(response) => response,
        Ok(Routed::Live) => api::live(&shared.node.snapshot()),
        Ok(Routed::Ready) => api::ready(&shared.node.snapshot()),
        Ok(Routed::Cluster) => api::cluster(&shared.node.snapshot(), view),
        Ok(Routed::Attempt(attempt)) => attempt_response(
            &attempt,
            &shared.responders,
            shared.answer_within,
            view,
            |id, op| shared.node.client(id, op),
        ),
    }
}

/// A client attempt's response: registered for its answer, handed over by `submit`, then
/// rendered once the core answers. A hand-off that refuses is `busy`, not submitted; an
/// answer that does not come within `wait` is `unknown_outcome` for a write.
fn attempt_response(
    attempt: &Attempt,
    responders: &Responders,
    wait: Duration,
    view: View<'_>,
    submit: impl FnOnce(AttemptId, ClientOp) -> Result<(), Busy>,
) -> Response {
    let (id, answered) = responders.register();
    if submit(id, attempt.op.clone()).is_err() {
        responders.forget(id);
        return api::busy(attempt);
    }
    answered.recv_timeout(wait).map_or_else(
        |_| {
            responders.forget(id);
            api::timed_out(attempt, view)
        },
        |response| api::render(attempt, response, view),
    )
}

/// O-9's prominent warnings for `node`: one line per configured address of this node that is
/// not loopback, saying that the API is unauthenticated. Empty for a loopback-only config.
#[must_use]
pub fn exposure_warnings(config: &Config, node: NodeId) -> Vec<String> {
    config
        .exposure_warnings()
        .into_iter()
        .filter(|warning| warning.node() == node)
        .map(|warning| {
            format!(
                "WARNING: the {:?} address {} is not loopback, and the API is unauthenticated \
                 (O-9)",
                warning.role(),
                warning.address()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::time::Duration;

    use serde_json::json;
    use types::{
        ClientOp, Config, EntryIndex, Key, Mutation, NodeId, RequestId, Response as CoreResponse,
        ServedBy, TerminalResult, Value,
    };

    use super::{Responders, attempt_response, exposure_warnings};
    use crate::api::{Attempt, Kind, View};
    use crate::runtime::Busy;

    const ONE_VOTER: &str = include_str!("../../../config/fixtures/one-voter.toml");

    #[test]
    fn a_non_loopback_address_gets_a_prominent_warning_and_loopback_none() {
        let node = NodeId::new(1).unwrap();
        let loopback = Config::parse(ONE_VOTER, Path::new("config")).unwrap();
        let exposed = Config::parse(
            &ONE_VOTER.replace("127.0.0.1:7101", "0.0.0.0:7101"),
            Path::new("config"),
        )
        .unwrap();
        let warnings = exposure_warnings(&exposed, node);
        assert_eq!(
            (
                exposure_warnings(&loopback, node),
                warnings.len(),
                warnings.first().is_some_and(
                    |line| line.contains("0.0.0.0:7101") && line.contains("unauthenticated")
                )
            ),
            (Vec::<String>::new(), 1, true),
            "rule: the default loopback addresses get no warning; a non-loopback client \
             address gets a prominent one saying the API is unauthenticated (O-9); got \
             {warnings:?}"
        );
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn put() -> Attempt {
        let config = Config::parse(ONE_VOTER, Path::new("config")).unwrap();
        let limits = config.key_value_limits();
        let key = Key::parse("k".to_owned(), limits).unwrap();
        let request_id = RequestId::parse(ID).unwrap();
        Attempt {
            op: ClientOp::Write {
                request_id,
                mutation: Mutation::Put {
                    key: key.clone(),
                    value: Value::parse("v".to_owned(), limits).unwrap(),
                },
            },
            kind: Kind::Put { request_id, key },
        }
    }

    #[test]
    fn a_refused_hand_off_is_busy_a_silent_core_unknown_and_an_answer_rendered() {
        let config = Config::parse(ONE_VOTER, Path::new("config")).unwrap();
        let view = View { config: &config };
        let responders = Responders::new();
        let wait = Duration::from_millis(20);
        let refused = attempt_response(&put(), &responders, wait, view, |_, _| Err(Busy));
        let after_refused = responders.waiting();
        let silent = attempt_response(&put(), &responders, wait, view, |_, _| Ok(()));
        let after_silent = responders.waiting();
        let mut outputs = responders.outputs();
        let answered = attempt_response(&put(), &responders, wait, view, |id, _| {
            (outputs.respond)(
                id,
                CoreResponse::Written {
                    result: TerminalResult::executed_at(
                        EntryIndex::new(2).unwrap(),
                        types::Decision::Set,
                    ),
                    replayed: false,
                    served_by: ServedBy {
                        node: NodeId::new(1).unwrap(),
                        applied: types::LogIndex::new(2).unwrap(),
                    },
                },
            );
            Ok(())
        });
        assert_eq!(
            (
                (refused.status, refused.body["error"]["details"].clone()),
                after_refused,
                (
                    silent.status,
                    silent.body["error"]["details"]["attempt_state"].clone()
                ),
                after_silent,
                (answered.status, answered.body["index"].clone()),
                responders.waiting(),
            ),
            (
                (
                    429,
                    json!({"request_id": ID, "attempt_state": "not_submitted"})
                ),
                0,
                (504, json!("submitted")),
                0,
                (200, json!(2)),
                0,
            ),
            "rule: an attempt the hand-off refuses is answered busy, not submitted (§7.11); \
             one handed over that gets no answer in time is unknown_outcome, submitted (A-5); \
             an answer completes the response; no responder is left waiting (CD-9)"
        );
    }
}
