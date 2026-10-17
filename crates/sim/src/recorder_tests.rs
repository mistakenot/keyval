#![cfg(test)]
//! Tests for the node-side recorder (W36.3; PLAN.md §10.2, T-5, T-13, P10): a real run's
//! document carries `recovered`, `append`, `truncate` and `node_state` records that rebuild
//! each node's log and state as the cluster has them, and that satisfy the schema.

// check: SIM-RECORDER
use std::collections::BTreeMap;

use serde_json::Value;
use types::{NodeId, Term};

use crate::observe;
use crate::observe_tests::{document, run, run_limited, run_with};
use crate::recorder::diff;
use crate::scheduler::VirtualTime;

const SCHEMA: &str = include_str!("../../../schemas/observation/observation.v1.schema.json");

/// Seeds the run tests drive.
const SEEDS: u64 = 4;

fn records(document: &Value) -> &Vec<Value> {
    document["records"].as_array().unwrap()
}

/// A reported log's terms and the diff expected against the test log.
type Case<'a> = (&'a [Term], (Option<u64>, usize));

fn term(raw: u64) -> Term {
    Term::new(raw).unwrap()
}

fn entry(index: u64, term_raw: u64) -> types::Entry {
    types::Entry {
        index: types::EntryIndex::new(index).unwrap(),
        term: term(term_raw),
        payload: types::EntryPayload::Empty,
    }
}

#[test]
fn a_log_diff_names_the_dropped_suffix_and_the_new_entries() {
    let log = [entry(1, 1), entry(2, 1), entry(3, 2)];
    let cases: [Case; 5] = [
        (&[], (None, 0)),
        (&[term(1), term(1), term(2)], (None, 3)),
        (&[term(1), term(1)], (None, 2)),
        (&[term(1), term(1), term(1), term(1)], (Some(3), 2)),
        (&[term(2)], (Some(1), 0)),
    ];
    for (reported, (truncate, common)) in cases {
        let observed = diff(reported, &log);
        assert_eq!(
            (observed.0.map(types::EntryIndex::get), observed.1),
            (truncate, common),
            "rule: a reported log is cut at its first entry whose term differs, and the log \
             is new from there; reported={reported:?}"
        );
    }
}

