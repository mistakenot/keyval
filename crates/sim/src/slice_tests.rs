//! W17.2's in-process tests of the slice scenario (PLAN.md §12.2 W17): it passes with every
//! step observed, across seeds; its report says what the run is and agrees with the run; a
//! run cut short is incomplete, in its verdict and in its trace's bytes (P10); and a
//! finding fails a run where the harness running out leaves it incomplete. The
//! fresh-process identity is `tests/sim_run.rs`'s.
#![cfg(test)]

// The SIM-RUN check selects these cases with `tests/sim_run.rs`'s; its marker is there.

use std::path::Path;

use types::{Config, NodeId};

use crate::cluster::ClusterError;
use crate::entropy::MasterSeed;
use crate::scheduler::{EventBudget, Shortfall};
use crate::slice::{Facts, LABEL, Profile, SCHEMA, Verdict, judge, run};
use crate::trace::{TraceHash, marks_incomplete};

const LOCAL: &str = include_str!("../../../config/local.toml");

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn slice(seed: u64) -> crate::slice::Run {
    run(
        &config(),
        MasterSeed::new(seed),
        Profile::Slice,
        EventBudget::DEFAULT,
    )
}

#[test]
fn seed_42_passes_the_slice_with_every_step_observed() {
    let run = slice(42);
    let facts = run.facts;
    let observed = (
        facts.elected.is_some(),
        facts.put.is_some_and(|put| put.index.get() >= 1),
        facts.read_back,
        facts
            .crashed
            .is_some_and(|crashed| crashed.unsynced_bytes > 0),
        facts.restarted.map(|restarted| restarted.incarnation),
        facts.mirrors_audited,
    );
    assert_eq!(
        (
            run.verdict,
            run.failures.as_slice(),
            run.error.as_deref(),
            observed
        ),
        (
            Verdict::Pass,
            &[][..],
            None,
            (true, true, true, true, Some(1), true)
        ),
        "rule: seed 42's slice elects, writes and reads back, crashes a follower holding \
         unsynced bytes, restarts it as incarnation 1 caught up, and audits every mirror \
         (§12.2 W17); facts={facts:?}"
    );
}

#[test]
fn every_seed_from_1_to_8_passes_the_slice() {
    let failed: Vec<(u64, Verdict, Vec<String>, Option<String>)> = (1..=8)
        .map(|seed| (seed, slice(seed)))
        .filter(|(_, run)| run.verdict != Verdict::Pass)
        .map(|(seed, run)| (seed, run.verdict, run.failures, run.error))
        .collect();
    assert!(
        failed.is_empty(),
        "rule: the slice is not tuned to one seed: seeds 1 to 8 pass it too (§12.2 W17); \
         failed={failed:?}"
    );
}

#[test]
fn the_report_says_what_the_run_is_and_agrees_with_the_run() {
    let run = slice(42);
    let report = run.report(Some("/trace/path"));
    let hash = format!("{:016x}", TraceHash::of(&run.trace).get());
    let lines = u64::try_from(run.trace.lines().count()).unwrap();
    assert_eq!(
        (
            report["schema"].as_str(),
            report["ok"].as_bool(),
            report["label"].as_str(),
            report["verdict"].as_str(),
            report["manifest"]["seed"].as_u64(),
            report["manifest"]["profile"].as_str(),
            report["trace"]["hash"].as_str(),
            report["trace"]["lines"].as_u64(),
            report["trace"]["complete"].as_bool(),
            report["trace"]["path"].as_str(),
            report["facts"]["restarted"]["incarnation"].as_u64(),
        ),
        (
            Some(SCHEMA),
            Some(true),
            Some(LABEL),
            Some("pass"),
            Some(42),
            Some("slice"),
            Some(hash.as_str()),
            Some(lines),
            Some(true),
            Some("/trace/path"),
            Some(1),
        ),
        "rule: the report names its schema, labels the run a simulated slice (P6), and \
         carries the run's verdict, seed, profile and trace hash; report={report}"
    );
    assert!(
        LABEL.starts_with("simulated slice") && LABEL.contains("not a real server"),
        "rule: the label says what the run is and what it is not (P6); {LABEL:?}"
    );
}

#[test]
fn a_run_cut_short_by_its_event_budget_is_incomplete_in_its_verdict_and_its_trace() {
    let run = run(
        &config(),
        MasterSeed::new(42),
        Profile::Slice,
        EventBudget::new(150),
    );
    let report = run.report(None);
    assert!(
        run.verdict == Verdict::Incomplete
            && run.error.is_some()
            && marks_incomplete(&run.trace)
            && report["ok"].as_bool() == Some(false)
            && report["trace"]["complete"].as_bool() == Some(false),
        "rule: a run stopped by its event budget is incomplete, never a pass, and its \
         trace's bytes say so (P10); verdict {:?}, error {:?}, last line {:?}",
        run.verdict,
        run.error,
        run.trace.lines().next_back()
    );
}

#[test]
fn only_the_slice_profile_exists_yet() {
    assert_eq!(
        (
            Profile::parse("slice"),
            Profile::parse("mixed"),
            Profile::parse(""),
            Profile::Slice.name()
        ),
        (Some(Profile::Slice), None, None, "slice"),
        "rule: W17's minimal `./dev sim run` knows the slice profile only (W42 adds the \
         others)"
    );
}

#[test]
fn a_finding_fails_the_run_and_a_harness_shortfall_leaves_it_incomplete() {
    let observed = slice(42).facts;
    let mirror = ClusterError::Mirror {
        node: NodeId::new(1).unwrap(),
        what: "the mirror has last index 7, the core 6".to_owned(),
    };
    let budget = ClusterError::Incomplete {
        events: 150,
        pending: 9,
        reason: Shortfall::BudgetExhausted,
    };
    let finding = judge(Some(&mirror), Vec::new(), &observed);
    let at_start = judge(Some(&mirror), Vec::new(), &Facts::default());
    let shortfall = judge(Some(&budget), Vec::new(), &observed);
    let unobserved = judge(None, Vec::new(), &Facts::default());
    let passed = judge(None, Vec::new(), &observed);
    let summary = |(verdict, failures, error): &(Verdict, Vec<String>, Option<String>)| {
        (*verdict, failures.len(), error.is_some())
    };
    assert_eq!(
        (
            summary(&finding),
            summary(&at_start),
            summary(&shortfall),
            summary(&unobserved),
            summary(&passed)
        ),
        (
            (Verdict::Fail, 1, false),
            (Verdict::Fail, 1, false),
            (Verdict::Incomplete, 0, true),
            (Verdict::Fail, 1, false),
            (Verdict::Pass, 0, false)
        ),
        "rule: a run stopped by a finding about the system (a mirror that disagrees), even \
         one at start-up with nothing observed, fails with the finding as its one failure; \
         one stopped by its event budget is incomplete with the reason (P10); one that ends \
         without observing every step fails; only every step observed passes (§10.2); \
         finding={finding:?} shortfall={shortfall:?} unobserved={unobserved:?}"
    );
}
