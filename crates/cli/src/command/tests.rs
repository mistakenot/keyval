//! The commands' output over a scripted network, a virtual clock and scripted entropy
//! (W33.3): every rendered document validates against its versioned schema, the exit status
//! tells terminal from unresolved, and `resume` replays the pending record and never makes a
//! new ID.

#![cfg(test)]

use jsonschema::Draft;
use serde_json::{Value as Json, json};
use types::{ExpectedVersion, Key, PageSize, RequestId, ScanRange, Version};

use super::*;
use crate::client::http::TransportError;
use crate::client::tests::{
    ASK, RESEND, Rig, error, key, node, put_ok, rig, served, settings, value,
};
use crate::pending::Method;

const WRITE: &str = include_str!("../../../../schemas/cli/write.v1.schema.json");
const GET: &str = include_str!("../../../../schemas/cli/get.v1.schema.json");
const SCAN: &str = include_str!("../../../../schemas/cli/scan.v1.schema.json");
const STATUS: &str = include_str!("../../../../schemas/cli/status.v1.schema.json");
const CLUSTER: &str = include_str!("../../../../schemas/cli/cluster.v1.schema.json");
const PENDING: &str = include_str!("../../../../schemas/cli/pending-list.v1.schema.json");

/// Asserts that `rendered.json` validates against `schema` and names `schema_id`.
fn valid(schema: &str, schema_id: &str, rendered: &Rendered) {
    let schema: Json = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&rendered.json)
        .map(|err| format!("{err} at {}", err.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "rule: every command output validates against its schema (§8.7); errors={errors:?}; \
         document={}",
        rendered.json
    );
    assert_eq!(rendered.json["schema"], schema_id, "the schema identifier");
}

fn busy() -> Json {
    error(
        "busy",
        "slow down",
        &json!({"request_id": "x", "attempt_state": "not_submitted"}),
    )
}

fn unknown_outcome() -> Json {
    error(
        "unknown_outcome",
        "not observed",
        &json!({"request_id": "x", "attempt_state": "submitted",
            "guidance": "retry_same_request_or_query_status", "nodes": []}),
    )
}

fn pending_ids(r: &Rig) -> Vec<RequestId> {
    r.store
        .list()
        .unwrap()
        .iter()
        .map(|record| record.request_id)
        .collect()
}

/// Renders a `put` under a rig whose script is already set.
fn put(r: &mut Rig) -> (Rendered, RequestId) {
    let (report, id) = r.client.put(&key("k"), &value("v")).unwrap();
    (render_write("put", id, &report), id)
}

// check: CLI-COMMANDS
#[test]
fn an_applied_put_validates_and_exits_zero() {
    let mut r = rig("c-put", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(200, &put_ok("x", 9, false));
    let (out, id) = put(&mut r);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.exit, 0, "rule: success exits 0; out={out:?}");
    assert_eq!(out.json["result"], "applied");
    assert_eq!(out.json["request_id"], id.to_string());
    assert_eq!(out.json["http_status"], 200);
    assert_eq!(
        out.json["server"]["index"], 9,
        "the server body is verbatim"
    );
    assert_eq!(out.json["pending_record_left"], Json::Null);
    assert!(out.human.contains("executed"), "human={}", out.human);
    assert_eq!(pending_ids(&r), vec![], "terminal result: record deleted");
}

#[test]
fn a_version_mismatch_is_terminal_and_not_unresolved() {
    let mut r = rig("c-cas", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        409,
        &error(
            "version_mismatch",
            "expected 41, current 57",
            &json!({"current_version": 57, "request_id": "x", "index": 58, "replayed": false}),
        ),
    );
    let (report, id) = r
        .client
        .cas(
            &key("k"),
            ExpectedVersion::Version(Version::new(41).unwrap()),
            &value("v"),
        )
        .unwrap();
    let out = render_write("cas", id, &report);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.json["result"], "version_mismatch");
    assert_eq!(out.exit, EXIT_TERMINAL, "out={out:?}");
    assert_eq!(out.json["unresolved"], Json::Null);
    assert_eq!(out.json["resume"], Json::Null, "nothing to resume");
    assert_eq!(
        pending_ids(&r),
        vec![],
        "a terminal result deletes the record"
    );
}

#[test]
fn capacity_exceeded_is_terminal_and_not_unresolved() {
    let mut r = rig("c-cap", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        507,
        &error(
            "capacity_exceeded",
            "full",
            &json!({"request_id": "x", "limit": "max_live_keys", "index": 7, "replayed": false}),
        ),
    );
    let (out, _) = put(&mut r);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.json["result"], "capacity_exceeded");
    assert_eq!(out.exit, EXIT_TERMINAL);
    assert_eq!(pending_ids(&r), vec![]);
}