#[test]
fn a_real_run_validates_and_starts_every_node_with_a_recovered_record() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    for seed in 0..SEEDS {
        let document = document(seed, 0);
        let errors: Vec<String> = validator
            .iter_errors(&document)
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "rule: a run's document is a valid keyval.observation/v1 (O-6); seed={seed}; \
             errors={errors:?}"
        );
        let mut first: BTreeMap<u64, String> = BTreeMap::new();
        for record in records(&document) {
            if let Some(node) = record["at"]["node"].as_u64() {
                first
                    .entry(node)
                    .or_insert_with(|| record["kind"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(
            document["voters"].as_array().map(Vec::len),
            Some(first.len()),
            "rule: every voter has node-side records; seed={seed}; first={first:?}"
        );
        assert!(
            first.values().all(|kind| kind == "recovered"),
            "rule: a node's records start with its recovered record (stream rule 6); \
             seed={seed}; first={first:?}"
        );
    }
}

#[test]
fn the_records_rebuild_each_nodes_final_log_and_state() {
    for seed in 0..SEEDS {
        let (clients, cluster, config, meta) = run(seed, 0);
        let document = observe::document(&cluster, &clients, &config, &meta).unwrap();
        let mut logs: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
        let mut states: BTreeMap<u64, &Value> = BTreeMap::new();
        for record in records(&document) {
            let Some(node) = record["at"]["node"].as_u64() else {
                continue;
            };
            let log = logs.entry(node).or_default();
            match record["kind"].as_str().unwrap() {
                "recovered" => {
                    *log = record["log"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|entry| {
                            (
                                entry["index"].as_u64().unwrap(),
                                entry["term"].as_u64().unwrap(),
                            )
                        })
                        .collect();
                }
                "append" => {
                    let index = record["index"].as_u64().unwrap();
                    assert_eq!(
                        index,
                        u64::try_from(log.len()).unwrap() + 1,
                        "rule: an append is one past the log's end (stream rule 5); seed={seed} \
                         node={node} record={record}"
                    );
                    log.push((index, record["term"].as_u64().unwrap()));
                }
                "truncate" => {
                    let from = usize::try_from(record["from_index"].as_u64().unwrap()).unwrap();
                    log.truncate(from - 1);
                }
                "node_state" => {
                    states.insert(node, record);
                }
                _ => {}
            }
        }
        for voter in config.voters() {
            let ctl = cluster.controller(voter).expect("every node is running");
            let now = ctl.status();
            let log = &logs[&voter.get()];
            assert_eq!(
                log.last().map_or(0, |entry| entry.0),
                now.last.get(),
                "rule: the recorded appends and truncations rebuild the node's log (T-5); \
                 seed={seed} node={}",
                voter.get()
            );
            let state = states[&voter.get()];
            assert_eq!(
                (
                    state["process"].as_str(),
                    state["role"].as_str(),
                    state["term"].as_u64(),
                    state["commit"].as_u64(),
                    state["applied"].as_u64(),
                    state["state_digest"].as_str().map(str::to_owned),
                ),
                (
                    Some("running"),
                    Some(now.role.as_str()),
                    Some(now.term.get()),
                    Some(now.commit.get()),
                    Some(ctl.state().applied_index().get()),
                    Some(ctl.state().digest().to_string()),
                ),
                "rule: a node's last node_state is its state at the end (T-5); seed={seed} \
                 node={}",
                voter.get()
            );
        }
    }
}

#[test]
fn the_same_seed_gives_the_same_node_side_records() {
    let kinds = |seed| {
        records(&document(seed, 0))
            .iter()
            .filter(|record| record["at"]["node"].is_u64())
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(2),
        kinds(2),
        "rule: recording draws no randomness, so a seed gives the same node-side records (P4)"
    );
}

#[test]
fn a_full_node_side_queue_makes_the_document_incomplete_not_wrong() {
    const BOUND: usize = 3;
    let (clients, cluster, config, meta) = run_limited(1, 0, Some(BOUND));
    let document = observe::document(&cluster, &clients, &config, &meta).unwrap();
    let node_side = records(&document)
        .iter()
        .filter(|record| record["at"]["node"].is_u64())
        .count();
    assert_eq!(
        (
            document["overflowed"].as_array().map(Vec::len),
            document["complete"].as_bool(),
            document["overflowed"][0]["bound"].as_u64(),
            node_side,
        ),
        (Some(1), Some(false), Some(3), BOUND),
        "rule: a node-side queue that fills keeps what it has and lists the overflow, so the \
         run is incomplete (P10); document={}",
        document["overflowed"]
    );
}

#[test]
fn a_restarted_node_starts_a_new_incarnation_from_what_its_disk_kept() {
    for seed in 0..SEEDS {
        let (clients, cluster, config, meta) = run_with(seed, 0, |cluster| {
            let node = NodeId::new(2).unwrap();
            cluster
                .crash_at(VirtualTime::from_micros(400_000), node)
                .unwrap();
            cluster
                .restart_at(VirtualTime::from_micros(900_000), node)
                .unwrap();
        });
        let document = observe::document(&cluster, &clients, &config, &meta).unwrap();
        let node_two: Vec<(&str, u64, &str)> = records(&document)
            .iter()
            .filter(|record| record["at"]["node"] == 2)
            .filter_map(|record| {
                let kind = record["kind"].as_str()?;
                let process = record["process"].as_str().unwrap_or("");
                ["recovered", "node_state"]
                    .contains(&kind)
                    .then(|| (kind, record["at"]["incarnation"].as_u64().unwrap(), process))
            })
            .collect();
        let recovered: Vec<u64> = node_two
            .iter()
            .filter(|(kind, _, _)| *kind == "recovered")
            .map(|(_, incarnation, _)| *incarnation)
            .collect();
        assert_eq!(
            recovered.len(),
            2,
            "rule: a node records one recovered record per incarnation (stream rule 6); \
             seed={seed}; node 2 = {node_two:?}"
        );
        assert!(
            node_two.contains(&("node_state", recovered[0], "crashed")),
            "rule: a crash is a node_state with process crashed, in the incarnation that \
             crashed; seed={seed}; node 2 = {node_two:?}"
        );
        assert!(
            config.voters().all(|voter| {
                let ctl = cluster.controller(voter);
                ctl.is_some()
            }),
            "rule: the restarted run finished with every node up; seed={seed}"
        );
    }
}
