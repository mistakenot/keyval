#![cfg(test)]
//! Fixtures for tier selection, T2 reuse, the pass criteria and the T0 overrun rule
//! (PLAN.md §10.10, §10.11).

use std::collections::BTreeMap;

use super::*;

/// The plan's fixed replay-pair count of each tier, keyed by tier name.
fn replay_pair_table() -> BTreeMap<&'static str, u32> {
    [
        GateTier::Fast,
        GateTier::Integration,
        GateTier::Campaign,
        GateTier::Release,
    ]
    .into_iter()
    .map(|tier| {
        let p = plan(tier);
        (p.name, p.replay_pairs)
    })
    .collect()
}

fn member(name: &str, dir: &str, deps: &[&str]) -> Member {
    Member {
        name: name.to_owned(),
        dir: dir.to_owned(),
        depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
    }
}

/// A miniature of the real layout: a chain types ← statemachine ← node ← server, a sim
/// that depends on node, an oracle with no production edge, and a nested probe directory.
fn workspace() -> Vec<Member> {
    vec![
        member("types", "crates/types", &[]),
        member("statemachine", "crates/statemachine", &["types"]),
        member("node", "crates/node", &["types", "statemachine"]),
        member("server", "crates/server", &["node"]),
        member("sim", "crates/sim", &["node", "model"]),
        member("model", "crates/model", &[]),
        member("xtask", "xtask", &[]),
        member("probe", "xtask/probes/probe", &["types"]),
    ]
}

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| (*p).to_owned()).collect()
}

fn names(selected: &Selected) -> Vec<&str> {
    selected.crates.iter().map(String::as_str).collect()
}

// check: TIER-PLAN
#[test]
fn replay_pairs_are_exactly_2_16_64_and_100() {
    let observed: Vec<(&str, u32)> = replay_pair_table().into_iter().collect();
    assert_eq!(
        observed,
        vec![("T0", 2), ("T1", 16), ("T2", 64), ("T3", 100)],
        "rule: §10.10 fixes the replay-pair count of each tier"
    );
}

#[test]
fn only_t3_has_a_seed_floor_and_it_is_100000() {
    for tier in [GateTier::Fast, GateTier::Integration, GateTier::Campaign] {
        assert_eq!(
            plan(tier).seed_minimum,
            None,
            "rule: {tier:?} sizes its seed budget to fit; it has no floor (§10.10)"
        );
    }
    assert_eq!(plan(GateTier::Release).seed_minimum, Some(100_000));
    assert_eq!(
        [T0_SEED_TARGET, T1_SEED_TARGET, T2_SEED_TARGET],
        [50, 2_000, 10_000],
        "rule: the initial seed targets of T0, T1 and T2"
    );
}

#[test]
fn each_tier_runs_its_own_registry_tier_and_the_ones_below() {
    for (tier, expected) in [
        (GateTier::Fast, vec!["T0"]),
        (GateTier::Integration, vec!["T0", "T1"]),
        (GateTier::Campaign, vec!["T0", "T1", "T2"]),
        (GateTier::Release, vec!["T0", "T1", "T2", "T3"]),
    ] {
        let p = plan(tier);
        assert_eq!(p.registry_tiers, expected.as_slice(), "tier={tier:?}");
        assert_eq!(
            p.registry_tiers.last().copied(),
            Some(p.name),
            "rule: a tier's own registry tier is the last it runs; tier={tier:?}"
        );
    }
}

#[test]
fn only_t0_shares_the_commit_candidate_and_only_t3_refuses_planned() {
    let flags: Vec<(&str, bool, bool)> = [
        GateTier::Fast,
        GateTier::Integration,
        GateTier::Campaign,
        GateTier::Release,
    ]
    .iter()
    .map(|t| {
        let p = plan(*t);
        (p.name, p.per_run_tree, p.accepts_planned)
    })
    .collect();
    assert_eq!(
        flags,
        vec![
            ("T0", false, true),
            ("T1", true, true),
            ("T2", true, true),
            ("T3", true, false)
        ],
        "rule: T1–T3 materialise their own tree (§10.10); T3 alone accepts no planned check"
    );
}

