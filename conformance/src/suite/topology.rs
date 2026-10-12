//! Topology and lifecycle tests (PLAN.md §10.8, W32.2): follower `307`, no-leader `503`,
//! and health during recovery. They run only against the real five-node cluster
//! (`CONFORMANCE_CLUSTER_URLS`), and the two that need a missing leader or a restarting node
//! create that precondition through `./dev cluster kill|restart` against the cluster named by
//! `CONFORMANCE_CLUSTER_CONFIG` (rule 18: no other way of touching a node). Every write a
//! test makes is driven to a terminal answer before the test relies on it.

#![cfg(test)]

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::support::{
    Node, assert_keys, assert_no_store, body, cluster_exclusive, cluster_shared, enc, error,
    fresh_id, fresh_prefix, index_of, ok, say,
};
use crate::http::{self, Request, Response};
use crate::spec::Spec;
use crate::tag::{Class, Tag, validate_all};
use crate::target::{Kind, Targets};

/// How long a test waits for the cluster to change state (election, restart).
const SETTLE: Duration = Duration::from_secs(60);
const PAUSE: Duration = Duration::from_millis(100);

const REDIRECT: Tag = Tag {
    test: "follower_write_is_redirected_to_the_leader",
    class: Class::Topology,
    operations: &["putKey", "deleteKey", "compareAndSwap", "getRequestStatus"],
    error_codes: &["not_leader"],
};

const NO_LEADER: Tag = Tag {
    test: "write_with_no_leader_is_unavailable_and_lists_the_nodes",
    class: Class::Topology,
    operations: &["putKey", "deleteKey", "compareAndSwap", "getKey"],
    error_codes: &["unavailable"],
};

const RECOVERY: Tag = Tag {
    test: "health_reports_a_restarting_node_until_it_is_ready",
    class: Class::Topology,
    operations: &[
        "getLiveness",
        "getReadiness",
        "getClusterStatus",
        "putKey",
        "getKey",
    ],
    error_codes: &["recovering"],
};

/// The real cluster and, when the environment names it, the config `./dev cluster` manages.
struct Cluster {
    nodes: Vec<(u64, Node)>,
    config: Option<String>,
}

impl Cluster {
    /// The cluster the environment names, or none (the tests then have nothing to run on).
    fn from_env(tag: &Tag) -> Option<Self> {
        let problems = validate_all(&[*tag], &spec());
        assert!(problems.is_empty(), "rule: a tag is sound; {problems:?}");
        let targets = Targets::from_lookup(|name| std::env::var(name).ok())
            .unwrap_or_else(|why| panic!("rule: the target environment is valid; {why}"));
        let urls = targets.selected(tag.class, tag.operations);
        if urls.is_empty() {
            assert!(
                std::env::var_os("CONFORMANCE_REQUIRE_CLUSTER").is_none(),
                "rule: CONFORMANCE_REQUIRE_CLUSTER is set, so topology test {} must run on a \
                 real cluster (P10), but CONFORMANCE_CLUSTER_URLS names none",
                tag.test
            );
            eprintln!(
                "topology test {}: no cluster named in CONFORMANCE_CLUSTER_URLS; nothing exercised",
                tag.test
            );
            return None;
        }
        let nodes = urls
            .iter()
            .zip(1..)
            .map(|((_, base), id)| (id, Node::at(Kind::ClusterNode, (*base).clone())))
            .collect();
        Some(Self {
            nodes,
            config: std::env::var("CONFORMANCE_CLUSTER_CONFIG").ok(),
        })
    }

    fn node(&self, id: u64) -> &Node {
        &self
            .nodes
            .iter()
            .find(|(n, _)| *n == id)
            .unwrap_or_else(|| panic!("rule: node {id} is one of the five"))
            .1
    }

    fn urls(&self) -> Vec<String> {
        self.nodes
            .iter()
            .map(|(_, n)| n.base().to_string())
            .collect()
    }

