//! Tests of the journal-exhaustion scenario (W43.3; PLAN.md §6 DM-7, §9.4, D-61): a node at
//! its local journal hard stop is `failed`, drops and counts peer messages, answers clients
//! `recovering`, restarts normally and fails again at the first append past the stop, while
//! the other nodes keep serving. Negative fixtures break each observed fact once and the
//! verdict must reject it.
#![cfg(test)]

// check: JOURNAL-EXHAUSTION

use std::path::Path;

use types::{ByteCount, Config, NodeId};

use crate::cluster::{Cluster, ClusterError};
use crate::entropy::MasterSeed;
use crate::exhaustion::{Facts, MAX_WRITES, Run, SCHEMA, Stopped, run, run_with};

/// One way to break a passing run's facts.
type Break = Box<dyn Fn(&mut Facts)>;
type Cases = Vec<(&'static str, Break)>;
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::EventBudget;
use crate::slice::Verdict;

const LOCAL: &str = include_str!("../../../config/local.toml");

const SEEDS: [u64; 4] = [1, 2, 3, 7];

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn drive(seed: u64) -> Run {
    run(&config(), MasterSeed::new(seed), EventBudget::DEFAULT)
}

fn passed(seed: u64) -> Run {
    let run = drive(seed);
    assert_eq!(
        run.verdict,
        Verdict::Pass,
        "rule: a journal-exhaustion run passes (DM-7); seed={seed} failures={:?} error={:?} \
         facts={:?}; re-run: ./dev check JOURNAL-EXHAUSTION",
        run.failures,
        run.error,
        run.facts
    );
    run
}

fn node(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

#[test]
fn a_node_at_its_hard_stop_fails_and_the_scenario_passes() {
    for seed in SEEDS {
        let run = passed(seed);
        let failed = run.facts.failed.unwrap();
        assert!(
            failed.stopped.did_not_fit() && failed.stopped.limit == run.facts.limit,
            "rule: the frame that failed the node did not fit under its stop (DM-7); \
             seed={seed} stopped={:?} limit={}",
            failed.stopped,
            run.facts.limit
        );
        assert!(
            run.facts.all_observed(),
            "rule: a passing run observed every step; seed={seed} facts={:?}",
            run.facts
        );
    }
}

#[test]
fn the_journal_never_passes_its_stop() {
    for seed in SEEDS {
        let run = passed(seed);
        let failed = run.facts.failed.unwrap();
        let restarted = run.facts.restarted.unwrap();
        assert!(
            failed.journal_len <= run.facts.limit && restarted.journal_len <= run.facts.limit,
            "rule: no append took the journal past max_journal_bytes, before or after the \
             restart (DM-7); seed={seed} limit={} failed_len={} restarted_len={}",
            run.facts.limit,
            failed.journal_len,
            restarted.journal_len
        );
    }
}

#[test]
fn a_failed_node_drops_and_counts_peer_messages_and_answers_recovering() {
    for seed in SEEDS {
        let failed = passed(seed).facts.failed.unwrap();
        assert!(
            failed.peers_dropped_later > failed.peers_dropped_at_failure,
            "rule: a failed node keeps dropping and counting peer envelopes (DM-7, §7.11); \
             seed={seed} at_failure={} later={}",
            failed.peers_dropped_at_failure,
            failed.peers_dropped_later
        );
        assert!(
            failed.answered_recovering && failed.read_answered_recovering,
            "rule: a failed node answers a write and a read `recovering` (A-2); seed={seed} \
             failed={failed:?}"
        );
    }
}

#[test]
fn the_other_nodes_keep_serving_and_agree() {
    for seed in SEEDS {
        let facts = passed(seed).facts;
        assert!(
            facts.others_served && facts.others_agree,
            "rule: one failed node does not stop the other four reading, writing or agreeing \
             (DM-7); seed={seed} facts={facts:?}"
        );
        assert_ne!(
            facts.leader, facts.victim,
            "rule: the victim is a follower, so no quorum is exhausted; seed={seed}"
        );
    }
}

#[test]
fn a_restarted_node_recovers_then_fails_at_the_first_append_past_the_stop() {
    for seed in SEEDS {
        let restarted = passed(seed).facts.restarted.unwrap();
        assert!(
            restarted.started_unfailed,
            "rule: after a restart the node recovers normally, not failed (DM-7); seed={seed}"
        );
        let again = restarted.refailed.unwrap();
        assert!(
            again.did_not_fit(),
            "rule: it re-enters failed at an append that would exceed the stop (DM-7); \
             seed={seed} stopped={again:?}"
        );
    }
}

#[test]
fn the_same_seed_gives_the_same_trace_and_another_seed_another() {
    let (first, again, other) = (drive(1), drive(1), drive(2));
    assert_eq!(
        first.hash(),
        again.hash(),
        "rule: the same seed gives byte-identical traces (T-1)"
    );
    assert_ne!(
        first.hash(),
        other.hash(),
        "rule: another seed gives another trace (negative control)"
    );
}

#[test]
fn the_report_says_it_is_outside_the_healthy_envelope_and_claims_no_liveness() {
    let report = passed(1).report();
    assert_eq!(report["schema"], SCHEMA);
    assert_eq!(
        report["outside_healthy_envelope"], true,
        "rule: an exhausted node is outside the healthy envelope (§9.4)"
    );
    let claim = report["liveness_claim"].as_str().unwrap();
    assert!(
        claim.starts_with("none"),
        "rule: the scenario never claims quorum-exhaustion liveness (DM-7); claim={claim:?}"
    );
}

/// A passing run's facts.
fn good() -> Facts {
    passed(1).facts
}

const STOPPED: Stopped = Stopped {
    end: 10,
    frame_len: 5,
    limit: 100,
};

/// Breaks of the facts about the node that failed.
fn failed_cases() -> Cases {
    vec![
        ("no leader", Box::new(|f| f.leader = None)),
        ("no victim", Box::new(|f| f.victim = None)),
        ("never failed", Box::new(|f| f.failed = None)),
        (
            "the refused frame fit",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.stopped = STOPPED;
                }
            }),
        ),
        (
            "the first stop reports another limit",
            Box::new(|f| {
                let limit = f.limit;
                if let Some(x) = f.failed.as_mut() {
                    x.stopped.limit = limit + 1;
                }
            }),
        ),
        (
            "the journal moved after the node failed",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.journal_len_at_failure -= 1;
                }
            }),
        ),
        (
            "the drop count did not grow",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.peers_dropped_at_failure = x.peers_dropped_later;
                }
            }),
        ),
        (
            "journal past the stop",
            Box::new(|f| {
                let limit = f.limit;
                if let Some(x) = f.failed.as_mut() {
                    x.journal_len = limit + 1;
                }
            }),
        ),
        (
            "no peer message was dropped",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.peers_dropped_later = 0;
                    x.peers_dropped_at_failure = 0;
                }
            }),
        ),
        (
            "the count went down",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.peers_dropped_at_failure = x.peers_dropped_later + 1;
                }
            }),
        ),
        (
            "a write was not answered recovering",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.answered_recovering = false;
                }
            }),
        ),
        (
            "a read was not answered recovering",
            Box::new(|f| {
                if let Some(x) = f.failed.as_mut() {
                    x.read_answered_recovering = false;
                }
            }),
        ),
    ]
}