#[test]
fn a_request_id_conflict_is_terminal_and_not_unresolved() {
    let mut r = rig("c-conflict", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        409,
        &error(
            "request_id_conflict",
            "different command",
            &json!({"request_id": "x", "original_index": 3}),
        ),
    );
    let (out, _) = put(&mut r);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.json["result"], "request_id_conflict");
    assert_eq!(out.exit, EXIT_TERMINAL);
    assert_eq!(out.json["unresolved"], Json::Null);
}

#[test]
fn a_rejected_request_carries_the_servers_code() {
    let mut r = rig("c-reject", settings(10, 5, 15_000), &[], RESEND);
    r.net
        .reply(400, &error("invalid_request", "bad key", &json!({})));
    let (out, _) = put(&mut r);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.json["result"], "rejected");
    assert_eq!(out.json["rejection"]["code"], "invalid_request");
    assert_eq!(out.json["rejection"]["http_status"], 400);
    assert_eq!(out.exit, EXIT_TERMINAL);
}

#[test]
fn history_full_is_refused_and_says_it_never_ran() {
    let mut r = rig("c-full", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        507,
        &error(
            "history_full",
            "no room",
            &json!({"request_id": "x", "limit": "max_log_entries",
                "attempt_state": "not_submitted"}),
        ),
    );
    let (out, id) = put(&mut r);
    valid(WRITE, WRITE_SCHEMA, &out);
    assert_eq!(out.json["result"], "refused");
    assert_eq!(out.exit, EXIT_TERMINAL);
    assert_eq!(out.json["resume"], format!("keyval resume {id}"));
    assert!(out.human.contains("never submitted"), "human={}", out.human);
}

/// Asserts the properties every unresolved write shares, and returns its bound name.
fn unresolved_write(r: &Rig, out: &Rendered, id: RequestId) -> String {
    valid(WRITE, WRITE_SCHEMA, out);
    assert_eq!(out.json["result"], "unresolved", "out={out:?}");
    assert_eq!(
        out.exit, EXIT_UNRESOLVED,
        "rule: exhausted bounds are unresolved (exit 3), never failed (§8.7)"
    );
    assert_eq!(out.json["request_id"], id.to_string());
    assert_eq!(
        out.json["resume"],
        format!("keyval resume {id}"),
        "rule: the output says how to resume"
    );
    for text in [
        out.human.to_lowercase(),
        out.json.to_string().to_lowercase(),
    ] {
        assert!(
            !text.contains("failed"),
            "rule: an unresolved write is never called failed (§8.7); text={text}"
        );
    }
    assert!(out.human.contains("UNRESOLVED"), "human={}", out.human);
    assert!(out.human.contains(&id.to_string()), "human={}", out.human);
    assert_eq!(
        pending_ids(r),
        vec![id],
        "rule: the record stays, with the same ID (O-4)"
    );
    out.json["unresolved"]["bound"].as_str().unwrap().to_owned()
}

#[test]
fn the_attempt_bound_ends_unresolved_with_a_resume_command() {
    let mut r = rig("c-attempts", settings(10, 5, 60_000), &[], RESEND);
    for _ in 0..10 {
        r.net.reply(429, &busy());
    }
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "attempts");
    assert_eq!(out.json["unresolved"]["sends"], 10);
}

#[test]
fn the_deadline_bound_ends_unresolved_with_a_resume_command() {
    let mut r = rig("c-deadline", settings(10, 5, 400), &[], RESEND);
    for _ in 0..10 {
        r.net.reply(504, &unknown_outcome());
    }
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "deadline");
}

#[test]
fn the_node_bound_ends_unresolved_with_a_resume_command() {
    // Two nodes allowed: node 1, then a redirect to 4, then a redirect to a third node.
    let mut r = rig("c-nodes", settings(10, 2, 60_000), &[], RESEND);
    for hint in [4, 5] {
        r.net.reply(
            307,
            &error(
                "not_leader",
                "elsewhere",
                &json!({"leader_hint": hint, "request_id": "x",
                    "attempt_state": "not_submitted"}),
            ),
        );
    }
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "nodes");
    assert_eq!(
        out.json["unresolved"]["nodes_tried"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "rule: no send goes to a third node (§7.11); out={out:?}"
    );
}

