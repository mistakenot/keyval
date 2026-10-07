//! Peer-envelope identity on a real node's peer address (PLAN.md §8.2, W21.3): an envelope
//! whose instance ID, protocol version, semantic fingerprint, sender or destination does not
//! match the receiving node is dropped and counted — it shows as one `input_dropped` record
//! in the node's log each — while a well-formed one from a configured voter is handed to
//! Raft and logs no drop. Each forged envelope is also opened by a local codec of the same
//! identity, which names the one reason it is refused for, so a drop cannot stand for another
//! reason's. The codec's own per-reason counts are PEER-ENVELOPE's (W13); the node does
//! not yet report them by reason, so this suite proves the drop and the count at the wire.
//!
//! Real processes on loopback: test evidence, not proof.
#![cfg(test)]

// dead_code fired: the shared support modules hold helpers this target does not call; they
// are shared with the other test targets, so trimming them per target is not an option
// (br-w21-3-envelope-mismatch-tests-lrc).
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

// dead_code fired: as above (br-w21-3-envelope-mismatch-tests-lrc).
#[allow(dead_code)]
#[path = "support/binary.rs"]
mod harness;

// dead_code fired: as above (br-w21-3-envelope-mismatch-tests-lrc).
#[allow(dead_code)]
#[path = "support/node.rs"]
mod node;

// check: ENVELOPE-IDENTITY

use std::fmt::Write as _;
use std::fs;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use raft_adapter::{EnvelopeError, PeerCodec};
use types::{Config, InstanceId, NodeId, OutboundMessage, PROTOCOL_VERSION, SemanticFingerprint};

use crate::harness::init;
use crate::node::{Node, Spec};
use crate::support::{INSTANCE, free_port, instance, write_config};

const THREE_VOTERS: &str = include_str!("../../../config/fixtures/three-voters.toml");

/// Header offsets of the envelope layout (§8.2): magic, version, instance, fingerprint,
/// sender, destination, payload length.
const VERSION_AT: usize = 4;
const INSTANCE_AT: usize = 12;
const FINGERPRINT_AT: usize = 28;
const SENDER_AT: usize = 60;

