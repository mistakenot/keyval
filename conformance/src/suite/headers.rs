//! Response headers on every answer (§8, C-6), and the health and cluster shapes (O-1).

#![cfg(test)]

use super::support::{Node, assert_no_store, each_target, enc, fresh_id, fresh_prefix, ok, say};
use crate::http::Response;
use crate::tag::{Class, Tag};

const NO_STORE: Tag = Tag {
    test: "every_response_is_no_store_json",
    class: Class::Semantic,
    operations: &[
        "putKey",
        "getKey",
        "deleteKey",
        "compareAndSwap",
        "scanKeys",
        "getRequestStatus",
        "getLiveness",
        "getReadiness",
        "getClusterStatus",
    ],
    error_codes: &[
        "invalid_request",
        "not_found",
        "request_id_conflict",
        "version_mismatch",
        "too_large",
    ],
};

// api-test: putKey
// api-test: getKey
// api-test: deleteKey
// api-test: compareAndSwap
// api-test: scanKeys
// api-test: getRequestStatus
// api-test: getLiveness
// api-test: getReadiness
// api-test: getClusterStatus
// api-test: invalid_request
// api-test: not_found
// api-test: request_id_conflict
// api-test: version_mismatch
// api-test: too_large
#[test]
fn every_response_is_no_store_json() {
    each_target(&NO_STORE, |node| {
        for (label, response) in &battery(node) {
            assert_no_store(response, label);
            if ["unknown path", "wrong method"].contains(&label.as_str()) {
                // The envelope is required only where the server controls the response (§8.4).
                continue;
            }
            let content_type = response.header("Content-Type").unwrap_or_default();
            assert!(
                content_type.starts_with("application/json"),
                "rule: every response is JSON; case={label} content-type={content_type:?} {}",
                say(response)
            );
            assert!(
                response.json().is_ok(),
                "rule: every body parses as JSON; case={label} {}",
                say(response)
            );
        }
    });
}

/// One response of every kind the API gives, labelled.
fn battery(node: &Node) -> Vec<(String, Response)> {
    let prefix = fresh_prefix();
    let key = format!("{prefix}k");
    let ek = enc(&key);
    let id = fresh_id();
    let value = r#"{"value":"v"}"#;
    let mut battery: Vec<(String, Response)> = Vec::new();
    let mut run = |label: &str, response: Response| battery.push((label.to_owned(), response));
    run("GET absent key", node.read(&key));
    run(
        "PUT",
        node.write("PUT", &format!("/v1/kv?key={ek}"), Some(&id), Some(value)),
    );
    run(
        "PUT replay",
        node.write("PUT", &format!("/v1/kv?key={ek}"), Some(&id), Some(value)),
    );
    run(
        "PUT conflicting reuse",
        node.write(
            "PUT",
            &format!("/v1/kv?key={ek}"),
            Some(&id),
            Some(r#"{"value":"other"}"#),
        ),
    );
    run("GET present key", node.read(&key));
    run(
        "CAS mismatch",
        node.cas(
            &key,
            &serde_json::json!({"kind":"version","version":999_999}),
            "x",
            &fresh_id(),
        ),
    );
    run(
        "CAS expecting absent on a present key",
        node.cas(
            &key,
            &serde_json::json!({"kind":"absent"}),
            "x",
            &fresh_id(),
        ),
    );
    run("DELETE", node.delete(&key, &fresh_id()));
    run(
        "PUT without a request ID",
        node.write("PUT", &format!("/v1/kv?key={ek}"), None, Some(value)),
    );
    run(
        "PUT with a raw plus",
        node.write("PUT", "/v1/kv?key=a+b", Some(&fresh_id()), Some(value)),
    );
    run(
        "PUT with a bad body",
        node.write(
            "PUT",
            &format!("/v1/kv?key={ek}"),
            Some(&fresh_id()),
            Some("["),
        ),
    );
    run(
        "PUT too large",
        node.put(&key, &"a".repeat(65_537), &fresh_id()),
    );
    run("GET with a bad escape", node.get("/v1/kv?key=%zz"));
    run(
        "SCAN",
        node.get(&format!(
            "/v1/scan?{}",
            super::support::prefix_range(&prefix)
        )),
    );
    run("SCAN with a bad limit", node.get("/v1/scan?limit=0"));
    run("status known", node.status(&id));
    run("status unknown", node.status(&fresh_id()));
    run("status malformed", node.get("/v1/requests/nope"));
    run("liveness", node.get("/v1/health/live"));
    run("readiness", node.get("/v1/health/ready"));
    run("cluster", node.get("/v1/cluster"));
    run("unknown path", node.get("/v1/nope"));
    run(
        "wrong method",
        node.write("PATCH", &format!("/v1/kv?key={ek}"), None, None),
    );
    battery
}

const HEALTH: Tag = Tag {
    test: "health_and_cluster_report_their_required_fields",
    class: Class::Semantic,
    operations: &["getLiveness", "getReadiness", "getClusterStatus", "putKey"],
    error_codes: &[],
};

// api-test: getLiveness
// api-test: getReadiness
// api-test: getClusterStatus
// api-test: putKey
#[test]
fn health_and_cluster_report_their_required_fields() {
    each_target(&HEALTH, |node| {
        let live = ok(&node.get("/v1/health/live"), "liveness");
        assert!(
            live["node_id"].as_u64().is_some()
                && live["status"] == "ok"
                && live["lifecycle"].is_string(),
            "rule: liveness reports node_id, status ok and a lifecycle; body={live}"
        );
        let ready = ok(&node.get("/v1/health/ready"), "readiness of a serving node");
        assert_eq!(
            ready["lifecycle"], "ready",
            "rule: a 200 readiness is ready; body={ready}"
        );
        assert_eq!(
            ready["node_id"], live["node_id"],
            "rule: both name the same node"
        );
        let put = ok(
            &node.put(&format!("{}k", fresh_prefix()), "v", &fresh_id()),
            "PUT",
        );
        let cluster = ok(&node.get("/v1/cluster"), "cluster status");
        for field in [
            "node_id",
            "cluster_id",
            "role",
            "term",
            "leader_id",
            "durable_last_index",
            "commit_index",
            "applied_index",
            "peers",
        ] {
            assert!(
                cluster.get(field).is_some(),
                "rule: cluster status has {field}; body={cluster}"
            );
        }
        assert_eq!(
            cluster["node_id"], live["node_id"],
            "rule: the same node answers"
        );
        assert!(
            cluster["peers"]
                .as_array()
                .is_some_and(|peers| !peers.is_empty()),
            "rule: the voters are listed; body={cluster}"
        );
        let (applied, commit) = (
            cluster["applied_index"].as_u64().unwrap_or(0),
            cluster["commit_index"].as_u64().unwrap_or(0),
        );
        assert!(applied <= commit, "rule: applied <= commit; body={cluster}");
        assert!(
            applied >= put["served_by"]["applied_index"].as_u64().unwrap_or(0),
            "rule: the cluster view is no behind a write this node answered; body={cluster}"
        );
    });
}