#[test]
fn resume_replays_the_same_id_and_bytes_after_an_unknown_status() {
    let mut r = rig("c-resume", settings(2, 5, 60_000), &[ASK, ASK], RESEND);
    // The write is answered "unknown", the status lookup says unknown, then the bounds end.
    r.net.reply(504, &unknown_outcome());
    r.net
        .reply(200, &json!({"state": "unknown", "served_by": served(1, 2)}));
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "attempts");
    let first = r.net.sends()[0].1.clone();

    r.net.reply(200, &put_ok("x", 8, true));
    let resumed = resume(&mut r.client, &r.store, id).unwrap();
    valid(WRITE, WRITE_SCHEMA, &resumed);
    assert_eq!(resumed.json["command"], "resume");
    assert_eq!(resumed.json["request_id"], id.to_string());
    assert_eq!(resumed.json["result"], "applied");
    assert_eq!(resumed.exit, 0);
    let last = r.net.sends().last().unwrap().1.clone();
    assert_eq!(
        last, first,
        "rule: resume sends the same ID and the same bytes (C-6, O-4)"
    );
    assert_eq!(
        pending_ids(&r),
        vec![],
        "a terminal result deletes the record"
    );
}

#[test]
fn resume_keeps_its_id_across_a_redirect_and_a_disconnect() {
    let mut r = rig("c-resume-net", settings(3, 5, 60_000), &[], RESEND);
    for _ in 0..3 {
        r.net.reply(429, &busy());
    }
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "attempts");
    let first = r.net.sends()[0].1.clone();

    r.net
        .then(Err(TransportError::Unknown("connection reset".to_owned())));
    r.net.reply(
        307,
        &error(
            "not_leader",
            "node 4 leads",
            &json!({"leader_hint": 4, "request_id": "x", "attempt_state": "not_submitted"}),
        ),
    );
    r.net.reply(200, &put_ok("x", 12, false));
    let resumed = resume(&mut r.client, &r.store, id).unwrap();
    valid(WRITE, WRITE_SCHEMA, &resumed);
    assert_eq!(resumed.json["result"], "applied", "out={resumed:?}");
    let sends = r.net.sends();
    assert_eq!(sends.len(), 6, "3 + 3 sends; sends={sends:?}");
    for (at, (_, request)) in sends.iter().enumerate() {
        assert_eq!(
            request, &first,
            "rule: every send, resumed or not, is the same ID and bytes (C-6); send {at}"
        );
    }
    assert_eq!(pending_ids(&r), vec![]);
}

#[test]
fn resume_that_exhausts_again_stays_pending_and_allocates_no_new_id() {
    let mut r = rig("c-resume-again", settings(2, 5, 60_000), &[], RESEND);
    for _ in 0..4 {
        r.net.reply(429, &busy());
    }
    let (out, id) = put(&mut r);
    assert_eq!(unresolved_write(&r, &out, id), "attempts");
    let again = resume(&mut r.client, &r.store, id).unwrap();
    assert_eq!(unresolved_write(&r, &again, id), "attempts");
    assert_eq!(again.json["command"], "resume");
    assert_eq!(
        pending_ids(&r),
        vec![id],
        "rule: exhaustion never allocates a replacement ID (§8.7); one record, the original"
    );
    let ids: std::collections::BTreeSet<_> = r
        .net
        .sends()
        .iter()
        .flat_map(|(_, request)| request.headers.clone())
        .filter(|(name, _)| *name == "Idempotency-Key")
        .map(|(_, id)| id)
        .collect();
    assert_eq!(ids.len(), 1, "every send used one ID; ids={ids:?}");
}

#[test]
fn resume_of_an_id_that_is_not_pending_is_an_error_not_a_new_write() {
    let mut r = rig("c-resume-none", settings(10, 5, 15_000), &[], RESEND);
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let Err(CommandError::Pending(why)) = resume(&mut r.client, &r.store, id) else {
        panic!("rule: resume of an unknown ID is refused");
    };
    assert!(why.contains(&id.to_string()), "why={why}");
    assert_eq!(r.net.sends(), vec![], "nothing is sent for it");
}

#[test]
fn get_reports_found_and_not_found() {
    let mut r = rig("c-get", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        200,
        &json!({"key": "a", "value": "hi", "version": 3, "served_by": served(1, 9)}),
    );
    r.net.reply(
        404,
        &error(
            "not_found",
            "no such key",
            &json!({"served_by": served(1, 9)}),
        ),
    );
    let found = render_get("a", &r.client.get(&key("a")).unwrap());
    valid(GET, GET_SCHEMA, &found);
    assert_eq!(
        (found.json["result"].clone(), found.exit),
        (json!("found"), 0)
    );
    assert_eq!(found.json["value"], "hi");
    assert_eq!(found.json["version"], 3);
    let missing = render_get("b", &r.client.get(&key("b")).unwrap());
    valid(GET, GET_SCHEMA, &missing);
    assert_eq!(missing.json["result"], "not_found");
    assert_eq!(missing.exit, EXIT_TERMINAL);
}

