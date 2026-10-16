#![cfg(test)]
//! Tests for [`crate::judge`]: the checkers K-1 to K-13 over a run's observation document.

// check: SIM-JUDGE
use checker::Verdict;
use serde_json::{Value, json};

use crate::judge::judge;

const RUN: &str = include_str!("../../../schemas/observation/fixtures/run.v1.json");
const DRAIN: &str = include_str!("../../../schemas/observation/fixtures/drain.v1.json");

const ALL: [&str; 13] = [
    "K-1", "K-2", "K-3", "K-4", "K-5", "K-6", "K-7", "K-8", "K-9", "K-10", "K-11", "K-12", "K-13",
];

fn doc(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture is JSON")
}

#[test]
fn a_clean_marker_run_passes_with_every_checker_run() {
    let judged = judge(&doc(RUN));
    assert_eq!(
        (judged.overall, judged.ran.as_slice(), judged.failures.len()),
        (Verdict::Pass, ALL.as_slice(), 0),
        "rule: a clean run passes with K-1 to K-13 run (T-13); judged={judged:#?}"
    );
}

#[test]
fn a_drain_run_that_reads_from_too_few_nodes_is_incomplete_not_failed() {
    let judged = judge(&doc(DRAIN));
    assert_eq!(
        (judged.overall, judged.ran.as_slice(), judged.failures.len()),
        (Verdict::Incomplete, ALL.as_slice(), 0),
        "rule: the drain fixture satisfies K-1 to K-12 but its reads come from no node, which \
         K-13 reports as a vacuous run (T-13); judged={judged:#?}"
    );
    assert!(
        judged
            .incomplete
            .iter()
            .any(|reason| reason.contains("ReadsFromTooFewNodes")),
        "rule: K-13 names the vacuity; judged={judged:#?}"
    );
}

#[test]
fn a_moved_committed_entry_fails_with_a_named_finding() {
    let mut run = doc(RUN);
    let mut changed = false;
    for record in run["records"].as_array_mut().unwrap() {
        if record["kind"] == "committed" && !changed {
            record["index"] = json!(record["index"].as_u64().unwrap() + 100);
            changed = true;
        }
    }
    assert!(changed, "fixture: the run has a committed record");
    let judged = judge(&run);
    assert_eq!(
        (judged.overall, judged.failures.is_empty()),
        (Verdict::Fail, false),
        "rule: moving a committed entry to another index fails a checker, and the failure \
         names its checker and rule (P9); judged={judged:#?}"
    );
}

#[test]
fn a_client_side_document_is_incomplete_and_runs_no_checker() {
    let mut run = doc(RUN);
    run["records"].as_array_mut().unwrap().retain(|record| {
        ["phase", "invoke", "response", "delivery", "resolution"]
            .contains(&record["kind"].as_str().unwrap_or_default())
    });
    let judged = judge(&run);
    assert_eq!(
        (judged.overall, judged.ran.len(), judged.failures.len()),
        (Verdict::Incomplete, 0, 0),
        "rule: without the node side nothing is judged and no violation is invented (P10); \
         judged={judged:#?}"
    );
    assert!(
        judged
            .incomplete
            .iter()
            .any(|reason| reason.contains("node-side")),
        "rule: the reason names the node side; judged={judged:#?}"
    );
}

#[test]
fn an_unreadable_document_is_incomplete_never_a_pass() {
    let judged = judge(&json!({ "records": [
        { "kind": "recovered" }, { "kind": "append" }, { "kind": "committed" },
        { "kind": "node_state" }, { "kind": "accept" }
    ] }));
    assert_eq!(
        (
            judged.overall,
            judged.ran.as_slice(),
            judged.incomplete.len()
        ),
        (Verdict::Incomplete, ["K-12"].as_slice(), 12),
        "rule: a document the checkers cannot read is incomplete, one reason per checker that \
         could not read it (P10); K-12 reads only records, so it alone ran; judged={judged:#?}"
    );
}