#[test]
fn t0_selects_the_changed_crate_and_everything_that_depends_on_it() {
    let selected = select(
        &plan(GateTier::Fast),
        &workspace(),
        &paths(&["crates/statemachine/src/lib.rs"]),
    );
    assert_eq!(
        names(&selected),
        ["node", "server", "sim", "statemachine"],
        "rule: the changed crate plus its reverse dependencies, transitively; selected={selected:?}"
    );
    assert!(!selected.whole_workspace);
    assert_eq!(selected.reason, Reason::ChangedMembers);
}

#[test]
fn t0_leaves_out_crates_that_do_not_depend_on_the_change() {
    let selected = select(
        &plan(GateTier::Fast),
        &workspace(),
        &paths(&["crates/model/src/lib.rs"]),
    );
    assert_eq!(
        names(&selected),
        ["model", "sim"],
        "rule: model is reached only by sim; types, node, server and xtask are untouched; \
         selected={selected:?}"
    );
}

#[test]
fn t1_t2_and_t3_take_the_whole_workspace_whatever_changed() {
    for tier in [GateTier::Integration, GateTier::Campaign, GateTier::Release] {
        let selected = select(
            &plan(tier),
            &workspace(),
            &paths(&["crates/model/src/lib.rs"]),
        );
        assert!(
            selected.whole_workspace && selected.crates.len() == workspace().len(),
            "rule: {tier:?} tests the whole workspace; selected={selected:?}"
        );
        assert_eq!(selected.reason, Reason::TierSelectsWholeWorkspace);
    }
}

#[test]
fn t0_and_t1_differ_on_the_same_change() {
    let change = paths(&["crates/types/src/lib.rs"]);
    let t0 = select(&plan(GateTier::Fast), &workspace(), &change);
    let t1 = select(&plan(GateTier::Integration), &workspace(), &change);
    assert!(
        t0.crates.len() < t1.crates.len(),
        "rule: T0 skips model and xtask, T1 does not; t0={t0:?} t1={t1:?}"
    );
    assert!(
        t0.crates.contains(&"probe".to_owned()),
        "dev/normal edge counts; t0={t0:?}"
    );
}

#[test]
fn a_nested_directory_belongs_to_the_innermost_member() {
    let selected = select(
        &plan(GateTier::Fast),
        &workspace(),
        &paths(&["xtask/probes/probe/src/lib.rs"]),
    );
    assert_eq!(
        names(&selected),
        ["probe"],
        "rule: xtask/probes/probe is its own member, not part of xtask; selected={selected:?}"
    );
}

#[test]
fn a_shared_path_or_no_changed_path_selects_everything() {
    for change in [
        paths(&["Cargo.lock"]),
        paths(&["registry/checks.toml"]),
        paths(&["api/openapi.yaml"]),
        paths(&["crates/types/src/lib.rs", "sim/corpus/seed-1.json"]),
        Vec::new(),
    ] {
        let selected = select(&plan(GateTier::Fast), &workspace(), &change);
        assert!(
            selected.whole_workspace,
            "rule: a path outside every member, or none given, leaves nothing out (P10); \
             change={change:?} selected={selected:?}"
        );
    }
}

#[test]
fn nested_prose_and_reports_alone_select_no_crate() {
    let selected = select(
        &plan(GateTier::Fast),
        &workspace(),
        &paths(&["reports/traceability.md", "docs/notes.md"]),
    );
    assert_eq!(
        (selected.crates.len(), selected.reason),
        (0, Reason::NothingBuilt),
        "rule: no crate is built from generated reports or nested prose; selected={selected:?}"
    );
}

