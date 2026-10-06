//! The HTTP peer transport (PLAN.md §8.2, §7.11; W21.1) on real `keyval-server` processes:
//! three voters elect and replicate over `POST /raft` between their peer addresses, and a
//! node's peer address refuses what is not a well-formed envelope post before the core sees
//! it. The envelope's own identity checks and their counts are the codec's (PEER-ENVELOPE)
//! and W21.3's; the queue bounds are unit-tested in `src/peer.rs`.
//!
//! Real processes on loopback: test/simulation evidence of the wiring, not proof.
#![cfg(test)]

// dead_code fired: the shared support modules hold helpers this target does not call; they
// are shared with the other test targets, so trimming them per target is not an option
// (br-w21-1-http-peer-transport-g97).
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

// dead_code fired: as above (br-w21-1-http-peer-transport-g97).
#[allow(dead_code)]
#[path = "support/binary.rs"]
mod harness;

// dead_code fired: as above (br-w21-1-http-peer-transport-g97).
#[allow(dead_code)]
#[path = "support/node.rs"]
mod node;

// check: PEER-TRANSPORT

use std::fmt::Write as _;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use types::NodeId;

use crate::harness::init;
use crate::node::{Node, Spec, try_send};
use crate::support::{
    INSTANCE, Reply, free_port, one_voter, put_body, read_reply, request, scratch, send,
    write_config,
};

const THREE_VOTERS: &str = include_str!("../../../config/fixtures/three-voters.toml");

/// The most bytes an envelope may take (`raft_adapter::MAX_ENVELOPE_BYTES`).
const MAX_ENVELOPE_BYTES: u64 = raft_adapter::MAX_ENVELOPE_BYTES;

struct Voter {
    id: u64,
    client: SocketAddr,
}

/// A three-voter config in `dir`: the fixture's timing and limits, nodes 1 to 3 on fresh
/// loopback ports with data directories `node-<id>` under `dir`.
fn three_voters(dir: &Path) -> (PathBuf, Vec<Voter>) {
    let header = THREE_VOTERS
        .split("[[nodes]]")
        .next()
        .unwrap()
        .replace("keyval-fixture-three-voters", "keyval-peer-transport-test");
    let mut text = header;
    let mut voters = Vec::new();
    for id in 1..=3_u64 {
        let client: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let peer: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        write!(
            text,
            "[[nodes]]\nid = {id}\nclient_address = \"{client}\"\npeer_address = \"{peer}\"\n\
             data_dir = \"node-{id}\"\n\n"
        )
        .unwrap();
        voters.push(Voter { id, client });
    }
    let (path, _) = write_config(dir, "three-voters.toml", &text);
    (path, voters)
}

fn spec(dir: &Path, config: &Path, cluster: &str, voter: &Voter) -> Spec {
    Spec {
        config: config.to_path_buf(),
        node: voter.id,
        cluster: cluster.to_owned(),
        address: voter.client,
        log: dir.join(format!("node-{}.log", voter.id)),
        dir: dir.to_path_buf(),
    }
}

fn cluster(address: SocketAddr) -> Reply {
    send(address, &request("GET", "/v1/cluster", &[], ""))
}

/// `/v1/cluster`, or nothing while the node is not yet listening.
fn try_cluster(address: SocketAddr) -> Option<Reply> {
    try_send(address, &request("GET", "/v1/cluster", &[], "")).ok()
}

