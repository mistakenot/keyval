//! W37.3's tests of the run phases and the heal protocol (PLAN.md §10.2, §10.4, §9.4): the
//! baseline before any fault, chaos at an offset, the heal resolving outstanding writes under
//! their own request IDs, the catch-up marker, the budget, the quorum guard and determinism.
#![cfg(test)]

// check: RUN-PHASES

use std::collections::BTreeSet;
use std::path::Path;

use types::{Config, KeyValueLimits, NodeId, RequestId};

use crate::clients::{End, Profile};
use crate::cluster::Observed;
use crate::drain::QUIET_TICKS;
use crate::entropy::MasterSeed;
use crate::nemesis::{CrashPoint, Fault};
use crate::observe::{Meta, document};
use crate::phases::{
    BASELINE_WRITES, Completion, Converged, Finished, HEAL_BUDGET_TICKS, Heal, QUORUM, Stage, run,
    run_with,
};
use crate::scenario::{Scenario, ScenarioError, Step, Timed};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::swarm::{FaultKind, FaultSet, Intensity, Swarm};

const LOCAL: &str = include_str!("../../../config/local.toml");

/// Seeds the run tests drive.
const SEEDS: [u64; 4] = [1, 2, 3, 7];

/// `local.toml` with values of at most 1 KiB, as the other run tests use.
fn config() -> Config {
    let text = LOCAL.replace("max_value_bytes = 65536", "max_value_bytes = 1024");
    Config::parse(&text, Path::new("config")).unwrap()
}

fn limits() -> KeyValueLimits {
    config().key_value_limits()
}