    /// `./dev cluster <action> --config C --node N --json`; failure fails the test.
    fn dev(&self, action: &str, id: u64) {
        let config = self.config.as_deref().unwrap_or_else(|| {
            panic!("rule: a topology test that kills nodes needs CONFORMANCE_CLUSTER_CONFIG (the config ./dev cluster manages)")
        });
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let output = Command::new(root.join("dev"))
            .current_dir(&root)
            .args(["cluster", action, "--config", config, "--node"])
            .arg(id.to_string())
            .arg("--json")
            .output()
            .unwrap_or_else(|why| panic!("rule: ./dev runs; {why}"));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        assert!(
            output.status.success() && report["ok"] == true,
            "rule: ./dev cluster {action} --node {id} succeeds; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// This node's `GET /v1/cluster`, or none while it does not answer.
    fn status(&self, id: u64) -> Option<Value> {
        let response =
            http::send(self.node(id).base(), &Request::new("GET", "/v1/cluster")).ok()?;
        (response.status == 200).then(|| body(&response))
    }

    /// Waits until every node in `alive` reports the same leader, who is one of them and
    /// calls itself leader; returns that leader's ID.
    fn stable_leader(&self, alive: &[u64]) -> u64 {
        eventually("every live node agrees on one leader", || {
            let mut leader = None;
            for id in alive {
                let status = self
                    .status(*id)
                    .ok_or("a node does not answer /v1/cluster")?;
                let seen = status["leader_id"]
                    .as_u64()
                    .ok_or("a node knows no leader")?;
                if *leader.get_or_insert(seen) != seen {
                    return Err("nodes name different leaders".to_owned());
                }
            }
            let leader = leader.ok_or("no node answered")?;
            let status = self.status(leader).ok_or("the leader does not answer")?;
            (status["role"] == "leader")
                .then_some(leader)
                .ok_or_else(|| {
                    format!(
                        "node {leader} is named leader but reports {}",
                        status["role"]
                    )
                })
        })
    }

    /// Waits until `id` answers `GET /v1/health/ready` with 200.
    fn wait_ready(&self, id: u64) {
        eventually(&format!("node {id} is ready"), || {
            let response = http::send(
                self.node(id).base(),
                &Request::new("GET", "/v1/health/ready"),
            )
            .map_err(|why| why.to_string())?;
            (response.status == 200)
                .then_some(())
                .ok_or_else(|| format!("status {}", response.status))
        });
    }
}

/// Restarts every node a test killed, whatever the test did, so the next test (and the
/// semantic runs) meet a whole cluster.
struct Restore<'a> {
    cluster: &'a Cluster,
    killed: Vec<u64>,
}

impl Restore<'_> {
    fn kill(&mut self, id: u64) {
        self.killed.push(id);
        self.cluster.dev("kill", id);
    }

    fn restore(&mut self) {
        // Every restart is attempted, so one failure leaves the other nodes up.
        let failed: Vec<u64> = std::mem::take(&mut self.killed)
            .into_iter()
            .filter(|id| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.cluster.dev("restart", *id);
                }))
                .is_err()
            })
            .collect();
        assert!(
            failed.is_empty(),
            "rule: every killed node restarts; ./dev cluster restart failed for {failed:?}"
        );
        let all: Vec<u64> = self.cluster.nodes.iter().map(|(id, _)| *id).collect();
        for id in &all {
            self.cluster.wait_ready(*id);
        }
        self.cluster.stable_leader(&all);
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if !self.killed.is_empty() && thread::panicking() {
            // A failed test still leaves the cluster whole; a failure here adds nothing.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.restore()));
        }
    }
}

fn spec() -> Spec {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/openapi.yaml");
    Spec::load(&path).unwrap_or_else(|why| panic!("rule: the spec loads; {why}"))
}

/// Calls `step` until it succeeds or [`SETTLE`] passes; running out fails the test (P10).
fn eventually<T>(what: &str, mut step: impl FnMut() -> Result<T, String>) -> T {
    let start = Instant::now();
    loop {
        match step() {
            Ok(value) => return value,
            Err(why) if start.elapsed() > SETTLE => {
                panic!("rule: within {SETTLE:?} {what}; last observation: {why}")
            }
            Err(_) => thread::sleep(PAUSE),
        }
    }
}

fn put_body(value: &str) -> String {
    json!({ "value": value }).to_string()
}