/// Waits until every voter reports the same leader, and that leader reports itself leading;
/// returns its ID.
fn wait_for_one_leader(voters: &[Voter], nodes: &[Node]) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reports: Vec<Reply> = voters
            .iter()
            .filter_map(|voter| try_cluster(voter.client))
            .collect();
        let leaders: Vec<Option<u64>> = reports
            .iter()
            .map(|report| report.body["leader_id"].as_u64())
            .collect();
        if reports.len() == voters.len()
            && let Some(Some(leader)) = leaders.first().copied()
            && leaders.iter().all(|seen| *seen == Some(leader))
            && reports.iter().any(|report| {
                report.body["role"] == "leader" && report.body["node_id"].as_u64() == Some(leader)
            })
        {
            return leader;
        }
        assert!(
            Instant::now() < deadline,
            "rule: three voters agree on one leader within 30 s over the peer transport (F-7, \
             A-3); leaders seen {leaders:?}; stderr: {:?}",
            nodes.iter().map(Node::stderr).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

mod transport_tests {
    use super::*;

    #[test]
    fn three_real_processes_elect_and_replicate_over_the_peer_transport() {
        let dir = scratch("peer-three");
        let (config, voters) = three_voters(&dir);
        for voter in &voters {
            let ran = init(&config, &voter.id.to_string(), INSTANCE, &["--json"]);
            assert_eq!(
                ran.code,
                Some(0),
                "fixture: node {} initialises; stderr {}",
                voter.id,
                ran.stderr
            );
        }
        let nodes: Vec<Node> = voters
            .iter()
            .map(|voter| Node::start(spec(&dir, &config, "keyval-peer-transport-test", voter)))
            .collect();
        let leader = wait_for_one_leader(&voters, &nodes);
        let leader_address = voters
            .iter()
            .find(|voter| voter.id == leader)
            .unwrap()
            .client;
        let id = format!("{:08x}{:024x}", 1, 1);
        let written = send(
            leader_address,
            &request(
                "PUT",
                "/v1/kv?key=peer",
                &[
                    ("Idempotency-Key", id.as_str()),
                    ("Content-Type", "application/json"),
                ],
                &put_body("over-http"),
            ),
        );
        assert_eq!(
            written.status,
            200,
            "rule: a write to the leader of three real processes commits through two followers' \
             acknowledgements, which only the peer transport carries (A-1); reply {:?}; stderr \
             {:?}",
            written.body,
            nodes.iter().map(Node::stderr).collect::<Vec<_>>()
        );
        let index = written.body["index"].as_u64().unwrap();
        for node in &nodes {
            node.wait_applied(index);
        }
        let report = cluster(leader_address);
        let matched: Vec<(u64, Option<u64>)> = report.body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|peer| (peer["id"].as_u64().unwrap(), peer["match_index"].as_u64()))
            .collect();
        assert!(
            matched
                .iter()
                .all(|(_, matched)| matched.is_some_and(|matched| matched >= index)),
            "rule: the leader's per-voter match_index reaches the write's index {index} for every \
             voter, so each follower stored it (O-1); matched {matched:?}"
        );
    }

    /// A `POST /raft` to `address` with `sender` (when given) and `body`.
    fn post_raft(address: SocketAddr, sender: Option<&str>, body: &[u8]) -> Reply {
        let mut head = String::from("POST /raft HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
        if let Some(sender) = sender {
            write!(head, "X-Keyval-Sender: {sender}\r\n").unwrap();
        }
        write!(head, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        send(address, &raw)
    }

    #[test]
    fn a_peer_address_takes_only_enveloped_posts_naming_a_sender() {
        let dir = scratch("peer-requests");
        let client = free_port();
        let (config, parsed) = one_voter(&dir, |text| {
            text.replace("127.0.0.1:7101", &format!("127.0.0.1:{client}"))
        });
        let ran = init(&config, "1", INSTANCE, &["--json"]);
        assert_eq!(ran.code, Some(0), "fixture: init; stderr {}", ran.stderr);
        let address: SocketAddr = format!("127.0.0.1:{client}").parse().unwrap();
        let peer_address = parsed.node(NodeId::new(1).unwrap()).unwrap().peer_address();
        let node = Node::start(Spec {
            config,
            node: 1,
            cluster: parsed.cluster_id().to_string(),
            address,
            log: dir.join("node-1.log"),
            dir: dir.clone(),
        });
        node.wait_leading();
        let junk = post_raft(peer_address, Some("2"), b"not an envelope");
        let no_sender = post_raft(peer_address, None, b"x");
        let word_sender = post_raft(peer_address, Some("two"), b"x");
        let zero_sender = post_raft(peer_address, Some("0"), b"x");
        let wrong_method = send(peer_address, &request("GET", "/raft", &[], ""));
        let wrong_path = send(peer_address, &request("POST", "/v1/kv", &[], "x"));
        let oversized = {
            // Only the head: the node answers 413 without reading a body that large.
            let mut stream = TcpStream::connect(peer_address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            write!(
                stream,
                "POST /raft HTTP/1.1\r\nHost: test\r\nX-Keyval-Sender: 2\r\nContent-Length: {}\r\n\r\n",
                MAX_ENVELOPE_BYTES + 1
            )
            .unwrap();
            stream.flush().unwrap();
            read_reply(&mut BufReader::new(stream))
        };
        let at_limit_is_read = post_raft(
            peer_address,
            Some("2"),
            &vec![0; usize::try_from(MAX_ENVELOPE_BYTES).unwrap()],
        );
        let ready = send(address, &request("GET", "/v1/health/ready", &[], ""));
        assert_eq!(
            (
                (junk.status, junk.body["accepted"].clone()),
                no_sender.status,
                word_sender.status,
                zero_sender.status,
                (wrong_method.status, wrong_method.header("allow")),
                wrong_path.status,
                oversized.status,
                at_limit_is_read.status,
                ready.status,
            ),
            (
                (200, serde_json::json!(true)),
                400,
                400,
                400,
                (405, vec!["POST"]),
                404,
                413,
                200,
                200,
            ),
            "rule: the peer address hands a well-formed POST /raft naming a sender to the core \
             (whose codec then drops what is not an envelope), refuses a missing, non-numeric or \
             zero sender (400), other methods (405) and paths (404), and a body past the envelope \
             limit {MAX_ENVELOPE_BYTES} (413), while one at the limit is read; the node stays \
             ready (§8.2, §7.11)"
        );
    }
}
