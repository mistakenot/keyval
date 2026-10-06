//! The HTTP peer transport (PLAN.md §8.2, §7.11; W21.1): `POST /raft` on each node's own
//! peer address, carrying one sealed peer envelope per request.
//!
//! The transport only carries bytes (CD-1, P3). The controller seals every message it sends
//! and opens every envelope it receives with the W13 codec, the code the simulated network
//! uses too, so the instance, protocol version, semantic fingerprint, sender and
//! destination checks, and their counted drops, are not repeated here.
//!
//! - **Out.** [`Peers`] keeps one bounded queue and one sender thread per peer. A message
//!   that finds its queue at `peer_send_queue_messages` (64) or `peer_send_queue_bytes`
//!   (8 MiB) is dropped and counted, never queued (§7.11): Raft retransmits, so a slow or
//!   absent peer costs messages, not memory. A send that fails is dropped and counted too.
//! - **In.** [`serve_peers`] listens on the peer address. A well-formed `POST /raft` hands
//!   its body to [`Node::peer`] as coming from the node named in the `X-Keyval-Sender`
//!   header, which the codec then checks against the sender the envelope itself carries, so
//!   an envelope relayed under another node's name is dropped and counted as a sender
//!   mismatch. The controller's inbound bound applies as for every peer message.
//!
//! The peer channel is internal: not part of the public API and not in the conformance
//! suite (§8.2).
//!
//! Bounds (CD-9): the send queues above; at most [`MAX_PEER_CONNECTIONS`] inbound
//! connections, past which a connection is closed unanswered and counted; an envelope body
//! of at most [`raft_adapter::MAX_ENVELOPE_BYTES`], past which the request answers `413`
//! and the connection closes; an idle connection closes after [`IDLE_TIMEOUT`].

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use raft_adapter::MAX_ENVELOPE_BYTES;
use serde_json::json;
use types::{Config, Limit, NodeId};

use crate::http::{self, Body, ReadError, Response};
use crate::runtime::Node;

/// The header naming the node an envelope is sent from (lower-case, as the reader holds it).
pub const SENDER_HEADER: &str = "x-keyval-sender";

/// The peer channel's path.
pub const PATH: &str = "/raft";

/// The most inbound peer connections at once. A node keeps one connection from each of its
/// four peers; the rest is slack for reconnects while an old connection is still closing.
pub const MAX_PEER_CONNECTIONS: usize = 16;

/// How long an idle inbound connection stays open.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a sender waits to connect to a peer, and then for each read or write.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a sender pauses after a failed send before the next, so a peer that is down is
/// not hammered.
const RETRY_PAUSE: Duration = Duration::from_millis(50);

/// The most bytes of a peer's response head a sender reads.
const MAX_RESPONSE_HEAD_BYTES: u64 = 16 * 1024;

/// What one peer's send queue has dropped and sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendStats {
    /// Messages handed to the sender thread and posted successfully.
    pub sent: u64,
    /// Messages dropped at `peer_send_queue_messages`.
    pub dropped_queue_messages: u64,
    /// Messages dropped at `peer_send_queue_bytes`.
    pub dropped_queue_bytes: u64,
    /// Messages dropped because the post failed (peer down, connection broken, refused).
    pub dropped_send_failed: u64,
}

#[derive(Debug, Default)]
struct QueueState {
    messages: VecDeque<Vec<u8>>,
    bytes: u64,
    closed: bool,
    stats: SendStats,
}

/// One peer's bounded send queue.
#[derive(Debug)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    max_messages: u64,
    max_bytes: u64,
}

impl Queue {
    fn new(max_messages: u64, max_bytes: u64) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            max_messages,
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `message`, or drops and counts it when either bound would be passed.
    fn push(&self, message: Vec<u8>) {
        let length = u64::try_from(message.len()).unwrap_or(u64::MAX);
        let queued = {
            let mut state = self.lock();
            let held = u64::try_from(state.messages.len()).unwrap_or(u64::MAX);
            if state.closed {
                state.stats.dropped_send_failed += 1;
                false
            } else if held >= self.max_messages {
                state.stats.dropped_queue_messages += 1;
                false
            } else if state.bytes.saturating_add(length) > self.max_bytes {
                state.stats.dropped_queue_bytes += 1;
                false
            } else {
                state.bytes += length;
                state.messages.push_back(message);
                true
            }
        };
        if queued {
            self.ready.notify_one();
        }
    }

