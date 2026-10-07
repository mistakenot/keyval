//! Five-process operation and write routing (PLAN.md F-7, §8.1, §8.4; W21.2) on real
//! `keyval-server` processes started from `config/local.toml`: five voters on distinct
//! client and peer addresses with their own data directories elect one leader and
//! replicate; a write to a follower is a `307` to the leader with the same decoded key
//! re-encoded and a typed `not_leader` body, and following it commits; with no quorum a
//! write is `503 unavailable` listing the configured nodes; reads are served locally.
//!
//! Real processes on loopback: test evidence of the wiring, not proof.
#![cfg(test)]

// dead_code fired: the shared support modules hold helpers this target does not call; they
// are shared with the other test targets, so trimming them per target is not an option
// (br-w21-2-five-node-routing-exn).
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

// dead_code fired: as above (br-w21-2-five-node-routing-exn).
#[allow(dead_code)]
#[path = "support/binary.rs"]
mod harness;

// dead_code fired: as above (br-w21-2-five-node-routing-exn).
#[allow(dead_code)]
#[path = "support/node.rs"]
mod node;

// check: CLUSTER-ROUTING

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::harness::init;
use crate::node::{Node, Spec, try_send};
use crate::support::{INSTANCE, Reply, free_port, put_body, request, scratch, send, write_config};

const LOCAL: &str = include_str!("../../../config/local.toml");

const VOTERS: u64 = 5;

struct Voter {
    id: u64,
    client: SocketAddr,
}

/// `config/local.toml` in `dir` with fresh loopback ports for every node and data
/// directories `node-<id>` under `dir`; everything else is the file's own text.
fn local_config(dir: &Path) -> (PathBuf, Vec<Voter>) {
    let mut text = LOCAL.to_owned();
    let mut voters = Vec::new();
    for id in 1..=VOTERS {
        let client: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let peer: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        for (old, new) in [
            (format!("127.0.0.1:700{id}"), client.to_string()),
            (format!("127.0.0.1:800{id}"), peer.to_string()),
            (format!("../.local/nodes/{id}"), format!("node-{id}")),
        ] {
            assert!(
                text.contains(&old),
                "fixture: config/local.toml names {old} for node {id}"
            );
            text = text.replace(&old, &new);
        }
        voters.push(Voter { id, client });
    }
    let (path, _) = write_config(dir, "local.toml", &text);
    (path, voters)
}

fn spec(dir: &Path, config: &Path, voter: &Voter) -> Spec {
    Spec {
        config: config.to_path_buf(),
        node: voter.id,
        cluster: "keyval-local-v1".to_owned(),
        address: voter.client,
        log: dir.join(format!("node-{}.log", voter.id)),
        dir: dir.to_path_buf(),
    }
}

/// Initialises the voters in `which`, then starts them.
fn start(dir: &Path, config: &Path, voters: &[Voter], which: &[u64]) -> Vec<Node> {
    which
        .iter()
        .map(|id| {
            let ran = init(config, &id.to_string(), INSTANCE, &["--json"]);
            assert_eq!(
                ran.code,
                Some(0),
                "fixture: node {id} initialises; stderr {}",
                ran.stderr
            );
            let voter = voters.iter().find(|voter| voter.id == *id).unwrap();
            Node::start(spec(dir, config, voter))
        })
        .collect()
}

fn stderrs(nodes: &[Node]) -> Vec<String> {
    nodes.iter().map(Node::stderr).collect()
}

/// `/v1/cluster`, or nothing while the node is not yet listening.
fn try_cluster(address: SocketAddr) -> Option<Reply> {
    try_send(address, &request("GET", "/v1/cluster", &[], "")).ok()
}

/// Waits until every voter reports the same leader and that leader reports itself leading;
/// returns its ID.
fn wait_for_one_leader(voters: &[Voter], nodes: &[Node]) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(40);
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
            "rule: five voters agree on one leader within 40 s (F-7, A-3); leaders seen \
             {leaders:?}; stderr: {:?}",
            stderrs(nodes)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn address_of(voters: &[Voter], id: u64) -> SocketAddr {
    voters.iter().find(|voter| voter.id == id).unwrap().client
}

fn request_id(n: u128) -> String {
    format!("{:08x}{:024x}", 7, n)
}

/// `PUT /v1/kv?key=<target_key>` with `id` and `value`; `target` is the raw request target.
fn put(address: SocketAddr, target: &str, id: &str, value: &str) -> Reply {
    send(
        address,
        &request(
            "PUT",
            target,
            &[
                ("Idempotency-Key", id),
                ("Content-Type", "application/json"),
            ],
            &put_body(value),
        ),
    )
}

/// The bytes a `Location`'s `key` query value decodes to (RFC 3986 percent-decoding).
fn decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
            out.push(u8::from_str_radix(pair, 16).unwrap());
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