#[test]
fn the_plan_the_agent_rules_and_the_tracker_export_select_everything() {
    for path in ["PLAN.md", "AGENTS.md", ".beads/issues.jsonl"] {
        let selected = select(&plan(GateTier::Fast), &workspace(), &paths(&[path]));
        assert_eq!(
            (selected.whole_workspace, selected.reason),
            (true, Reason::SharedPathChanged),
            "rule: xtask's own checks read {path}, so changing it leaves nothing out; \
             selected={selected:?}"
        );
    }
}

#[test]
fn a_directory_name_that_merely_starts_like_a_member_is_not_inside_it() {
    let selected = select(
        &plan(GateTier::Fast),
        &workspace(),
        &paths(&["crates/types-extra/src/lib.rs"]),
    );
    assert!(
        selected.whole_workspace,
        "rule: crates/types-extra is not crates/types; unowned paths select everything; \
         selected={selected:?}"
    );
}

#[test]
fn the_real_workspace_has_the_dependency_edges_selection_needs() {
    let root = crate::test_support::workspace_root()
        .canonicalize()
        .unwrap();
    let metadata = crate::metadata::load(&root).unwrap();
    let all = members(&metadata, &root);
    let selected = select(
        &plan(GateTier::Fast),
        &all,
        &paths(&["crates/types/src/lib.rs"]),
    );
    for expected in ["types", "statemachine", "node"] {
        assert!(
            selected.crates.iter().any(|c| c == expected),
            "rule: a change to types reaches {expected}; selected={selected:?}"
        );
    }
}

#[test]
fn suites_are_recognised_by_their_check_ids() {
    for (id, suite) in [
        ("CONF-HARNESS", Some(Suite::Conformance)),
        ("CONF-TOPOLOGY", Some(Suite::Conformance)),
        ("CRASH-PROBES", Some(Suite::RealFileProbes)),
        ("LIFECYCLE-SMOKE", Some(Suite::RealFileProbes)),
        ("BUG-VARIANTS", Some(Suite::BugVariants)),
        ("LINT-PROBES", None),
        ("K-1", None),
        ("SIM-RUN", None),
    ] {
        assert_eq!(suite_of(id), suite, "rule: suite membership of {id}");
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|i| (*i).to_owned()).collect()
}

#[test]
fn t2_reuses_conformance_probes_and_variants_and_t1_reuses_nothing() {
    let all = ids(&[
        "K-1",
        "CONF-HARNESS",
        "CRASH-PROBES",
        "BUG-VARIANTS",
        "SIM-RUN",
    ]);
    let t2 = reuse_split(&plan(GateTier::Campaign), &all);
    assert_eq!(
        t2.run,
        ids(&["K-1", "SIM-RUN"]),
        "rule: T2 does not re-run those suites"
    );
    assert_eq!(
        t2.reused,
        vec![
            ("CONF-HARNESS".to_owned(), Suite::Conformance),
            ("CRASH-PROBES".to_owned(), Suite::RealFileProbes),
            ("BUG-VARIANTS".to_owned(), Suite::BugVariants),
        ]
    );
    for tier in [GateTier::Fast, GateTier::Integration, GateTier::Release] {
        let split = reuse_split(&plan(tier), &all);
        assert_eq!(
            (split.run.len(), split.reused.len()),
            (5, 0),
            "rule: {tier:?} runs every suite itself"
        );
    }
}