    /// The next message, waiting for one; `None` once the queue is closed.
    fn next(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(message) = state.messages.pop_front() {
                state.bytes -= u64::try_from(message.len()).unwrap_or(u64::MAX);
                return Some(message);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    fn sent(&self) {
        self.lock().stats.sent += 1;
    }

    fn failed(&self) {
        self.lock().stats.dropped_send_failed += 1;
    }

    fn stats(&self) -> SendStats {
        self.lock().stats
    }

    fn held(&self) -> (usize, u64) {
        let state = self.lock();
        (state.messages.len(), state.bytes)
    }
}

/// The outbound side: one bounded queue and sender thread per peer.
#[derive(Debug)]
pub struct Peers {
    node: NodeId,
    queues: BTreeMap<NodeId, Arc<Queue>>,
    addresses: BTreeMap<NodeId, SocketAddr>,
}

impl Peers {
    /// The queues for every other voter in `config`, bounded by its send-queue limits. No
    /// thread runs until [`Peers::spawn`].
    #[must_use]
    pub fn new(config: &Config, node: NodeId) -> Arc<Self> {
        let messages = config.limit(Limit::PeerQueueMessages);
        let bytes = config.limit(Limit::PeerQueueBytes);
        let mut queues = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for peer in config.nodes().filter(|peer| peer.id() != node) {
            queues.insert(peer.id(), Arc::new(Queue::new(messages, bytes)));
            addresses.insert(peer.id(), peer.peer_address());
        }
        Arc::new(Self {
            node,
            queues,
            addresses,
        })
    }

    /// Queues a sealed envelope for `to`, or drops and counts it (§7.11). A message for a
    /// node that is not a peer is dropped and counted nowhere: the controller only sends to
    /// voters.
    pub fn send(&self, to: NodeId, envelope: Vec<u8>) {
        if let Some(queue) = self.queues.get(&to) {
            queue.push(envelope);
        }
    }

    /// Starts one sender thread per peer, under `node`'s watch: a dead sender halts the node
    /// rather than leave a peer silently unreachable (br-7tjt).
    ///
    /// # Errors
    ///
    /// The operating system's error if a thread cannot be had.
    pub fn spawn(self: &Arc<Self>, node: &Node) -> io::Result<Vec<JoinHandle<()>>> {
        let mut handles = Vec::new();
        for (peer, queue) in &self.queues {
            let Some(address) = self.addresses.get(peer).copied() else {
                continue;
            };
            let queue = Arc::clone(queue);
            let from = self.node;
            handles.push(node.spawn_watched(&format!("keyval-peer-{peer}"), move || {
                sender(&queue, address, from);
            })?);
        }
        Ok(handles)
    }

    /// What each peer's queue has dropped and sent.
    #[must_use]
    pub fn stats(&self) -> BTreeMap<NodeId, SendStats> {
        self.queues
            .iter()
            .map(|(peer, queue)| (*peer, queue.stats()))
            .collect()
    }

    /// The messages and bytes queued for `peer`.
    #[must_use]
    pub fn held(&self, peer: NodeId) -> Option<(usize, u64)> {
        self.queues.get(&peer).map(|queue| queue.held())
    }

    /// Stops the senders; what is queued is dropped.
    pub fn close(&self) {
        for queue in self.queues.values() {
            queue.close();
        }
    }
}

/// One peer's sender: posts each queued envelope in order, over one kept-alive connection.
fn sender(queue: &Queue, address: SocketAddr, from: NodeId) {
    let mut connection: Option<Connection> = None;
    while let Some(envelope) = queue.next() {
        if post(&mut connection, address, from, &envelope).is_ok() {
            queue.sent();
        } else {
            queue.failed();
            connection = None;
            thread::sleep(RETRY_PAUSE);
        }
    }
}

struct Connection {
    reader: BufReader<TcpStream>,
}

/// Posts `envelope`, retrying once on a fresh connection when a kept-alive one has gone
/// stale.
fn post(
    connection: &mut Option<Connection>,
    address: SocketAddr,
    from: NodeId,
    envelope: &[u8],
) -> io::Result<()> {
    if let Some(open) = connection.as_mut() {
        if exchange(open, address, from, envelope).is_ok() {
            return Ok(());
        }
        *connection = None;
    }
    let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let mut fresh = Connection {
        reader: BufReader::new(stream),
    };
    exchange(&mut fresh, address, from, envelope)?;
    *connection = Some(fresh);
    Ok(())
}

/// One request and its response on `connection`.
fn exchange(
    connection: &mut Connection,
    address: SocketAddr,
    from: NodeId,
    envelope: &[u8],
) -> io::Result<()> {
    let head = format!(
        "POST {PATH} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/octet-stream\r\n\
         X-Keyval-Sender: {from}\r\nContent-Length: {}\r\n\r\n",
        envelope.len()
    );
    let writer = connection.reader.get_mut();
    writer.write_all(head.as_bytes())?;
    writer.write_all(envelope)?;
    writer.flush()?;
    let status = read_status(&mut connection.reader)?;
    if status == 200 {
        Ok(())
    } else {
        Err(io::Error::other(format!("the peer answered {status}")))
    }
}

/// Reads one response, returning its status and consuming its body.
fn read_status(reader: &mut BufReader<TcpStream>) -> io::Result<u16> {
    let mut line = String::new();
    let mut budget = MAX_RESPONSE_HEAD_BYTES;
    let mut take = |reader: &mut BufReader<TcpStream>, line: &mut String| -> io::Result<()> {
        line.clear();
        let read = reader.by_ref().take(budget).read_line(line)?;
        budget = budget.saturating_sub(u64::try_from(read).unwrap_or(budget));
        if read == 0 || !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer's response ended inside its head",
            ));
        }
        Ok(())
    };
    take(reader, &mut line)?;
    let status = line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other(format!("bad status line {line:?}")))?;
    let mut length = 0_usize;
    loop {
        take(reader, &mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().map_err(io::Error::other)?;
        }
    }
    // The body is a small JSON acknowledgement; a peer that sends more is broken.
    if length > 4096 {
        return Err(io::Error::other("the peer's response body is oversized"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(status)
}

/// What the listener has refused, by reason (all counted, §7.11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveStats {
    /// Envelopes handed to the node (the controller may still drop them).
    pub handed_over: u64,
    /// Envelopes the node's hand-off refused (the §7.11 inbound bound).
    pub refused_busy: u64,
    /// Requests without exactly one valid `X-Keyval-Sender`.
    pub bad_sender: u64,
    /// Bodies over the envelope limit.
    pub too_large: u64,
    /// Requests that were not `POST /raft`, or not well-formed HTTP.
    pub bad_request: u64,
    /// Connections closed unanswered at [`MAX_PEER_CONNECTIONS`].
    pub refused_connections: u64,
}

#[derive(Debug, Default)]
struct Counters {
    handed_over: AtomicU64,
    refused_busy: AtomicU64,
    bad_sender: AtomicU64,
    too_large: AtomicU64,
    bad_request: AtomicU64,
    refused_connections: AtomicU64,
}

struct Shared {
    node: Arc<Node>,
    counters: Counters,
    connections: AtomicUsize,
    stop: AtomicBool,
}

/// A running peer listener.
pub struct PeerServing {
    address: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

/// Serves the peer channel on `listener` for `node`.
///
/// # Errors
///
/// The operating system's error if the listener's address or the accept thread cannot be
/// had.
pub fn serve_peers(listener: TcpListener, node: Arc<Node>) -> io::Result<PeerServing> {
    let address = listener.local_addr()?;
    let shared = Arc::new(Shared {
        node,
        counters: Counters::default(),
        connections: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    });
    let accept = {
        let accepting = Arc::clone(&shared);
        shared.node.spawn_watched("keyval-peer-accept", move || {
            accept_loop(&listener, &accepting);
        })?
    };
    Ok(PeerServing {
        address,
        shared,
        accept,
    })
}

impl PeerServing {
    /// The address it listens on.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// What it has refused so far.
    #[must_use]
    pub fn stats(&self) -> ReceiveStats {
        let counters = &self.shared.counters;
        ReceiveStats {
            handed_over: counters.handed_over.load(Ordering::SeqCst),
            refused_busy: counters.refused_busy.load(Ordering::SeqCst),
            bad_sender: counters.bad_sender.load(Ordering::SeqCst),
            too_large: counters.too_large.load(Ordering::SeqCst),
            bad_request: counters.bad_request.load(Ordering::SeqCst),
            refused_connections: counters.refused_connections.load(Ordering::SeqCst),
        }
    }

    /// Stops accepting. Open connections end when their peers close them or go idle.
    ///
    /// # Errors
    ///
    /// If the accept thread panicked.
    pub fn stop(self) -> Result<(), String> {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop with a connection of our own.
        let _ = TcpStream::connect(self.address);
        self.accept
            .join()
            .map_err(|_| "the peer accept thread panicked".to_owned())
    }

    /// Waits until the listener stops (it never does on its own).
    ///
    /// # Errors
    ///
    /// If the accept thread panicked.
    pub fn wait(self) -> Result<(), String> {
        self.accept
            .join()
            .map_err(|_| "the peer accept thread panicked".to_owned())
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            continue;
        };
        if shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_PEER_CONNECTIONS {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            shared
                .counters
                .refused_connections
                .fetch_add(1, Ordering::SeqCst);
            continue;
        }
        let shared_conn = Arc::clone(shared);
        let spawned = thread::Builder::new()
            .name("keyval-peer-conn".to_owned())
            .spawn(move || {
                let _ = connection(&stream, &shared_conn);
                shared_conn.connections.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Serves one peer connection's requests in order until it closes.
fn connection(stream: &TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream);
    let mut writer = stream;
    let limit = usize::try_from(MAX_ENVELOPE_BYTES).unwrap_or(usize::MAX);
    loop {
        let mut continue_to = stream;
        let request = match http::read_request_limited(&mut reader, &mut continue_to, limit) {
            Ok(request) => request,
            Err(ReadError::Closed | ReadError::Io(_)) => return Ok(()),
            Err(ReadError::Malformed(why)) => {
                shared.counters.bad_request.fetch_add(1, Ordering::SeqCst);
                let mut refusal = Response::json(400, json!({"error": why}));
                refusal.close = true;
                return http::write_response(&mut writer, &refusal);
            }
        };
        let mut response = answer(&request, shared);
        response.close |= request.close;
        http::write_response(&mut writer, &response)?;
        if response.close {
            return Ok(());
        }
    }
}

/// The sender a request names: exactly one `X-Keyval-Sender` holding a node ID.
fn sender_of(request: &http::Request) -> Option<NodeId> {
    match request.field(SENDER_HEADER)[..] {
        [value] => value
            .parse::<u64>()
            .ok()
            .and_then(|id| NodeId::new(id).ok()),
        _ => None,
    }
}

/// The response to one peer request.
fn answer(request: &http::Request, shared: &Shared) -> Response {
    let counters = &shared.counters;
    if request.path != PATH {
        counters.bad_request.fetch_add(1, Ordering::SeqCst);
        return Response::json(
            404,
            json!({"error": "the peer channel serves POST /raft only"}),
        );
    }
    if request.method != "POST" {
        counters.bad_request.fetch_add(1, Ordering::SeqCst);
        let mut refusal = Response::json(405, json!({"error": "use POST"}));
        refusal.allow = Some("POST");
        return refusal;
    }
    let envelope = match &request.body {
        Body::Bytes(bytes) => bytes,
        Body::TooLarge => {
            counters.too_large.fetch_add(1, Ordering::SeqCst);
            return Response::json(413, json!({"error": "the envelope is over the limit"}));
        }
    };
    let Some(from) = sender_of(request) else {
        counters.bad_sender.fetch_add(1, Ordering::SeqCst);
        return Response::json(
            400,
            json!({"error": "exactly one X-Keyval-Sender header holding a node ID is required"}),
        );
    };
    // Only the envelope's identity checks can say more, and the controller makes them: this
    // answer says whether the node took the envelope, not whether it was valid.
    let handed = shared.node.peer(from, envelope.clone());
    let counter = if handed {
        &counters.handed_over
    } else {
        &counters.refused_busy
    };
    counter.fetch_add(1, Ordering::SeqCst);
    Response::json(200, json!({"accepted": handed}))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use types::{Config, NodeId};

    use super::{Peers, Queue, SendStats, sender_of};
    use crate::http::{Body, Request};

    fn request(headers: &[(&str, &str)]) -> Request {
        Request {
            method: "POST".to_owned(),
            path: "/raft".to_owned(),
            query: None,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Body::Bytes(Vec::new()),
            close: false,
        }
    }

    #[test]
    fn a_queue_takes_its_message_bound_then_drops_and_counts() {
        let queue = Queue::new(64, 8 << 20);
        for _ in 0..65 {
            queue.push(vec![0; 10]);
        }
        assert_eq!(
            (queue.held(), queue.stats()),
            (
                (64, 640),
                SendStats {
                    dropped_queue_messages: 1,
                    ..SendStats::default()
                }
            ),
            "rule: a send queue holds 64 messages and drops and counts the 65th (§7.11)"
        );
    }

    #[test]
    fn a_queue_takes_its_byte_bound_exactly_then_drops_and_counts() {
        let queue = Queue::new(64, 100);
        queue.push(vec![0; 60]);
        queue.push(vec![0; 40]);
        queue.push(vec![0; 1]);
        let first = queue.next();
        queue.push(vec![0; 60]);
        assert_eq!(
            (
                first.map(|message| message.len()),
                queue.held(),
                queue.stats()
            ),
            (
                Some(60),
                (2, 100),
                SendStats {
                    dropped_queue_bytes: 1,
                    ..SendStats::default()
                }
            ),
            "rule: a send queue holds exactly its byte bound, drops and counts what would pass \
             it, and takes messages again once the sender has drained some (§7.11)"
        );
    }

    #[test]
    fn a_closed_queue_ends_its_sender_and_drops_what_comes() {
        let queue = Queue::new(64, 100);
        queue.push(vec![0; 1]);
        queue.close();
        queue.push(vec![0; 1]);
        assert_eq!(
            (queue.next(), queue.stats().dropped_send_failed),
            (None, 1),
            "rule: a closed queue gives its sender nothing more and counts later sends dropped"
        );
    }

    #[test]
    fn peers_are_every_other_voter_and_a_send_to_a_stranger_is_ignored() {
        let config = Config::parse(
            include_str!("../../../config/local.toml"),
            Path::new("config"),
        )
        .unwrap();
        let me = NodeId::new(1).unwrap();
        let peers = Peers::new(&config, me);
        peers.send(NodeId::new(2).unwrap(), vec![1, 2, 3]);
        peers.send(me, vec![9]);
        peers.send(NodeId::new(99).unwrap(), vec![9]);
        assert_eq!(
            (
                peers
                    .stats()
                    .keys()
                    .copied()
                    .map(NodeId::get)
                    .collect::<Vec<_>>(),
                peers.held(NodeId::new(2).unwrap()),
                peers.held(me)
            ),
            (vec![2, 3, 4, 5], Some((1, 3)), None),
            "rule: a node has a send queue for each of the other four voters only"
        );
    }

    #[test]
    fn the_sender_header_must_be_exactly_one_node_id() {
        let none = request(&[]);
        let good = request(&[("x-keyval-sender", "3")]);
        let zero = request(&[("x-keyval-sender", "0")]);
        let word = request(&[("x-keyval-sender", "three")]);
        let twice = request(&[("x-keyval-sender", "3"), ("x-keyval-sender", "3")]);
        assert_eq!(
            (
                sender_of(&none),
                sender_of(&good).map(NodeId::get),
                sender_of(&zero),
                sender_of(&word),
                sender_of(&twice)
            ),
            (None, Some(3), None, None, None),
            "rule: a peer request names its sender once, as a non-zero node ID"
        );
    }
}