fn id(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// A heartbeat from `from` to `to` in term 3, as the library encodes it (protobuf
/// `eraftpb.Message`: type `MsgHeartbeat`, to, from, term; commit 0, since a heartbeat whose
/// commit index is past the receiver's log is refused and counted too).
fn heartbeat(from: u64, to: u64) -> Vec<u8> {
    let (from, to) = (u8::try_from(from).unwrap(), u8::try_from(to).unwrap());
    vec![0x08, 0x08, 0x10, to, 0x18, from, 0x20, 0x03]
}

/// Node 1 of a three-voter cluster, alone: its peers are configured but never started.
struct Fixture {
    node: Node,
    peer_address: SocketAddr,
    log: std::path::PathBuf,
    config: Config,
}

fn start(name: &str) -> Fixture {
    let dir = support::scratch(name);
    let mut text = THREE_VOTERS
        .split("[[nodes]]")
        .next()
        .unwrap()
        .replace("keyval-fixture-three-voters", "keyval-envelope-test");
    let mut client = None;
    for node in 1..=3_u64 {
        let address: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let peer: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        client = client.or(Some(address));
        write!(
            text,
            "[[nodes]]\nid = {node}\nclient_address = \"{address}\"\npeer_address = \"{peer}\"\n\
             data_dir = \"node-{node}\"\n\n"
        )
        .unwrap();
    }
    let (path, config) = write_config(&dir, "three-voters.toml", &text);
    let ran = init(&path, "1", INSTANCE, &["--json"]);
    assert_eq!(ran.code, Some(0), "fixture: init; stderr {}", ran.stderr);
    let log = dir.join("node-1.log");
    let node = Node::start(Spec {
        config: path,
        node: 1,
        cluster: config.cluster_id().to_string(),
        address: client.unwrap(),
        log: log.clone(),
        dir,
    });
    let peer_address = config.node(id(1)).unwrap().peer_address();
    Fixture {
        node,
        peer_address,
        log,
        config,
    }
}

impl Fixture {
    /// The envelope node `from` seals for node `to`.
    fn envelope(&self, from: u64, to: u64) -> Vec<u8> {
        codec(from, instance(), self.config.semantic_fingerprint())
            .seal(&OutboundMessage {
                to: id(to),
                bytes: heartbeat(from, to),
            })
            .unwrap()
    }

    /// Posts `envelope` to the node's peer address as node `sender`; returns the status.
    fn post(&self, sender: u64, envelope: &[u8]) -> u16 {
        let mut head = String::from("POST /raft HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
        write!(
            head,
            "X-Keyval-Sender: {sender}\r\nContent-Length: {}\r\n\r\n",
            envelope.len()
        )
        .unwrap();
        let mut raw = head.into_bytes();
        raw.extend_from_slice(envelope);
        support::send(self.peer_address, &raw).status
    }

    /// `input_dropped` records in the node's log so far.
    fn drops(&self) -> usize {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| {
                record["event"] == "input_dropped" && record["input"] == "peer_message"
            })
            .count()
    }

    /// Waits until the log shows `count` drops, and a little longer to show no more.
    fn settle_at(&self, count: usize, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.drops() < count {
            assert!(
                Instant::now() < deadline,
                "rule: {what} is dropped and counted — {count} `input_dropped` record(s) in the \
                 log within 10 s (§8.2); saw {}; stderr: {}",
                self.drops(),
                self.node.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            self.drops(),
            count,
            "rule: {what} is counted exactly once; log {}",
            fs::read_to_string(&self.log).unwrap_or_default()
        );
    }
}

/// Node `node`'s codec among voters 1 to 3, in `instance` with `fingerprint`.
fn codec(node: u64, instance: InstanceId, fingerprint: SemanticFingerprint) -> PeerCodec {
    PeerCodec::new(instance, fingerprint, id(node), [1, 2, 3].map(id)).unwrap()
}

/// `envelope` with `bytes` written at `offset`.
fn patched(envelope: &[u8], offset: usize, bytes: &[u8]) -> Vec<u8> {
    let mut patched = envelope.to_vec();
    patched[offset..offset + bytes.len()].copy_from_slice(bytes);
    patched
}

mod identity_tests {
    use super::*;

    /// Waits until the node answers on its peer address.
    fn wait_listening(fixture: &Fixture) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::net::TcpStream::connect(fixture.peer_address).is_err() {
            assert!(
                Instant::now() < deadline,
                "fixture: the peer address listens within 20 s; stderr: {}",
                fixture.node.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_matching_envelope_is_not_dropped() {
        let fixture = start("envelope-control");
        wait_listening(&fixture);
        let status = fixture.post(2, &fixture.envelope(2, 1));
        assert_eq!(
            status, 200,
            "rule: a well-formed envelope from a configured voter is accepted (§8.2)"
        );
        fixture.settle_at(0, "a matching envelope (control: not)");
    }

    #[test]
    fn each_identity_mismatch_is_dropped_and_counted_while_a_matching_one_is_not() {
        let fixture = start("envelope-mismatch");
        wait_listening(&fixture);
        let sealed = fixture.envelope(2, 1);
        let other_instance = InstanceId::from_bytes([8; 16]);
        let other_version = PROTOCOL_VERSION + 1;
        let fingerprint = fixture.config.semantic_fingerprint();
        let cases: [(&str, u64, Vec<u8>, EnvelopeError); 7] = [
            (
                "an envelope of another cluster instance",
                2,
                patched(&sealed, INSTANCE_AT, &other_instance.to_bytes()),
                EnvelopeError::Instance,
            ),
            (
                "an envelope of another protocol version",
                2,
                patched(&sealed, VERSION_AT, &other_version.to_le_bytes()),
                EnvelopeError::ProtocolVersion {
                    version: other_version,
                },
            ),
            (
                "an envelope of another semantic fingerprint",
                2,
                patched(&sealed, FINGERPRINT_AT, &[9; 32]),
                EnvelopeError::Fingerprint,
            ),
            (
                "an envelope of node 3, whose inner message agrees, posted as from node 2",
                2,
                fixture.envelope(3, 1),
                EnvelopeError::Sender { sender: 3 },
            ),
            (
                "an envelope naming a sender outside the voter set",
                4,
                patched(&sealed, SENDER_AT, &4_u64.to_le_bytes()),
                EnvelopeError::Sender { sender: 4 },
            ),
            (
                "an envelope naming the receiver itself as sender",
                1,
                patched(&sealed, SENDER_AT, &1_u64.to_le_bytes()),
                EnvelopeError::Sender { sender: 1 },
            ),
            (
                "an envelope addressed to another node",
                2,
                fixture.envelope(2, 3),
                EnvelopeError::Destination { destination: 3 },
            ),
        ];
        let mut expected = 0;
        for (what, sender, envelope, reason) in cases {
            // The node runs this very codec (W13): the reason it drops for is this one, so the
            // log's bare `input_dropped` below is not another reason's drop in disguise.
            let opened = codec(1, instance(), fingerprint).open(id(sender), &envelope);
            assert_eq!(
                opened,
                Err(reason),
                "rule: {what} is refused for exactly its own reason (§8.2); sender {sender}"
            );
            let status = fixture.post(sender, &envelope);
            assert_eq!(
                status, 200,
                "rule: the peer address hands {what} to the core, which drops it (§8.2)"
            );
            expected += 1;
            fixture.settle_at(expected, what);
        }
        assert_eq!(
            codec(1, instance(), fingerprint).open(id(2), &sealed),
            Ok(heartbeat(2, 1)),
            "rule: the matching envelope opens to its message at the same codec (§8.2)"
        );
        let status = fixture.post(2, &sealed);
        assert_eq!(status, 200, "rule: a matching envelope is accepted");
        fixture.settle_at(
            expected,
            "a matching envelope after the mismatches (control: not)",
        );
    }
}