fn evidence(run: &str, at: &str, passed: bool, checks: &[&str]) -> Evidence {
    Evidence {
        run_id: run.to_owned(),
        tree_id: "t".repeat(40),
        finished_at: at.to_owned(),
        passed,
        passed_checks: checks.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn needed() -> Vec<(String, Suite)> {
    vec![
        ("CONF-HARNESS".to_owned(), Suite::Conformance),
        ("CRASH-PROBES".to_owned(), Suite::RealFileProbes),
    ]
}

#[test]
fn the_latest_passing_t1_run_vouches_for_the_reused_suites() {
    let runs = vec![
        evidence(
            "old",
            "2026-10-01T00:00:00Z",
            true,
            &["CONF-HARNESS", "CRASH-PROBES"],
        ),
        evidence(
            "new",
            "2026-10-02T00:00:00Z",
            true,
            &["CONF-HARNESS", "CRASH-PROBES", "K-1"],
        ),
    ];
    let found = reusable_evidence(&runs, &needed()).unwrap();
    assert_eq!(found.run_id, "new", "rule: the latest run is the evidence");
}

#[test]
fn an_older_green_t1_run_never_hides_a_newer_red_one() {
    let runs = vec![
        evidence(
            "old",
            "2026-10-01T00:00:00Z",
            true,
            &["CONF-HARNESS", "CRASH-PROBES"],
        ),
        evidence("new", "2026-10-02T00:00:00Z", false, &["CONF-HARNESS"]),
    ];
    let why = reusable_evidence(&runs, &needed()).unwrap_err();
    assert!(
        why.contains("new") && why.contains("did not pass"),
        "why={why}"
    );
}

#[test]
fn a_t1_run_missing_a_reused_check_does_not_vouch_for_it() {
    let runs = vec![evidence(
        "only",
        "2026-10-02T00:00:00Z",
        true,
        &["CONF-HARNESS"],
    )];
    let why = reusable_evidence(&runs, &needed()).unwrap_err();
    assert!(
        why.contains("CRASH-PROBES") && !why.contains("CONF-HARNESS,"),
        "why={why}"
    );
}

#[test]
fn no_t1_run_means_no_evidence() {
    let why = reusable_evidence(&[], &needed()).unwrap_err();
    assert!(why.contains("no T1 run"), "why={why}");
}

fn complete_t3() -> Facts {
    Facts {
        planned_in_contents: Vec::new(),
        replay_pairs: Some(100),
        fresh_seeds: Some(100_000),
        zero_coverage_cells: Some(Vec::new()),
        checker_violations: Some(0),
        incomplete_runs: Some(0),
    }
}

#[test]
fn a_complete_t3_passes() {
    assert_eq!(
        judge(&plan(GateTier::Release), &complete_t3()),
        Vec::<String>::new()
    );
}

#[test]
fn t3_refuses_a_planned_check_anywhere_in_its_contents() {
    let mut facts = complete_t3();
    facts.planned_in_contents = ids(&["PARITY"]);
    let failures = judge(&plan(GateTier::Release), &facts);
    assert!(
        failures.len() == 1 && failures[0].contains("PARITY"),
        "rule: T3 accepts no planned check (§10.10); failures={failures:?}"
    );
    let t1 = judge(
        &plan(GateTier::Integration),
        &Facts {
            planned_in_contents: ids(&["PARITY"]),
            ..Facts::default()
        },
    );
    assert!(
        t1.is_empty(),
        "rule: T1 reports a planned check and passes; failures={t1:?}"
    );
}

#[test]
fn t3_needs_exactly_100_replay_pairs_and_at_least_100000_seeds() {
    for (pairs, seeds, expect) in [
        (99, 100_000, "replay pairs"),
        (101, 100_000, "replay pairs"),
        (100, 99_999, "fresh seeds"),
    ] {
        let mut facts = complete_t3();
        facts.replay_pairs = Some(pairs);
        facts.fresh_seeds = Some(seeds);
        let failures = judge(&plan(GateTier::Release), &facts);
        assert!(
            failures.len() == 1 && failures[0].contains(expect),
            "rule: pairs={pairs} seeds={seeds} breaks {expect}; failures={failures:?}"
        );
    }
    let mut more = complete_t3();
    more.fresh_seeds = Some(250_000);
    assert!(
        judge(&plan(GateTier::Release), &more).is_empty(),
        "more seeds than the floor pass"
    );
}

#[test]
fn t3_fails_on_a_zero_coverage_cell_and_on_anything_unmeasured() {
    let mut facts = complete_t3();
    facts.zero_coverage_cells = Some(ids(&["partition×restart"]));
    let failures = judge(&plan(GateTier::Release), &facts);
    assert!(
        failures.len() == 1 && failures[0].contains("partition×restart"),
        "failures={failures:?}"
    );

    let unmeasured = judge(&plan(GateTier::Release), &Facts::default());
    assert_eq!(
        unmeasured.len(),
        5,
        "rule: T3 requires every listed content, so each unmeasured fact fails; failures={unmeasured:?}"
    );
}

#[test]
fn t1_and_t2_fail_on_wrong_pair_counts_and_violations_but_not_on_unmeasured_contents() {
    for tier in [GateTier::Integration, GateTier::Campaign] {
        let p = plan(tier);
        assert!(
            judge(&p, &Facts::default()).is_empty(),
            "rule: {tier:?} runs only contents whose checks exist; unmeasured is reported, not failed"
        );
        let wrong = judge(
            &p,
            &Facts {
                replay_pairs: Some(p.replay_pairs - 1),
                ..Facts::default()
            },
        );
        assert_eq!(wrong.len(), 1, "tier={tier:?} failures={wrong:?}");
        let bad = judge(
            &p,
            &Facts {
                checker_violations: Some(2),
                incomplete_runs: Some(1),
                ..Facts::default()
            },
        );
        assert_eq!(
            bad.len(),
            2,
            "rule: 0 violations and 0 incomplete runs; failures={bad:?}"
        );
    }
}

#[test]
fn t0_judges_only_its_replay_pairs() {
    let p = plan(GateTier::Fast);
    assert_eq!(
        judge(
            &p,
            &Facts {
                replay_pairs: Some(2),
                ..Facts::default()
            }
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        judge(
            &p,
            &Facts {
                replay_pairs: Some(1),
                ..Facts::default()
            }
        )
        .len(),
        1
    );
}

fn timing(run: &str, at: &str, over: bool) -> T0Timing {
    T0Timing {
        run_id: run.to_owned(),
        finished_at: at.to_owned(),
        over_alert: over,
    }
}

#[test]
fn three_consecutive_slow_t0_runs_are_a_sustained_overrun() {
    let history = vec![
        timing("c", "2026-10-03T00:00:00Z", true),
        timing("a", "2026-10-01T00:00:00Z", true),
        timing("b", "2026-10-02T00:00:00Z", true),
    ];
    let found = sustained_overrun(&history).unwrap();
    assert_eq!(
        found.run_ids,
        ids(&["c", "b", "a"]),
        "newest first, whatever the input order"
    );
    assert_eq!(found.consecutive, SUSTAINED_RUNS);
}

#[test]
fn one_fast_run_among_the_latest_three_ends_the_overrun() {
    let history = vec![
        timing("a", "2026-10-01T00:00:00Z", true),
        timing("b", "2026-10-02T00:00:00Z", true),
        timing("c", "2026-10-03T00:00:00Z", false),
    ];
    assert_eq!(
        sustained_overrun(&history),
        None,
        "rule: the newest run was in budget"
    );
}

#[test]
fn fewer_than_three_receipts_never_make_a_sustained_overrun() {
    let history = vec![
        timing("a", "2026-10-01T00:00:00Z", true),
        timing("b", "2026-10-02T00:00:00Z", true),
    ];
    assert_eq!(sustained_overrun(&history), None);
    assert_eq!(sustained_overrun(&[]), None);
}

#[test]
fn old_slow_runs_do_not_count_once_three_newer_ones_are_in_budget() {
    let history = vec![
        timing("a", "2026-10-01T00:00:00Z", true),
        timing("b", "2026-10-02T00:00:00Z", true),
        timing("c", "2026-10-03T00:00:00Z", true),
        timing("d", "2026-10-04T00:00:00Z", false),
        timing("e", "2026-10-05T00:00:00Z", false),
        timing("f", "2026-10-06T00:00:00Z", false),
    ];
    assert_eq!(sustained_overrun(&history), None);
}