/// The three write shapes, as (method, target, body) for `key`.
fn write_shapes(key: &str) -> [(&'static str, String, Option<String>); 3] {
    let cas = json!({ "expected": { "kind": "absent" }, "value": "c" }).to_string();
    [
        (
            "PUT",
            format!("/v1/kv?key={}", enc(key)),
            Some(put_body("p")),
        ),
        ("DELETE", format!("/v1/kv?key={}", enc(key)), None),
        ("POST", format!("/v1/cas?key={}", enc(key)), Some(cas)),
    ]
}

fn refused(response: &Response, status: u16, code: &str, id: &str, why: &str) -> Value {
    let details = error(response, status, code, why);
    assert_no_store(response, why);
    assert_eq!(
        details["request_id"], id,
        "rule: a refusal echoes the request ID (§8.4); {why}; {details}"
    );
    assert_eq!(
        details["attempt_state"], "not_submitted",
        "rule: a refusal submitted nothing; {why}; {details}"
    );
    details
}

// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: getRequestStatus
// api-test: not_leader
#[test]
pub(super) fn follower_write_is_redirected_to_the_leader() {
    let Some(cluster) = Cluster::from_env(&REDIRECT) else {
        return;
    };
    let _healthy = cluster_shared();
    let all: Vec<u64> = cluster.nodes.iter().map(|(id, _)| *id).collect();
    let leader = cluster.stable_leader(&all);
    let leader_url = cluster.node(leader).base().to_string();
    for (id, follower) in cluster.nodes.iter().filter(|(id, _)| *id != leader) {
        let key = format!("{}k", fresh_prefix());
        for (method, target, payload) in write_shapes(&key) {
            let request_id = fresh_id();
            let why = format!("{method} to follower {id} (leader {leader})");
            let response =
                follower.write_unfollowed(method, &target, Some(&request_id), payload.as_deref());
            let details = refused(&response, 307, "not_leader", &request_id, &why);
            assert_eq!(
                response.header("Location"),
                Some(format!("{leader_url}{target}").as_str()),
                "rule: Location is the leader's URL for the same path and query (F-7); {why}"
            );
            assert_eq!(
                details["leader_hint"], leader,
                "rule: leader_hint names the leader; {why}; {details}"
            );
            assert_keys(
                &details,
                &["attempt_state", "leader_hint", "request_id"],
                "not_leader details",
            );
        }
        replay_is_answered_locally(*id, follower, &key);
    }
}

/// A follower that has applied a write answers its request ID itself, without redirecting
/// (F-7: the short-circuit precedes the leader check).
fn replay_is_answered_locally(id: u64, follower: &Node, key: &str) {
    let request_id = fresh_id();
    let target = format!("/v1/kv?key={}", enc(key));
    let payload = put_body("v");
    let first = ok(
        &follower.write("PUT", &target, Some(&request_id), Some(&payload)),
        "PUT followed to the leader",
    );
    eventually(&format!("follower {id} has applied the PUT"), || {
        let status = body(&follower.status(&request_id));
        (status["state"] == "known")
            .then_some(())
            .ok_or_else(|| status.to_string())
    });
    let again = ok(
        &follower.write_unfollowed("PUT", &target, Some(&request_id), Some(&payload)),
        &format!("the same PUT again on follower {id} is answered locally, not redirected"),
    );
    assert_eq!(again["replayed"], true, "rule: a replay says so; {again}");
    assert_eq!(
        index_of(&again),
        index_of(&first),
        "rule: the replay keeps the original index; {first} {again}"
    );
}

// api-test: putKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: getKey
// api-test: unavailable
#[test]
pub(super) fn write_with_no_leader_is_unavailable_and_lists_the_nodes() {
    let Some(cluster) = Cluster::from_env(&NO_LEADER) else {
        return;
    };
    let _alone = cluster_exclusive();
    let all: Vec<u64> = cluster.nodes.iter().map(|(id, _)| *id).collect();
    let leader = cluster.stable_leader(&all);
    let seed = format!("{}seed", fresh_prefix());
    let seeded = ok(
        &cluster.node(leader).put(&seed, "kept", &fresh_id()),
        "seeding PUT on the leader",
    );
    let victims: Vec<u64> = std::iter::once(leader)
        .chain(all.iter().copied().filter(|id| *id != leader).take(2))
        .collect();
    let survivors: Vec<u64> = all
        .iter()
        .copied()
        .filter(|id| !victims.contains(id))
        .collect();
    for id in &survivors {
        let read = cluster.node(*id).read_after(&seed, index_of(&seeded));
        assert_eq!(
            read.status,
            200,
            "rule: the seed reached node {id}; {}",
            say(&read)
        );
    }
    let mut restore = Restore {
        cluster: &cluster,
        killed: Vec::new(),
    };
    for id in &victims {
        restore.kill(*id);
    }
    for id in &survivors {
        unavailable_on(&cluster, *id);
        let read = cluster.node(*id).read(&seed);
        let value = ok(
            &read,
            &format!("a read on node {id} with no leader is served locally"),
        );
        assert_eq!(
            value["value"], "kept",
            "rule: reads need no leader (F-7); {value}"
        );
    }
    restore.restore();
}

/// Waits for survivor `id` to stop believing in a leader, then checks every write shape
/// answers `503 unavailable` with all five client addresses.
fn unavailable_on(cluster: &Cluster, id: u64) {
    let node = cluster.node(id);
    let key = format!("{}k", fresh_prefix());
    for (method, target, payload) in write_shapes(&key) {
        let request_id = fresh_id();
        let response = eventually(&format!("node {id} has no leader for {method}"), || {
            let response =
                node.write_unfollowed(method, &target, Some(&request_id), payload.as_deref());
            match response.status {
                503 => Ok(response),
                200 => panic!(
                    "rule: no quorum, no commit; node {id} acknowledged {method}: {}",
                    say(&response)
                ),
                other => Err(format!("status {other}")),
            }
        });
        let why = format!("{method} on node {id} with no leader");
        let details = refused(&response, 503, "unavailable", &request_id, &why);
        assert_eq!(
            details["nodes"],
            json!(cluster.urls()),
            "rule: unavailable lists every configured client address in node order (F-7); {why}; {details}"
        );
        assert_keys(
            &details,
            &["attempt_state", "nodes", "request_id"],
            "unavailable details",
        );
    }
}

// api-test: getLiveness
// api-test: getReadiness
// api-test: getClusterStatus
// api-test: putKey
// api-test: getKey
// api-test: recovering
#[test]
pub(super) fn health_reports_a_restarting_node_until_it_is_ready() {
    let Some(cluster) = Cluster::from_env(&RECOVERY) else {
        return;
    };
    let _alone = cluster_exclusive();
    let all: Vec<u64> = cluster.nodes.iter().map(|(id, _)| *id).collect();
    let leader = cluster.stable_leader(&all);
    let victim = all
        .iter()
        .copied()
        .find(|id| *id != leader)
        .unwrap_or(leader);
    let key = format!("{}durable", fresh_prefix());
    let written = ok(
        &cluster.node(leader).put(&key, "survives", &fresh_id()),
        "seeding PUT on the leader",
    );
    let wanted = index_of(&written);
    cluster.node(victim).read_after(&key, wanted);
    let mut restore = Restore {
        cluster: &cluster,
        killed: Vec::new(),
    };
    restore.kill(victim);
    cluster.dev("restart", victim);
    restore.killed.clear();
    watch_recovery(&cluster, victim);
    let read = ok(
        &cluster.node(victim).read_after(&key, wanted),
        "the restarted node serves the write it had before the crash",
    );
    assert_eq!(
        read["value"], "survives",
        "rule: committed data survives a crash; {read}"
    );
    let status = cluster.status(victim).unwrap_or(Value::Null);
    assert!(
        status["applied_index"].as_u64() >= Some(wanted.0) && status["node_id"] == victim,
        "rule: cluster status reports the restarted node caught up; {status}"
    );
    restore.restore();
}

/// Samples liveness, readiness and a write on `id` until it is ready, checking that every
/// answer on the way is one the spec allows (O-1, §7.7).
fn watch_recovery(cluster: &Cluster, id: u64) {
    let node = cluster.node(id);
    let mut not_ready = 0_u32;
    eventually(&format!("node {id} becomes ready"), || {
        let Ok(live) = http::send(node.base(), &Request::new("GET", "/v1/health/live")) else {
            not_ready += 1;
            return Err("not listening yet".to_owned());
        };
        let live_body = ok(&live, "liveness answers 200 in every lifecycle state (O-1)");
        assert_keys(
            &live_body,
            &["lifecycle", "node_id", "status"],
            "live health",
        );
        assert_eq!(
            live_body["node_id"], id,
            "rule: liveness names the node; {live_body}"
        );
        assert!(
            ["starting", "recovering", "ready"]
                .contains(&live_body["lifecycle"].as_str().unwrap_or("")),
            "rule: a restarting node is starting, recovering or ready, never failed; {live_body}"
        );
        let request_id = fresh_id();
        let attempt = node.write_unfollowed(
            "PUT",
            &format!("/v1/kv?key={}probe", fresh_prefix()),
            Some(&request_id),
            Some(&put_body("x")),
        );
        if attempt.status == 503 {
            let refusal = body(&attempt)["error"]["code"].clone();
            assert!(
                refusal == "recovering" || refusal == "unavailable",
                "rule: a write refused with 503 is recovering or unavailable; {}",
                say(&attempt)
            );
            if refusal == "recovering" {
                let details = refused(
                    &attempt,
                    503,
                    "recovering",
                    &request_id,
                    "write while not ready",
                );
                assert!(
                    ["starting", "recovering"]
                        .contains(&details["lifecycle"].as_str().unwrap_or("")),
                    "rule: recovering names a not-ready lifecycle; {details}"
                );
            }
        }
        let ready = http::send(node.base(), &Request::new("GET", "/v1/health/ready"))
            .map_err(|why| why.to_string())?;
        match ready.status {
            200 => {
                assert_eq!(
                    body(&ready)["lifecycle"],
                    "ready",
                    "rule: ready says ready; {}",
                    say(&ready)
                );
                Ok(())
            }
            503 => {
                not_ready += 1;
                let details = error(&ready, 503, "recovering", "readiness while recovering");
                assert_no_store(&ready, "readiness while recovering");
                assert!(
                    ["starting", "recovering"]
                        .contains(&details["lifecycle"].as_str().unwrap_or("")),
                    "rule: not-ready readiness names its lifecycle; {details}"
                );
                Err("still recovering".to_owned())
            }
            other => panic!(
                "rule: readiness is 200 or 503; got {other}: {}",
                say(&ready)
            ),
        }
    });
    // Whether recovery is observable depends on how fast the node recovers; say which it was.
    eprintln!("node {id}: {not_ready} not-ready sample(s) seen before it was ready");
}