/// Percent-encodes every byte but the RFC 3986 unreserved ones.
fn encode(key: &str) -> String {
    key.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Splits the one `Location` into where to send and what to send, after checking that it
/// names the leader's client address and the redirected request's own path and query.
fn follow(location: &[&str], leader: SocketAddr, target: &str) -> (SocketAddr, String) {
    let url = location.first().unwrap();
    let (authority, rest) = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .unwrap_or_else(|| panic!("rule: Location is an http URL with a path: {url}"));
    let followed = format!("/{rest}");
    assert_eq!(
        (authority.to_owned(), followed.clone()),
        (leader.to_string(), target.to_owned()),
        "rule: the Location names the leader's client address and the same path and query as \
         the redirected request (F-7, §8.4)"
    );
    (authority.parse().unwrap(), followed)
}

mod routing_tests {
    use super::*;

    #[test]
    fn five_real_processes_from_the_local_config_elect_one_leader_and_replicate() {
        let dir = scratch("cluster-five");
        let (config, voters) = local_config(&dir);
        let all: Vec<u64> = voters.iter().map(|voter| voter.id).collect();
        let nodes = start(&dir, &config, &voters, &all);
        let leader = wait_for_one_leader(&voters, &nodes);
        let written = put(
            address_of(&voters, leader),
            "/v1/kv?key=five",
            &request_id(1),
            "over-five",
        );
        assert_eq!(
            written.status,
            200,
            "rule: a write to the leader of five real processes commits (A-1); reply {:?}; \
             stderr {:?}",
            written.body,
            stderrs(&nodes)
        );
        let index = written.body["index"].as_u64().unwrap();
        for node in &nodes {
            node.wait_applied(index);
        }
        let report = send(
            address_of(&voters, leader),
            &request("GET", "/v1/cluster", &[], ""),
        );
        let matched: Vec<(u64, Option<u64>)> = report.body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|peer| (peer["id"].as_u64().unwrap(), peer["match_index"].as_u64()))
            .collect();
        assert!(
            matched.len() == usize::try_from(VOTERS).unwrap()
                && matched
                    .iter()
                    .all(|(_, matched)| matched.is_some_and(|matched| matched >= index)),
            "rule: the leader's match_index reaches the write's index {index} for all five \
             voters (O-1); matched {matched:?}"
        );
    }

    #[test]
    fn a_write_to_a_follower_is_a_307_to_the_leader_and_following_it_commits() {
        let dir = scratch("cluster-redirect");
        let (config, voters) = local_config(&dir);
        let all: Vec<u64> = voters.iter().map(|voter| voter.id).collect();
        let nodes = start(&dir, &config, &voters, &all);
        let leader = wait_for_one_leader(&voters, &nodes);
        let follower = voters.iter().find(|voter| voter.id != leader).unwrap();
        let key = "a b/é+&=%";
        let encoded = encode(key);
        let target = format!("/v1/kv?key={encoded}");
        let id = request_id(2);
        let redirected = put(follower.client, &target, &id, "via-follower");
        let location = redirected.header("location");
        let leader_address = address_of(&voters, leader);
        let location_key = location
            .first()
            .and_then(|url| url.strip_prefix(&format!("http://{leader_address}/v1/kv?key=")))
            .map(decode);
        assert_eq!(
            (
                redirected.status,
                redirected.code().map(str::to_owned),
                redirected.body["error"]["details"]["leader_hint"].as_u64(),
                redirected.body["error"]["details"]["request_id"]
                    .as_str()
                    .map(str::to_owned),
                redirected.body["error"]["details"]["attempt_state"]
                    .as_str()
                    .map(str::to_owned),
                location.len(),
                location_key,
            ),
            (
                307,
                Some("not_leader".to_owned()),
                Some(leader),
                Some(id.clone()),
                Some("not_submitted".to_owned()),
                1,
                Some(key.as_bytes().to_vec()),
            ),
            "rule: a write to follower {} is 307 not_leader naming leader {leader}, with one \
             Location at the leader's client address whose key decodes to the same bytes \
             (F-7, §8.1, §8.4); location {location:?}; stderr {:?}",
            follower.id,
            stderrs(&nodes)
        );
        // Follow the Location exactly: its authority is where the write goes, and its path
        // and query must be the request's own, so the same target as the redirected write.
        let (authority, followed_target) = follow(&location, leader_address, &target);
        let committed = put(authority, &followed_target, &id, "via-follower");
        assert_eq!(
            (committed.status, committed.body["replayed"].clone()),
            (200, serde_json::json!(false)),
            "rule: the same request ID and body sent to the Location commits (F-7, A-1); \
             reply {:?}",
            committed.body
        );
        let index = committed.body["index"].as_u64().unwrap();
        for node in &nodes {
            node.wait_applied(index);
        }
        let replay = put(follower.client, &target, &id, "via-follower");
        assert_eq!(
            (replay.status, replay.body["replayed"].clone()),
            (200, serde_json::json!(true)),
            "rule: a follower answers a request ID it has applied from its own history, \
             without redirecting (F-6, F-7 decision order); reply {:?}",
            replay.body
        );
        let read = send(
            follower.client,
            &request("GET", &format!("/v1/kv?key={encoded}"), &[], ""),
        );
        assert_eq!(
            (
                read.status,
                read.body["value"].clone(),
                read.body["served_by"]["node_id"].as_u64()
            ),
            (200, serde_json::json!("via-follower"), Some(follower.id)),
            "rule: a read at a follower is served locally, carrying that node as served_by \
             (F-7); reply {:?}",
            read.body
        );
    }

    #[test]
    fn with_no_quorum_a_write_is_503_unavailable_listing_the_configured_nodes() {
        let dir = scratch("cluster-leaderless");
        let (config, voters) = local_config(&dir);
        let nodes = start(&dir, &config, &voters, &[1, 2]);
        let deadline = Instant::now() + Duration::from_secs(30);
        let refused = loop {
            let reply = try_send(
                address_of(&voters, 1),
                &request(
                    "PUT",
                    "/v1/kv?key=nobody",
                    &[
                        ("Idempotency-Key", &request_id(3)),
                        ("Content-Type", "application/json"),
                    ],
                    &put_body("v"),
                ),
            );
            let settled = reply
                .as_ref()
                .is_ok_and(|reply| reply.code() != Some("recovering"));
            if settled && let Ok(reply) = reply {
                break reply;
            }
            assert!(
                Instant::now() < deadline,
                "fixture: node 1 answers something other than recovering within 30 s: \
                 {reply:?}; stderr {:?}",
                stderrs(&nodes)
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let listed: Vec<String> = refused.body["error"]["details"]["nodes"]
            .as_array()
            .map(|nodes| {
                nodes
                    .iter()
                    .map(|url| url.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let expected: Vec<String> = voters
            .iter()
            .map(|voter| format!("http://{}", voter.client))
            .collect();
        assert_eq!(
            (
                refused.status,
                refused.code().map(str::to_owned),
                listed,
                refused.header("location").len(),
                refused.body["error"]["details"]["attempt_state"]
                    .as_str()
                    .map(str::to_owned),
            ),
            (
                503,
                Some("unavailable".to_owned()),
                expected,
                0,
                Some("not_submitted".to_owned()),
            ),
            "rule: with two of five voters up there is no quorum and no leader, so a write is \
             503 unavailable listing all five configured nodes and no Location (F-7, §8.4); \
             reply {:?}; stderr {:?}",
            refused.body,
            stderrs(&nodes)
        );
    }

    #[test]
    fn after_the_leader_is_killed_the_other_four_elect_and_redirect_to_the_new_one() {
        let dir = scratch("cluster-failover");
        let (config, voters) = local_config(&dir);
        let all: Vec<u64> = voters.iter().map(|voter| voter.id).collect();
        let mut nodes = start(&dir, &config, &voters, &all);
        let first = wait_for_one_leader(&voters, &nodes);
        let written = put(
            address_of(&voters, first),
            "/v1/kv?key=before",
            &request_id(4),
            "v1",
        );
        assert_eq!(
            written.status, 200,
            "rule: the first leader commits a write (A-1); reply {:?}",
            written.body
        );
        let at = nodes
            .iter()
            .position(|node| node.spec.node == first)
            .unwrap();
        nodes[at]
            .kill()
            .unwrap_or_else(|why| panic!("fixture: the leader is killed: {why:?}"));
        let survivors: Vec<&Voter> = voters.iter().filter(|voter| voter.id != first).collect();
        let deadline = Instant::now() + Duration::from_secs(40);
        let second = loop {
            let seen: Vec<Option<u64>> = survivors
                .iter()
                .map(|voter| try_cluster(voter.client))
                .map(|report| report.and_then(|report| report.body["leader_id"].as_u64()))
                .collect();
            if let Some(Some(leader)) = seen.first().copied()
                && leader != first
                && seen.iter().all(|seen| *seen == Some(leader))
            {
                break leader;
            }
            assert!(
                Instant::now() < deadline,
                "rule: the four survivors elect a new leader within 40 s (A-3); leaders seen \
                 {seen:?}; stderr: {:?}",
                stderrs(&nodes)
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let follower = survivors.iter().find(|voter| voter.id != second).unwrap();
        let redirected = put(follower.client, "/v1/kv?key=after", &request_id(5), "v2");
        assert_eq!(
            (
                redirected.status,
                redirected.body["error"]["details"]["leader_hint"].as_u64(),
                redirected
                    .header("location")
                    .first()
                    .map(ToString::to_string),
            ),
            (
                307,
                Some(second),
                Some(format!(
                    "http://{}/v1/kv?key=after",
                    address_of(&voters, second)
                )),
            ),
            "rule: after leader {first} dies, a follower redirects to the new leader {second} \
             (F-7, A-3); reply {:?}",
            redirected.body
        );
        let committed = put(
            address_of(&voters, second),
            "/v1/kv?key=after",
            &request_id(5),
            "v2",
        );
        assert_eq!(
            committed.status, 200,
            "rule: the new leader commits with four voters (A-1); reply {:?}",
            committed.body
        );
    }
}