#[test]
fn an_unanswered_read_is_unresolved_and_says_to_run_it_again() {
    let mut r = rig("c-get-unres", settings(2, 5, 60_000), &[], RESEND);
    for _ in 0..2 {
        r.net.reply(429, &busy());
    }
    let out = render_get("a", &r.client.get(&key("a")).unwrap());
    valid(GET, GET_SCHEMA, &out);
    assert_eq!(out.json["result"], "unresolved");
    assert_eq!(out.exit, EXIT_UNRESOLVED);
    assert!(out.human.contains("run it again"), "human={}", out.human);
}

#[test]
fn scan_reports_a_page() {
    let mut r = rig("c-scan", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(
        200,
        &json!({"items": [
            {"key": "b", "value": "1", "version": 3},
            {"key": "c", "value": "2", "version": 4}],
            "has_more": true, "next_after": "c", "served_by": served(1, 9)}),
    );
    let range = ScanRange::new(
        Some(key("a")),
        Some(key("z")),
        None,
        PageSize::new(2).unwrap(),
    )
    .unwrap();
    let out = render_scan(&r.client.scan(&range).unwrap());
    valid(SCAN, SCAN_SCHEMA, &out);
    assert_eq!(out.json["result"], "page");
    assert_eq!(out.json["count"], 2);
    assert_eq!(out.json["has_more"], true);
    assert_eq!(out.json["next_after"], "c");
    assert!(
        out.human.contains("more: pass --after \"c\""),
        "rule: text mode names the continuation; human={}",
        out.human
    );
    assert_eq!(
        out.json["server"]["next_after"], "c",
        "the body is verbatim"
    );
    assert_eq!(out.human, "b\t3\t1\nc\t4\t2\nmore: pass --after \"c\"\n");
}

#[test]
fn status_known_exits_zero_and_unknown_is_unresolved_not_failed() {
    let mut r = rig("c-status", settings(10, 2, 15_000), &[], RESEND);
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    r.net.reply(
        200,
        &json!({"state": "known", "result": put_ok("x", 8, true), "served_by": served(1, 9)}),
    );
    let known = render_status(&r.client.status(id).unwrap());
    valid(STATUS, STATUS_SCHEMA, &known);
    assert_eq!(known.json["state"], "known");
    assert_eq!(known.json["result"], "applied");
    assert_eq!(known.exit, 0);

    for n in 1..=2 {
        r.net
            .reply(200, &json!({"state": "unknown", "served_by": served(n, 2)}));
    }
    let unknown = render_status(&r.client.status(id).unwrap());
    valid(STATUS, STATUS_SCHEMA, &unknown);
    assert_eq!(unknown.json["state"], "unknown");
    assert_eq!(unknown.json["unknown_on"], json!([1, 2]));
    assert_eq!(unknown.json["resume"], format!("keyval resume {id}"));
    assert_eq!(unknown.exit, EXIT_UNRESOLVED);
    assert!(
        !unknown.human.contains("failed"),
        "rule: unknown is not failed (P8); human={}",
        unknown.human
    );
}

#[test]
fn cluster_reports_the_answering_node() {
    let mut r = rig("c-cluster", settings(10, 5, 15_000), &[], RESEND);
    r.net.reply(200, &json!({"node_id": 1, "role": "leader"}));
    let out = render_cluster(&r.client.cluster().unwrap());
    valid(CLUSTER, CLUSTER_SCHEMA, &out);
    assert_eq!(out.json["node"], 1);
    assert_eq!(out.json["server"]["role"], "leader");
    assert_eq!(out.json["result"], "answered");
    assert_eq!(out.exit, 0);
    assert_eq!(node(1).get(), 1);
}

#[test]
fn cluster_with_no_answering_node_is_unresolved_and_schema_valid() {
    let mut r = rig("c-cluster-none", settings(10, 2, 15_000), &[], RESEND);
    // A 200 whose body is not an object is not an answer.
    r.net.reply(200, &json!([1, 2]));
    r.net
        .then(Err(TransportError::NotSent("refused".to_owned())));
    let out = render_cluster(&r.client.cluster().unwrap());
    valid(CLUSTER, CLUSTER_SCHEMA, &out);
    assert_eq!(out.json["result"], "unresolved", "doc={}", out.json);
    assert_eq!(out.json["node"], Json::Null);
    assert_eq!(out.json["server"], Json::Null);
    assert_eq!(
        out.exit, EXIT_UNRESOLVED,
        "rule: no answer is unresolved, never success"
    );
    assert!(
        out.json["notes"].to_string().contains("not a JSON object"),
        "doc={}",
        out.json
    );
}

#[test]
fn status_that_reached_no_node_says_so_and_is_still_unresolved() {
    let mut r = rig("c-status-none", settings(10, 2, 15_000), &[], RESEND);
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    for _ in 0..2 {
        r.net
            .then(Err(TransportError::NotSent("refused".to_owned())));
    }
    let out = render_status(&r.client.status(id).unwrap());
    valid(STATUS, STATUS_SCHEMA, &out);
    assert_eq!(out.json["unknown_on"], json!([]));
    assert_eq!(out.exit, EXIT_UNRESOLVED);
    assert!(
        out.human.contains("no node could be reached"),
        "rule: not 'no node asked has applied' when none was asked; human={}",
        out.human
    );
}

#[test]
fn a_persisted_write_that_cannot_send_names_its_id_and_keeps_its_record() {
    let mut empty = settings(10, 5, 15_000);
    empty.nodes.clear();
    let mut r = rig("c-nonodes", empty, &[], RESEND);
    let err = r.client.put(&key("k"), &value("v")).unwrap_err();
    let records = r.store.list().unwrap();
    assert_eq!(records.len(), 1, "rule: the record stays (O-4); err={err}");
    let id = records[0].request_id;
    assert!(
        err.to_string().contains(&format!("keyval resume {id}")),
        "rule: the error names the ID and how to resume; err={err}"
    );
}

#[test]
fn pending_lists_every_unresolved_write_and_how_to_resume_it() {
    let mut r = rig("c-pending", settings(1, 5, 60_000), &[], RESEND);
    let empty = render_pending(&r.store, &r.store.list().unwrap());
    valid(PENDING, PENDING_SCHEMA, &empty);
    assert_eq!(empty.json["count"], 0);
    assert!(empty.human.contains("no pending"), "human={}", empty.human);

    r.net.reply(429, &busy());
    r.net.reply(429, &busy());
    let (_, first) = put(&mut r);
    let (report, second) = r.client.delete(&key("gone")).unwrap();
    assert!(matches!(report.outcome, Outcome::Unresolved(_)));
    let out = render_pending(&r.store, &r.store.list().unwrap());
    valid(PENDING, PENDING_SCHEMA, &out);
    assert_eq!(out.json["count"], 2);
    let listed: Vec<String> = out.json["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["request_id"].as_str().unwrap().to_owned())
        .collect();
    for id in [first, second] {
        assert!(
            listed.contains(&id.to_string()),
            "id={id}; listed={listed:?}"
        );
        assert!(
            out.human.contains(&format!("keyval resume {id}")),
            "human={}",
            out.human
        );
    }
}

#[test]
fn a_record_with_a_control_character_in_its_target_is_refused() {
    let r = rig("c-crlf", settings(10, 5, 15_000), &[], RESEND);
    let id = RequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
    for target in [
        "/v1/kv?key=a\r\nX-Evil: 1",
        "no-slash",
        "/v1/kv?key=a b",
        "/v1/\u{e9}",
    ] {
        let record = Record {
            request_id: id,
            method: Method::Put,
            target: target.to_owned(),
            body: "{}".to_owned(),
        };
        assert!(
            r.store.persist(&record).is_err(),
            "rule: a target that is not a bare path-and-query is not stored; target={target:?}"
        );
    }
    assert_eq!(pending_ids(&r), vec![]);
}

#[test]
fn the_schemas_name_exactly_the_results_the_commands_produce() {
    let schema: Json = serde_json::from_str(WRITE).unwrap();
    let mut names: Vec<String> = schema["properties"]["result"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    names.sort();
    let mut expected: Vec<String> = [
        "applied",
        "version_mismatch",
        "capacity_exceeded",
        "request_id_conflict",
        "rejected",
        "refused",
        "unresolved",
        "unexpected",
    ]
    .map(str::to_owned)
    .to_vec();
    expected.sort();
    assert_eq!(
        names, expected,
        "rule: the schema enum is the renderer's names"
    );
    let _: Key = key("k");
}
