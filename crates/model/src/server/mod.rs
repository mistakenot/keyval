//! The reference model server (PLAN.md §10.14 use 3, W30).
//!
//! A test-only, single-node HTTP server over the reference model that implements
//! `api/openapi.yaml`, so the conformance suite's semantic tests can be validated before
//! the real server exists (§10.8).
//!
//! It is the model behind the contract's own parsing rules, nothing more: one node, always
//! ready, the leader of term 1, every admitted write applied as the next log entry at once
//! (DM-7's `max_log_entries` admits it; the byte ceiling waits for br-camk). Topology and
//! lifecycle behaviour (`not_leader`, `unavailable`, `recovering`, `unknown_outcome`) is
//! out of its scope (§10.14).
//!
//! Plain `std` HTTP/1.1 and `serde_json` for strict JSON (owner decisions on
//! br-w30-1-model-server-impl-eop): a thread per connection, one lock around the model,
//! at most [`MAX_CONNECTIONS`] connections at once (CD-9).

mod api;
mod http;
mod query;

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod socket_tests;

use std::fmt;
use std::io::{self, BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::data::MAX_INTEGER;
use crate::{HistoryLimits, Limits, Model};

pub use http::{MAX_BODY_BYTES, MAX_HEAD_BYTES};

/// The most connections served at once; a connection beyond it is closed unanswered. Room
/// for a conformance suite's pooled clients.
pub const MAX_CONNECTIONS: usize = 256;

/// How long a connection may sit idle between requests before it is closed.
///
/// Longer than common HTTP clients keep an idle pooled connection (90 s), so the server is
/// not the side that races a reused connection's close.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long, and how many unread bytes, a closing connection drains after its last
/// response, so that a client still sending (an over-long body, say) reads the response
/// instead of a connection reset.
pub const LINGER: (Duration, u64) = (Duration::from_secs(2), 8 << 20);

/// A model server's configuration: its node ID, cluster ID and semantic limits (§7.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    node_id: u64,
    cluster_id: String,
    limits: Limits,
    history: HistoryLimits,
}

/// A configuration the contract cannot carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A node ID outside 1 to 2^53 - 1 (§8.1, R-22).
    NodeId(u64),
    /// An empty cluster ID.
    ClusterId,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeId(id) => write!(f, "node ID {id} is not between 1 and 2^53 - 1"),
            Self::ClusterId => f.write_str("the cluster ID is empty"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// A configuration for node `node_id` of cluster `cluster_id` under `limits` and the
    /// history capacity `history`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a node ID outside 1 to 2^53 - 1 or an empty cluster ID.
    pub fn new(
        node_id: u64,
        cluster_id: impl Into<String>,
        limits: Limits,
        history: HistoryLimits,
    ) -> Result<Self, ConfigError> {
        let cluster_id = cluster_id.into();
        if node_id == 0 || node_id > MAX_INTEGER {
            return Err(ConfigError::NodeId(node_id));
        }
        if cluster_id.is_empty() {
            return Err(ConfigError::ClusterId);
        }
        Ok(Self {
            node_id,
            cluster_id,
            limits,
            history,
        })
    }

    /// The node ID.
    #[must_use]
    pub const fn node_id(&self) -> u64 {
        self.node_id
    }

    /// The cluster ID.
    #[must_use]
    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    /// The semantic limits.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// The history capacity (DM-7).
    #[must_use]
    pub const fn history(&self) -> HistoryLimits {
        self.history
    }
}

/// A bound model server, ready to serve.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    state: Arc<Mutex<api::State>>,
    active: Arc<AtomicUsize>,
}

impl Server {
    /// Binds `address` (port 0 picks a free port) over a fresh model under `config`.
    ///
    /// # Errors
    ///
    /// The bind's I/O error.
    pub fn bind(address: impl ToSocketAddrs, config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        let url = format!("http://{}", listener.local_addr()?);
        let state = api::State {
            model: Model::new(config.limits),
            config,
            url,
        };
        Ok(Self {
            listener,
            state: Arc::new(Mutex::new(state)),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The address it listens on.
    ///
    /// # Errors
    ///
    /// The socket's I/O error.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves connections until accepting fails, each on its own thread.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the listener.
    pub fn serve(self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            // A failed accept (an aborted connection, a full descriptor table) ends only
            // that connection; the pause keeps a persistent failure from spinning.
            let Ok(stream) = stream else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            if self.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                self.active.fetch_sub(1, Ordering::SeqCst);
                drop(stream);
                continue;
            }
            let state = Arc::clone(&self.state);
            let active = Arc::clone(&self.active);
            let finished = Arc::clone(&self.active);
            let spawned = thread::Builder::new().spawn(move || {
                // The slot is freed however the connection ends, a panic included.
                let _slot = Slot(active);
                // A connection's I/O error ends only that connection.
                let _ = connection(&stream, &state);
            });
            if spawned.is_err() {
                finished.fetch_sub(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

/// One of the [`MAX_CONNECTIONS`] slots, given back when its connection's thread ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Half-closes the connection and drains what the client is still sending, within
/// [`LINGER`], so its unread bytes do not turn the last response into a reset.
fn linger(stream: &TcpStream, reader: &mut BufReader<TcpStream>) {
    let (time, bytes) = LINGER;
    if stream.shutdown(Shutdown::Write).is_ok() && stream.set_read_timeout(Some(time)).is_ok() {
        let _ = io::copy(&mut reader.by_ref().take(bytes), &mut io::sink());
    }
}

/// Serves one connection's requests in order until it closes.
fn connection(stream: &TcpStream, state: &Mutex<api::State>) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream.try_clone()?;
    loop {
        let response = match http::read_request(&mut reader, &mut writer) {
            Ok(request) => {
                let mut response = {
                    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                    api::handle(&mut state, &request)
                };
                // A HEAD's answer has a body the client will not read: close after it.
                response.close |= request.close || request.method == "HEAD";
                response
            }
            Err(http::ReadError::Malformed(why)) => {
                let mut response = http::Response::new(
                    400,
                    json!({"error": {"code": "invalid_request", "message": why, "details": {}}}),
                );
                response.close = true;
                response
            }
            Err(http::ReadError::Closed | http::ReadError::Io(_)) => return Ok(()),
        };
        http::write_response(&mut writer, &response)?;
        if response.close {
            linger(stream, &mut reader);
            return Ok(());
        }
    }
}