fn node(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

fn drive(scenario: &Scenario) -> Finished {
    run(&config(), scenario).unwrap_or_else(|error| {
        panic!(
            "rule: a scenario run reaches its end (§10.2); seed={} error={error:?}; re-run: \
             ./dev check RUN-PHASES",
            scenario.seed().get()
        )
    })
}

fn converged(finished: &Finished) -> &Converged {
    match &finished.heal {
        Heal::Converged(converged) => converged,
        Heal::Stalled(stalled) => {
            panic!("rule: the heal finishes within its budget (§10.2); stalled={stalled:?}")
        }
    }
}

fn generated(seed: u64) -> Scenario {
    Scenario::generate(MasterSeed::new(seed), limits(), Profile::PutGet)
}

#[test]
fn baseline_completes_before_the_first_fault() {
    for seed in SEEDS {
        let scenario = generated(seed);
        let finished = drive(&scenario);
        let chaos_at = finished.phases.chaos.at;
        let baseline = finished
            .clients
            .outcomes()
            .iter()
            .filter(|outcome| outcome.client.get() >= scenario.plan().clients.len() as u64)
            .filter(|outcome| outcome.request_id.is_some())
            .filter(|outcome| outcome.at_ms * 1_000 <= chaos_at.as_micros())
            .count();
        assert_eq!(
            baseline, BASELINE_WRITES,
            "rule: the baseline's writes end before chaos begins (§10.2); seed={seed} chaos_at={chaos_at:?}"
        );
        let first_fault = finished.cluster.faults().map(|(at, _)| *at).min();
        if let Some(first) = first_fault {
            assert!(
                first >= chaos_at,
                "rule: no fault before chaos (§10.2); seed={seed} first={first:?} chaos={chaos_at:?}"
            );
        }
        assert!(
            finished.phases.setup.seq <= finished.phases.baseline.seq
                && finished.phases.baseline.seq <= finished.phases.chaos.seq
                && finished.phases.chaos.seq <= finished.phases.heal.seq
                && finished.phases.heal.seq <= finished.phases.check.seq,
            "rule: phases are in order; seed={seed} phases={:?}",
            finished.phases
        );
    }
}

#[test]
fn chaos_runs_for_the_scenarios_length_at_an_offset() {
    let scenario = generated(2);
    let finished = drive(&scenario);
    let length = finished.phases.heal.at.as_micros() - finished.phases.chaos.at.as_micros();
    assert_eq!(
        length,
        scenario.chaos_ms() * 1_000,
        "rule: chaos lasts the scenario's chaos length (§10.2); seed=2"
    );
    assert!(
        finished.phases.chaos.at > VirtualTime::ZERO,
        "rule: the baseline takes virtual time before chaos begins"
    );
    let offset = finished.phases.chaos.at.as_micros();
    let fired = |wanted: NodeId| {
        finished.cluster.faults().any(|(_, fault)| match fault {
            Fault::Crash { node, .. } => *node == wanted,
            _ => false,
        })
    };
    for timed in scenario.faults() {
        // The latest crash or arming of `node` before this step: a restart closing an arming
        // is a no-op when it never fired, so it is not logged.
        let closes_arm = |wanted: NodeId| {
            let mut last_is_arm = false;
            for other in scenario.faults() {
                if other.at_ms >= timed.at_ms {
                    break;
                }
                match other.step {
                    Step::Arm { node, .. } if node == wanted => last_is_arm = true,
                    Step::Crash { node, .. } if node == wanted => last_is_arm = false,
                    _ => {}
                }
            }
            last_is_arm
        };
        match timed.step {
            // Arming is not a fault that happened: its crash is logged when it fires.
            Step::Arm { .. } => continue,
            Step::Restart { node } if closes_arm(node) && !fired(node) => continue,
            _ => {}
        }
        let wanted = offset + timed.at_ms * 1_000;
        assert!(
            finished
                .cluster
                .faults()
                .any(|(at, _)| at.as_micros() == wanted),
            "rule: each step happens at its offset time (§10.4); step={timed:?} wanted={wanted}"
        );
    }
}

#[test]
fn the_heal_converges_with_a_unique_marker_on_every_recoverable_node() {
    for seed in SEEDS {
        let scenario = generated(seed);
        let finished = drive(&scenario);
        let heal = converged(&finished);
        assert!(
            heal.ticks <= HEAL_BUDGET_TICKS,
            "rule: the heal fits its budget (§10.2); seed={seed} ticks={}",
            heal.ticks
        );
        assert!(
            heal.nodes.len() >= QUORUM && heal.nodes.len() + heal.lost.len() == 5,
            "rule: recoverable plus lost nodes are the five voters; seed={seed} heal={heal:?}"
        );
        let marker = heal
            .marker()
            .expect("rule: the marker heal writes a marker (§10.2)");
        let first = heal.nodes[0];
        for end in &heal.nodes {
            assert_eq!(
                end.digest, first.digest,
                "rule: every recoverable node holds the same state once the marker is applied \
                 everywhere (K-7: trailing election no-ops may leave applied indices unequal); \
                 seed={seed} node={:?}",
                end.node
            );
            let ctl = finished.cluster.controller(end.node).unwrap();
            assert!(
                ctl.state().history().get(&marker).is_some(),
                "rule: the marker is applied on every recoverable node; seed={seed} node={:?}",
                end.node
            );
        }
        let planned: BTreeSet<RequestId> = scenario
            .plan()
            .clients
            .iter()
            .flat_map(|client| client.ops.iter())
            .filter_map(|op| op.intent.request_id())
            .collect();
        assert!(
            !planned.contains(&marker),
            "rule: the marker is uniquely identified; seed={seed} marker={marker}",
        );
        let marker_writes = finished
            .clients
            .sends()
            .iter()
            .filter(|send| {
                matches!(&send.request, types::ClientOp::Write { request_id, .. } if *request_id == marker)
            })
            .count();
        assert!(
            marker_writes >= 1,
            "rule: the marker is written by a client, not planted; seed={seed}"
        );
    }
}

#[test]
fn no_logical_write_is_left_unresolved() {
    for seed in SEEDS {
        let scenario = generated(seed);
        let finished = drive(&scenario);
        converged(&finished);
        assert_eq!(
            (finished.clients.in_flight(), finished.clients.parked()),
            (0, 0),
            "rule: the heal resolves every outstanding write (§10.2); seed={seed}"
        );
        for outcome in finished.clients.outcomes() {
            if outcome.request_id.is_some() {
                assert!(
                    !matches!(outcome.end, End::Unresolved(_)),
                    "rule: a write ends in a terminal answer or a refusal; seed={seed} outcome={outcome:?}"
                );
            }
        }
        // One outcome per write that started, under the request ID the plan gave it. The
        // heal starts nothing new (§10.2), so the clients' plans end at what started: a
        // prefix of the scenario's plan, plus the baseline's writes and the marker.
        let started: BTreeSet<RequestId> = finished
            .clients
            .plan()
            .clients
            .iter()
            .flat_map(|client| client.ops.iter())
            .filter_map(|op| op.intent.request_id())
            .collect();
        let ended: Vec<RequestId> = finished
            .clients
            .outcomes()
            .iter()
            .filter_map(|outcome| outcome.request_id)
            .collect();
        let distinct: BTreeSet<RequestId> = ended.iter().copied().collect();
        assert!(
            distinct == started && distinct.len() == ended.len(),
            "rule: every write that started ends exactly once under its own ID; seed={seed} \
             started={} ended={} distinct={}",
            started.len(),
            ended.len(),
            distinct.len()
        );
        let scenario_ids: BTreeSet<RequestId> = scenario
            .plan()
            .clients
            .iter()
            .flat_map(|client| client.ops.iter())
            .filter_map(|op| op.intent.request_id())
            .collect();
        assert!(
            scenario_ids.len() + BASELINE_WRITES + 1 >= started.len()
                && started
                    .iter()
                    .filter(|id| scenario_ids.contains(id))
                    .count()
                    + BASELINE_WRITES
                    + 1
                    == started.len(),
            "rule: a started write is the scenario's, the baseline's or the marker; seed={seed}"
        );
    }
}

#[test]
fn the_control_converges_without_a_fault() {
    let scenario = Scenario::control(MasterSeed::new(5), limits(), Profile::Mixed);
    let finished = drive(&scenario);
    let heal = converged(&finished);
    assert_eq!(
        (
            heal.lost.len(),
            heal.nodes.len(),
            finished.cluster.faults().count()
        ),
        (0, 5, 2),
        "rule: the control loses nothing and the heal's own restores are its only faults; heal={heal:?}"
    );
}

#[test]
fn the_heal_injects_no_fault_but_its_restores() {
    for seed in SEEDS {
        let scenario = generated(seed);
        let finished = drive(&scenario);
        let heal_at = finished.phases.heal.at;
        for (at, fault) in finished.cluster.faults() {
            if *at >= heal_at {
                assert!(
                    matches!(
                        fault,
                        Fault::Resume { .. }
                            | Fault::Restart { .. }
                            | Fault::Heal
                            | Fault::Profiles { .. }
                    ),
                    "rule: the heal only restores (§10.2); seed={seed} at={at:?} fault={fault:?}"
                );
            }
        }
    }
}

#[test]
fn the_same_seed_gives_the_same_trace() {
    for seed in [3, 11] {
        let scenario = generated(seed);
        let first = drive(&scenario);
        let second = drive(&scenario);
        assert_eq!(
            first.cluster.trace().hash(),
            second.cluster.trace().hash(),
            "rule: a seed reproduces its run exactly (T-4); seed={seed}"
        );
        assert_eq!(
            first.phases, second.phases,
            "rule: phase marks reproduce; seed={seed}"
        );
        assert_eq!(
            first.heal, second.heal,
            "rule: the heal reproduces; seed={seed}"
        );
    }
}

#[test]
fn a_starved_heal_is_stalled_not_an_error() {
    let scenario = generated(1);
    let finished = run_with(&config(), &scenario, 1).unwrap();
    let Heal::Stalled(stalled) = &finished.heal else {
        panic!(
            "rule: a one-tick heal cannot finish (§9.4); heal={:?}",
            finished.heal
        );
    };
    assert!(
        stalled.ticks <= 1 && [Stage::Resolving, Stage::Marker].contains(&stalled.stage),
        "rule: a stalled heal reports its stage and stops at its budget; stalled={stalled:?}"
    );
}

#[test]
fn a_heal_with_a_lost_node_converges_on_the_recoverable_ones() {
    let swarm = Swarm::Faulty {
        faults: FaultSet::new(&FaultKind::ALL).unwrap(),
        intensity: Intensity::Medium,
    };
    let faults = vec![Timed {
        at_ms: 2_000,
        step: Step::Lose { node: node(3) },
    }];
    let scenario = Scenario::from_parts(
        1,
        swarm,
        Profile::PutGet,
        8_000,
        faults,
        generated(1).plan().clone(),
    )
    .unwrap();
    let finished = drive(&scenario);
    let Heal::Converged(done) = &finished.heal else {
        panic!(
            "rule: a scenario losing one voter heals on the other four (§10.4); heal={:?}",
            finished.heal
        );
    };
    assert_eq!(
        done.lost,
        vec![node(3)],
        "rule: the lost node is reported, not waited for; done={done:?}"
    );
    assert_eq!(
        done.nodes.len(),
        4,
        "rule: the four recoverable nodes all applied the marker; done={done:?}"
    );
}

#[test]
fn a_scenario_that_loses_a_quorum_cannot_be_built() {
    let swarm = Swarm::Faulty {
        faults: FaultSet::new(&FaultKind::ALL).unwrap(),
        intensity: Intensity::Medium,
    };
    let faults = (1..=3)
        .map(|n| Timed {
            at_ms: 1_000 * n,
            step: Step::Lose { node: node(n) },
        })
        .collect();
    let built = Scenario::from_parts(
        1,
        swarm,
        Profile::PutGet,
        60_000,
        faults,
        generated(1).plan().clone(),
    );
    assert!(
        matches!(built, Err(ScenarioError::Invalid(_))),
        "rule: three lost voters leave no quorum, so the scenario is refused before any run \
         (§10.4 quorum guard); built={built:?}"
    );
}

#[test]
fn disarming_cancels_every_armed_crash() {
    let scenario = generated(1);
    let (mut cluster, _clients) = scenario.begin(&config()).unwrap();
    cluster
        .arm(node(1), CrashPoint::BeforeSync, crate::Survival::Nothing)
        .unwrap();
    cluster
        .arm(node(2), CrashPoint::BeforeReply, crate::Survival::All)
        .unwrap();
    assert_eq!(
        cluster.disarm_all(),
        2,
        "rule: both armed nodes are disarmed"
    );
    assert_eq!(cluster.disarm_all(), 0, "rule: nothing is left armed");
}

#[test]
fn parked_writes_resume_under_their_own_request_id() {
    let scenario = generated(4);
    let (mut cluster, mut clients) = scenario.begin(&config()).unwrap();
    clients.hold_plan();
    // Every node stalls before the client's write, so its retries run out unanswered.
    for n in 1..=5 {
        cluster
            .inject_at(VirtualTime::ZERO, Fault::Pause { node: node(n) })
            .unwrap();
    }
    let write = crate::phases::baseline_writes(&scenario, limits());
    let client = clients.add_client(write[..1].to_vec(), 0);
    let mut rounds = 0;
    while clients.parked() == 0 {
        let to = VirtualTime::from_micros(cluster.now().as_micros() + 1_000_000);
        clients.run(&mut cluster, to).unwrap();
        rounds += 1;
        assert!(
            rounds < 600,
            "rule: a write nobody answers runs out its retries"
        );
    }
    let before = clients.outcomes().to_vec();
    assert!(
        matches!(before[0].end, End::Unresolved(_)),
        "rule: an unanswered write is unresolved, never failed (§8.7); outcome={:?}",
        before[0]
    );
    let request_id = before[0].request_id;
    for n in 1..=5 {
        cluster
            .inject_at(cluster.now(), Fault::Resume { node: node(n) })
            .unwrap();
    }
    assert_eq!(clients.resume_parked(&mut cluster).unwrap(), 1);
    let mut rounds = 0;
    while clients.in_flight() > 0 {
        let to = VirtualTime::from_micros(cluster.now().as_micros() + 100_000);
        clients.run(&mut cluster, to).unwrap();
        cluster.run_until(to).unwrap();
        rounds += 1;
        assert!(
            rounds < 600,
            "rule: the resumed write resolves once nodes run"
        );
    }
    let after: Vec<_> = clients
        .outcomes()
        .iter()
        .filter(|outcome| outcome.client == client)
        .collect();
    assert_eq!(
        after.len(),
        1,
        "rule: a resumed write has one outcome, the resumed one; after={after:?}"
    );
    assert!(
        matches!(after[0].end, End::Done(_)) && after[0].request_id == request_id,
        "rule: the resumed write ends in a terminal answer under its own ID; after={after:?}"
    );
    assert_eq!(clients.parked(), 0);
}

#[test]
fn the_observation_of_a_run_builds_and_is_complete() {
    let scenario = generated(2);
    let config = config();
    let finished = drive(&scenario);
    converged(&finished);
    let meta = Meta {
        seed: scenario.seed().get(),
        scenario_version: 1,
        profile: "swarm".to_owned(),
        build_id: "test-build".to_owned(),
        prng: crate::slice::PRNG.to_owned(),
        max_events: EventBudget::DEFAULT.get(),
        chaos_ticks: finished.chaos_ticks,
        heal_budget_ticks: HEAL_BUDGET_TICKS,
        heal: scenario.workload().heal_mode(),
        quiet_ticks: QUIET_TICKS,
        phases: finished.phases,
    };
    let built = document(&finished.cluster, &finished.clients, &config, &meta);
    assert!(
        built.is_ok(),
        "rule: a finished run's observation builds (§10.2); error={:?}",
        built.err()
    );
    assert!(
        finished
            .cluster
            .observed()
            .any(|entry| matches!(entry, Observed::Invoked { .. })),
        "rule: the run observed invocations"
    );
}

#[test]
fn many_swarm_seeds_converge_within_the_budget() {
    let mut stalled = Vec::new();
    let mut resumed = 0;
    for seed in (20..28).chain([39]) {
        let profile = if seed % 2 == 0 {
            Profile::PutGet
        } else {
            Profile::Mixed
        };
        let scenario = Scenario::generate(MasterSeed::new(seed), limits(), profile);
        let finished = drive(&scenario);
        match &finished.heal {
            Heal::Stalled(found) => stalled.push((seed, found.clone())),
            Heal::Converged(done) => resumed += done.resumed,
        }
    }
    assert!(
        resumed > 0,
        "rule: some swarm seed parks a plan write that the heal resumes, or the resume path is untested"
    );
    assert!(
        stalled.is_empty(),
        "rule: a healthy-envelope scenario heals within 600 ticks (§9.4, K-9); stalled={stalled:?}"
    );
}

/// `local.toml` with the small-limits fixture's capacity limits (DM-6, DM-7): the fixture
/// itself has three voters, and a scenario drives five. A live-quota profile (DM-6) keeps the
/// default history budget and log bound, which its writes must not exhaust; a history profile (DM-7) takes
/// the small one, with the journal bound at the margin formula's minimum.
fn small_config(profile: Profile) -> Config {
    let mut limits = vec![
        ("max_key_bytes = 256", "max_key_bytes = 16"),
        ("max_value_bytes = 65536", "max_value_bytes = 64"),
        ("max_live_keys = 10000", "max_live_keys = 8"),
        ("max_live_bytes = 67108864", "max_live_bytes = 1024"),
    ];
    if profile == Profile::HistoryCapacity {
        limits.push(("max_log_entries = 1000000", "max_log_entries = 100"));
        limits.push(("max_history_bytes = 3221225472", "max_history_bytes = 4096"));
        limits.push((
            "max_journal_bytes = 4294967296",
            "max_journal_bytes = 67120384",
        ));
    }
    let text = limits.iter().fold(LOCAL.to_owned(), |text, (from, to)| {
        assert!(text.contains(from), "rule: local.toml carries {from}");
        text.replace(from, to)
    });
    Config::parse(&text, Path::new("config")).unwrap()
}

/// A scenario of `profile` on the small-limits fixture, run to its end.
fn drive_small(seed: u64, profile: Profile, control: bool) -> (Scenario, Finished) {
    let config = small_config(profile);
    let master = MasterSeed::new(seed);
    let scenario = if control {
        Scenario::control(master, config.key_value_limits(), profile)
    } else {
        Scenario::generate(master, config.key_value_limits(), profile)
    };
    let finished = run(&config, &scenario).unwrap_or_else(|error| {
        panic!(
            "rule: a capacity scenario run reaches its end (§10.2); seed={seed} \
             profile={profile:?} error={error:?}; re-run: ./dev check RUN-PHASES"
        )
    });
    (scenario, finished)
}

#[test]
fn a_live_capacity_heal_writes_over_a_precreated_marker_of_equal_size() {
    for seed in [1, 2] {
        for control in [true, false] {
            let (scenario, finished) = drive_small(seed, Profile::LiveCapacity, control);
            let heal = converged(&finished);
            let marker = heal.marker().unwrap_or_else(|| {
                panic!(
                    "rule: a live-capacity heal still writes a marker (DM-6, D-53); seed={seed} \
                     control={control} heal={heal:?}"
                )
            });
            assert!(
                heal.ticks <= HEAL_BUDGET_TICKS,
                "rule: the heal fits its budget; seed={seed} control={control} ticks={}",
                heal.ticks
            );
            let Completion::Marker(end) = &heal.completion else {
                panic!("rule: a live-capacity heal completes by marker; seed={seed} heal={heal:?}");
            };
            assert!(
                end.pre_created,
                "rule: the marker overwrote a baseline-created value of equal size (DM-6); \
                 seed={seed} control={control} marker={end:?}"
            );
            for end in &heal.nodes {
                let ctl = finished.cluster.controller(end.node).unwrap();
                assert!(
                    ctl.state().history().get(&marker).is_some(),
                    "rule: the marker is applied on every recoverable node; seed={seed} \
                     control={control} node={:?} scenario={:?}",
                    end.node,
                    scenario.seed()
                );
            }
        }
    }
}

#[test]
fn a_history_capacity_heal_drains_and_writes_no_marker() {
    for seed in [1, 2] {
        for control in [true, false] {
            let (_, finished) = drive_small(seed, Profile::HistoryCapacity, control);
            let heal = converged(&finished);
            let Completion::Drain(certified) = &heal.completion else {
                panic!(
                    "rule: a history-capacity heal completes by certified drain, not a marker \
                     (DM-7, D-53); seed={seed} control={control} heal={heal:?}"
                );
            };
            assert_eq!(
                certified.quiet_ticks(),
                QUIET_TICKS,
                "rule: the drain held for the full default quiet period (§10.2); seed={seed} \
                 control={control}"
            );
            assert!(
                heal.marker().is_none(),
                "rule: no marker is written when the history may be full; seed={seed}"
            );
            let first = heal.nodes[0];
            for end in &heal.nodes {
                assert_eq!(
                    end.digest, first.digest,
                    "rule: every recoverable node holds one state after the drain; seed={seed} \
                     control={control} node={:?}",
                    end.node
                );
            }
            if !control {
                let parked = finished
                    .clients
                    .outcomes()
                    .iter()
                    .filter(|outcome| matches!(outcome.end, End::NotExecuted(_) | End::Refused))
                    .count();
                eprintln!("history-capacity seed={seed}: {parked} writes refused or not executed");
            }
            for outcome in finished.clients.outcomes() {
                let resolved = match &outcome.end {
                    End::Done(_) | End::Refused => true,
                    End::NotExecuted(lookups) => lookups.len() == heal.nodes.len(),
                    End::Unresolved(_) => false,
                };
                assert!(
                    resolved,
                    "rule: after the drain every write is done, refused, or not-executed by a \
                     lookup on every recoverable node (D-53); seed={seed} control={control} \
                     outcome={outcome:?}"
                );
            }
        }
    }
}