/// Breaks of the facts about the other nodes and the restart.
fn restart_cases() -> Cases {
    vec![
        (
            "the others did not serve",
            Box::new(|f| f.others_served = false),
        ),
        ("the others disagree", Box::new(|f| f.others_agree = false)),
        ("never restarted", Box::new(|f| f.restarted = None)),
        (
            "restarted failed",
            Box::new(|f| {
                if let Some(x) = f.restarted.as_mut() {
                    x.started_unfailed = false;
                }
            }),
        ),
        (
            "never failed again",
            Box::new(|f| {
                if let Some(x) = f.restarted.as_mut() {
                    x.refailed = None;
                }
            }),
        ),
        (
            "failed again at a frame that fit",
            Box::new(|f| {
                if let Some(x) = f.restarted.as_mut() {
                    x.refailed = Some(STOPPED);
                }
            }),
        ),
        (
            "journal past the stop after the restart",
            Box::new(|f| {
                let limit = f.limit;
                if let Some(x) = f.restarted.as_mut() {
                    x.journal_len = limit + 1;
                }
            }),
        ),
        (
            "mirrors not audited",
            Box::new(|f| f.mirrors_audited = false),
        ),
    ]
}

/// Each fact broken once; the facts must no longer count as observed.
#[test]
fn every_fact_the_verdict_needs_has_a_negative_fixture() {
    let control = good();
    assert!(control.all_observed(), "control: the untouched facts pass");
    for (what, change) in failed_cases().into_iter().chain(restart_cases()) {
        let mut facts = control;
        change(&mut facts);
        assert!(
            !facts.all_observed(),
            "rule: the verdict rejects a run where {what}; facts={facts:?}"
        );
    }
}

#[test]
fn a_hard_stop_the_victim_never_reaches_fails_the_run() {
    // A stop far above anything the writes append is never hit: the exhaust step gives up
    // after MAX_WRITES and the run fails (it is a finding about the script, not a pass).
    let run = run_with(&config(), MasterSeed::new(1), EventBudget::DEFAULT, 1 << 40);
    assert_eq!(
        run.verdict,
        Verdict::Fail,
        "rule: a run whose victim never reaches its stop is not a pass; failures={:?} error={:?}",
        run.failures,
        run.error
    );
    assert!(
        run.failures
            .iter()
            .any(|why| why.contains(&format!("{MAX_WRITES} writes did not take the victim"))),
        "rule: the failure names the writes that did not reach the stop; failures={:?}",
        run.failures
    );
}

#[test]
fn a_run_the_harness_cannot_finish_is_incomplete_not_a_pass() {
    let run = run(&config(), MasterSeed::new(1), EventBudget::new(50));
    assert_eq!(
        run.verdict,
        Verdict::Incomplete,
        "rule: a run that exhausts its event budget is incomplete (P10); failures={:?} error={:?}",
        run.failures,
        run.error
    );
    assert!(run.error.is_some(), "rule: the harness error is reported");
}

#[test]
fn an_unknown_node_cannot_be_given_a_stop() {
    let config = config();
    let peer = LinkProfile::new(200, 2_000, Ppm::NEVER, Ppm::NEVER).unwrap();
    let client = LinkProfile::new(100, 1_000, Ppm::NEVER, Ppm::NEVER).unwrap();
    let net = NetConfig {
        peer,
        client,
        queue: QueueBound::of(&config),
        max_in_flight: EventBudget::DEFAULT,
    };
    let mut cluster = Cluster::new(config, MasterSeed::new(1), net, EventBudget::DEFAULT).unwrap();
    let error = cluster
        .limit_journal(node(99), ByteCount::new(1_000))
        .unwrap_err();
    assert!(
        matches!(error, ClusterError::UnknownNode { .. }),
        "rule: a stop for a node outside the cluster is refused; error={error:?}"
    );
    assert!(
        cluster
            .limit_journal(node(1), ByteCount::new(1_000))
            .is_ok()
    );
}
