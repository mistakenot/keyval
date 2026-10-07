//! W37.2's tests of scenarios and scenario files (PLAN.md §9.1, §9.4, §10.2, §10.4, T-10,
//! A-4): the scenario a seed generates, the healthy envelope it honours by construction, the
//! no-fault control, the file that records it and reads back, and the file reproducing its
//! seed's run exactly.
#![cfg(test)]

// check: SWARM-SCENARIO

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Value, json};
use types::{Config, KeyValueLimits, NodeId};

use crate::clients::{Plan, Profile};
use crate::disk::Survival;
use crate::entropy::MasterSeed;
use crate::nemesis::{CrashPoint, MAX_LOST};
use crate::scenario::{
    EPISODE_MS, HEALTHY_CLIENT, HEALTHY_PEER, LEAD_MS, LinkSpec, SCHEMA, Scenario, ScenarioError,
    Step, Timed, VOTERS, Violation, validate,
};
use crate::scheduler::VirtualTime;
use crate::swarm::{FaultKind, FaultSet, Intensity, Swarm};

const LOCAL: &str = include_str!("../../../config/local.toml");

/// Seeds the generation tests use.
const SEEDS: u64 = 400;

/// Seeds the run tests drive.
const RUNS: u64 = 3;

/// `local.toml` with values of at most 1 KiB, as the client run tests use (a run with 64 KiB
/// values is some 30 times slower in a debug build).
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

fn generated(seed: u64, profile: Profile) -> Scenario {
    Scenario::generate(MasterSeed::new(seed), limits(), profile)
}

fn timed(at_ms: u64, step: Step) -> Timed {
    Timed { at_ms, step }
}

/// A faulty swarm enabling every fault type, so a hand-made schedule is never refused for
/// its type.
fn everything() -> Swarm {
    Swarm::Faulty {
        faults: FaultSet::new(&FaultKind::ALL).unwrap(),
        intensity: Intensity::Medium,
    }
}

fn plan() -> Plan {
    generated(1, Profile::PutGet).plan().clone()
}

fn judge(chaos_ms: u64, faults: &[Timed]) -> Result<(), Violation> {
    validate(everything(), chaos_ms, faults, &plan())
}

#[test]
fn a_seed_decides_its_scenario_and_different_seeds_differ() {
    let mut distinct = BTreeSet::new();
    for seed in 0..SEEDS {
        let first = generated(seed, Profile::Mixed);
        assert_eq!(
            first,
            generated(seed, Profile::Mixed),
            "rule: the same seed generates the same scenario (T-10); seed={seed}"
        );
        assert_eq!(
            first.seed(),
            MasterSeed::new(seed),
            "rule: it records its seed; seed={seed}"
        );
        distinct.insert(first.to_text());
    }
    assert_eq!(
        distinct.len(),
        usize::try_from(SEEDS).unwrap(),
        "rule: no two seeds generate the same scenario file"
    );
}

#[test]
fn generated_scenarios_stay_inside_the_healthy_envelope_by_construction() {
    let nodes: Vec<NodeId> = (1..=VOTERS).map(node).collect();
    for seed in 0..SEEDS {
        for profile in [Profile::PutGet, Profile::Mixed] {
            let scenario = generated(seed, profile);
            assert_eq!(
                validate(
                    scenario.swarm(),
                    scenario.chaos_ms(),
                    scenario.faults(),
                    scenario.plan()
                ),
                Ok(()),
                "rule: a generated scenario passes the §9.4 checks; seed={seed} profile={profile:?}"
            );
            let Swarm::Faulty { faults, intensity } = scenario.swarm() else {
                panic!("rule: a generated swarm is faulty; seed={seed}");
            };
            assert_eq!(scenario.chaos_ms(), intensity.chaos_ms(), "seed={seed}");
            let mut lost = BTreeSet::new();
            let mut previous = 0;
            for step in scenario.faults() {
                assert!(
                    (LEAD_MS..=scenario.chaos_ms()).contains(&step.at_ms) && step.at_ms >= previous,
                    "rule: steps are in time order within the chaos phase after a quiet lead; seed={seed} step={step:?}"
                );
                previous = step.at_ms;
                if let Some(kind) = step.step.kind() {
                    assert!(
                        faults.contains(kind),
                        "rule: only enabled fault types are injected; seed={seed} kind={kind:?} swarm={:?}",
                        scenario.swarm()
                    );
                }
                if let Step::Lose { node } = step.step {
                    lost.insert(node);
                }
            }
            assert!(
                lost.len() <= MAX_LOST && lost.iter().all(|node| nodes.contains(node)),
                "rule: at most {MAX_LOST} voters are lost, so three stay (§9.1, §9.4); seed={seed} lost={lost:?}"
            );
            assert_eq!(
                lost.is_empty(),
                !faults.contains(FaultKind::PermanentLoss),
                "rule: losses happen exactly when permanent loss is enabled; seed={seed}"
            );
        }
    }
}

#[test]
fn every_fault_type_is_scheduled_by_some_seed_and_undone() {
    let mut injected = BTreeSet::new();
    let mut crashes_with_every_survival = BTreeSet::new();
    let mut shapes = BTreeSet::new();
    for seed in 0..SEEDS {
        for step in generated(seed, Profile::PutGet).faults() {
            if let Some(kind) = step.step.kind() {
                injected.insert(kind);
            }
            match &step.step {
                Step::Crash { survival, .. }
                | Step::Arm { survival, .. }
                | Step::CrashAll { survival } => {
                    crashes_with_every_survival.insert(format!("{survival:?}"));
                }
                Step::Partition { groups } => {
                    let mut sizes: Vec<usize> = groups.iter().map(Vec::len).collect();
                    sizes.sort_unstable();
                    shapes.insert(sizes);
                }
                Step::Restart { .. }
                | Step::Lose { .. }
                | Step::Pause { .. }
                | Step::Resume { .. }
                | Step::Cuts { .. }
                | Step::Cut { .. }
                | Step::Mend { .. }
                | Step::Heal
                | Step::Net { .. } => {}
            }
        }
    }
    assert_eq!(
        injected.len(),
        FaultKind::ALL.len(),
        "rule: every §9.1 fault type is scheduled by some seed; injected={injected:?}"
    );
    assert_eq!(
        crashes_with_every_survival.len(),
        5,
        "rule: crashes keep nothing, a prefix, a garbage tail, everything or a seeded part; seen={crashes_with_every_survival:?}"
    );
    assert_eq!(
        shapes,
        BTreeSet::from([vec![1, 2, 2], vec![1, 4], vec![2, 3]]),
        "rule: symmetric partitions come as 3/2, 4/1 and 2/2/1"
    );
    assert!(
        EPISODE_MS.1 > 1_000,
        "rule: an episode can outlast an election timeout, so a paused or cut-off leader is deposed"
    );
}

#[test]
fn the_control_injects_nothing_and_shares_the_seeds_workload() {
    for seed in 0..SEEDS {
        let control = Scenario::control(MasterSeed::new(seed), limits(), Profile::Mixed);
        assert_eq!(control.swarm(), Swarm::Control, "seed={seed}");
        assert!(
            control.faults().is_empty(),
            "rule: the no-fault control schedules nothing (§10.4); seed={seed}"
        );
        assert_eq!(
            control.plan(),
            generated(seed, Profile::Mixed).plan(),
            "rule: the control runs its seed's own workload, so it is the baseline; seed={seed}"
        );
        assert_eq!(
            control.manifest()["swarm"],
            json!({"control": true, "faults": [], "intensity": null}),
            "rule: the manifest records the control; seed={seed}"
        );
    }
}

#[test]
fn the_manifest_records_what_the_run_was() {
    let scenario = generated(11, Profile::PutGet);
    let manifest = scenario.manifest();
    assert_eq!(manifest["seed"], 11);
    assert_eq!(manifest["scenario_schema"], SCHEMA);
    assert_eq!(manifest["workload"], "put_get");
    assert_eq!(manifest["swarm"], scenario.swarm().manifest());
    assert_eq!(manifest["chaos_ms"], scenario.chaos_ms());
    assert_eq!(manifest["scheduled_steps"], scenario.faults().len());
    let operations: usize = scenario.plan().clients.iter().map(|c| c.ops.len()).sum();
    assert_eq!(manifest["planned_operations"], operations);
}

#[test]
fn a_scenario_file_reads_back_to_the_scenario() {
    for seed in 0..SEEDS {
        for profile in [Profile::PutGet, Profile::Mixed] {
            let scenario = generated(seed, profile);
            let text = scenario.to_text();
            assert!(
                text.ends_with('\n'),
                "rule: the file ends in a newline; seed={seed}"
            );
            assert!(
                text.lines().count() > 20,
                "rule: the file is pretty-printed for a human to read; seed={seed}"
            );
            let back = Scenario::parse(&text, limits());
            assert_eq!(
                back.as_ref().ok(),
                Some(&scenario),
                "rule: a scenario file reads back to its scenario (T-10); seed={seed} profile={profile:?} verdict={back:?}"
            );
            assert_eq!(
                back.unwrap().to_text(),
                text,
                "rule: and back to the same text; seed={seed}"
            );
        }
    }
}

#[test]
fn a_control_file_and_the_local_limits_read_back_too() {
    let local = Config::parse(LOCAL, Path::new("config"))
        .unwrap()
        .key_value_limits();
    for seed in 0..20 {
        for scenario in [
            Scenario::control(MasterSeed::new(seed), local, Profile::Mixed),
            Scenario::generate(MasterSeed::new(seed), local, Profile::Mixed),
        ] {
            assert_eq!(
                Scenario::parse(&scenario.to_text(), local).ok().as_ref(),
                Some(&scenario),
                "rule: files with values up to the local limit read back; seed={seed}"
            );
        }
    }
}

/// `text` of `scenario` with `edit` applied to its JSON.
fn edited(scenario: &Scenario, edit: impl FnOnce(&mut Value)) -> String {
    let mut value = scenario.to_json();
    edit(&mut value);
    value.to_string()
}

/// A seed's scenario that has a crash episode, so edits to its faults have something to bite.
fn with_crash() -> Scenario {
    (0..SEEDS)
        .map(|seed| generated(seed, Profile::PutGet))
        .find(|scenario| {
            scenario.swarm().enables(FaultKind::CrashRestart)
                && scenario
                    .faults()
                    .iter()
                    .any(|step| matches!(step.step, Step::Crash { .. }))
        })
        .expect("some seed schedules a crash")
}

fn is_invalid(
    verdict: Result<Scenario, ScenarioError>,
    wanted: impl FnOnce(&Violation) -> bool,
    what: &str,
) {
    match verdict {
        Err(ScenarioError::Invalid(violation)) if wanted(&violation) => {}
        other => panic!("rule: {what} is scenario invalid (§10.16); verdict={other:?}"),
    }
}

fn is_malformed(verdict: &Result<Scenario, ScenarioError>, what: &str) {
    assert!(
        matches!(verdict, Err(ScenarioError::Malformed(_))),
        "rule: {what} is a malformed file; verdict={verdict:?}"
    );
}

/// The first write of the plan, as a JSON pointer.
fn first_put(value: &Value) -> String {
    for (c, client) in value["plan"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        for (o, op) in client["ops"].as_array().unwrap().iter().enumerate() {
            if op["op"] == "put" {
                return format!("/plan/clients/{c}/ops/{o}");
            }
        }
    }
    panic!("fixture: the plan has a PUT");
}

#[test]
fn a_file_that_is_not_a_scenario_is_refused() {
    let base = generated(3, Profile::Mixed);
    let long_key = json!("k".repeat(257));
    let crash = |extra: Value| {
        let mut step = json!({"at_ms": 300, "fault": "crash"});
        step.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!([step])
    };
    let put = first_put(&base.to_json());
    let edits = [
        (
            "a wrong schema",
            "/schema".to_owned(),
            json!("keyval.scenario/v0"),
        ),
        (
            "an unknown workload",
            "/workload".to_owned(),
            json!("chaos"),
        ),
        ("a negative seed", "/seed".to_owned(), json!(-1)),
        (
            "a fault of an unknown kind",
            "/faults".to_owned(),
            json!([{"at_ms": 300, "fault": "meteor"}]),
        ),
        (
            "a crash that names no node",
            "/faults".to_owned(),
            crash(json!({"survival": "all"})),
        ),
        (
            "a crash of node zero",
            "/faults".to_owned(),
            crash(json!({"node": 0, "survival": "all"})),
        ),
        (
            "an unknown survival",
            "/faults".to_owned(),
            crash(json!({"node": 1, "survival": "most"})),
        ),
        (
            "a key longer than the limit",
            "/plan/keys/0".to_owned(),
            long_key,
        ),
        ("an empty key", "/plan/keys/0".to_owned(), json!("")),
        (
            "a duplicate key",
            "/plan/keys/1".to_owned(),
            base.to_json()["plan"]["keys"][0].clone(),
        ),
        (
            "three keys",
            "/plan/keys".to_owned(),
            json!(["a", "b", "c"]),
        ),
        ("no hot keys", "/plan/hot".to_owned(), json!(0)),
        ("more hot keys than keys", "/plan/hot".to_owned(), json!(99)),
        (
            "a value over the limit",
            format!("{put}/value"),
            json!("v".repeat(1025)),
        ),
        (
            "a request ID that is not hex",
            format!("{put}/request_id"),
            json!("XYZ"),
        ),
        ("an unknown operation", format!("{put}/op"), json!("rename")),
        (
            "a client out of order",
            "/plan/clients/0/client".to_owned(),
            json!(9),
        ),
    ];
    for (what, pointer, replacement) in edits {
        let text = edited(&base, |value| {
            *value.pointer_mut(&pointer).unwrap() = replacement;
        });
        is_malformed(&Scenario::parse(&text, limits()), what);
    }
    let text = edited(&base, |value| {
        value.as_object_mut().unwrap().remove("chaos_ms");
    });
    is_malformed(&Scenario::parse(&text, limits()), "a missing chaos length");
    is_malformed(
        &Scenario::parse("not json at all", limits()),
        "text that is not JSON",
    );
    is_malformed(&Scenario::parse("[]", limits()), "a JSON array");
}
#[test]
fn a_request_id_used_twice_is_refused() {
    let base = generated(5, Profile::PutGet);
    let text = edited(&base, |value| {
        let mut ids = Vec::new();
        for client in value["plan"]["clients"].as_array().unwrap() {
            for op in client["ops"].as_array().unwrap() {
                if op["op"] == "put" {
                    ids.push(op["request_id"].clone());
                }
            }
        }
        assert!(
            ids.len() >= 2,
            "fixture: the plan has two writes; ids={ids:?}"
        );
        let mut seen = 0;
        for client in value["plan"]["clients"].as_array_mut().unwrap() {
            for op in client["ops"].as_array_mut().unwrap() {
                if op["op"] == "put" {
                    op["request_id"] = ids[0].clone();
                    seen += 1;
                }
            }
        }
        assert!(seen >= 2);
    });
    is_malformed(
        &Scenario::parse(&text, limits()),
        "two writes sharing a request ID",
    );
}

#[test]
fn a_file_outside_the_healthy_envelope_is_scenario_invalid() {
    let crash = with_crash();
    let crash_at = crash
        .faults()
        .iter()
        .position(|step| matches!(step.step, Step::Crash { .. }))
        .unwrap();

    // The crash's restart removed: the node is left down when chaos ends.
    let text = edited(&crash, |value| {
        let faults = value["faults"].as_array_mut().unwrap();
        faults.retain(|step| step["fault"] != "restart");
    });
    is_invalid(
        Scenario::parse(&text, limits()),
        |v| matches!(v, Violation::NodeLeftDown { .. }),
        "a crash never restarted",
    );

    // A step after chaos.
    let text = edited(&crash, |value| {
        let chaos = value["chaos_ms"].as_u64().unwrap();
        value["faults"]
            .as_array_mut()
            .unwrap()
            .push(json!({"at_ms": chaos + 1, "fault": "heal"}));
    });
    is_invalid(
        Scenario::parse(&text, limits()),
        |v| matches!(v, Violation::AfterChaos { .. }),
        "a step after the chaos phase",
    );

    // Steps out of time order.
    let text = edited(&crash, |value| {
        value["faults"].as_array_mut().unwrap().reverse();
    });
    if crash.faults().len() > 1
        && crash.faults().first().map(|s| s.at_ms) != crash.faults().last().map(|s| s.at_ms)
    {
        is_invalid(
            Scenario::parse(&text, limits()),
            |v| matches!(v, Violation::OutOfOrder { .. }),
            "steps out of time order",
        );
    }

    // A fault type the swarm did not enable: the crash's swarm without crashes.
    let text = edited(&crash, |value| {
        let others: Vec<Value> = value["swarm"]["faults"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|name| **name != "crash_restart")
            .cloned()
            .collect();
        value["swarm"]["faults"] = if others.is_empty() {
            json!(["pause"])
        } else {
            json!(others)
        };
    });
    is_invalid(
        Scenario::parse(&text, limits()),
        |v| {
            matches!(
                v,
                Violation::DisabledFault {
                    kind: FaultKind::CrashRestart,
                    ..
                }
            )
        },
        "a crash under a swarm that does not enable crashes",
    );

    // The control with a fault in it.
    let text = edited(&crash, |value| {
        value["swarm"] = json!({"control": true, "faults": [], "intensity": null});
    });
    is_invalid(
        Scenario::parse(&text, limits()),
        |v| matches!(v, Violation::ControlInjects { .. }),
        "a fault in the no-fault control",
    );
    assert!(crash_at < crash.faults().len());
}

fn crash(n: u64, at: u64) -> Timed {
    timed(
        at,
        Step::Crash {
            node: node(n),
            survival: Survival::All,
        },
    )
}

fn restart(n: u64, at: u64) -> Timed {
    timed(at, Step::Restart { node: node(n) })
}

fn lose(n: u64, at: u64) -> Timed {
    timed(at, Step::Lose { node: node(n) })
}

fn pause(n: u64, at: u64) -> Timed {
    timed(at, Step::Pause { node: node(n) })
}

fn resume(n: u64, at: u64) -> Timed {
    timed(at, Step::Resume { node: node(n) })
}

fn net(peer: LinkSpec, client: LinkSpec, at: u64) -> Timed {
    timed(at, Step::Net { peer, client })
}

#[test]
fn nodes_must_be_back_and_unpaused_when_chaos_ends() {
    assert_eq!(
        judge(3_000, &[]),
        Ok(()),
        "rule: nothing scheduled is healthy"
    );
    assert_eq!(judge(3_000, &[crash(2, 300), restart(2, 900)]), Ok(()));
    assert_eq!(
        judge(3_000, &[crash(2, 300)]),
        Err(Violation::NodeLeftDown { node: 2 }),
        "rule: a node still down when chaos ends is outside the envelope"
    );
    assert_eq!(
        judge(3_000, &[pause(3, 300)]),
        Err(Violation::NodeLeftPaused { node: 3 }),
        "rule: a node still paused when chaos ends is outside the envelope"
    );
    assert_eq!(
        judge(3_000, &[pause(3, 300), crash(3, 400), restart(3, 600)]),
        Ok(()),
        "rule: a crash clears a pause, as the cluster does, so the resume is not needed"
    );
    assert_eq!(judge(3_000, &[pause(3, 300), resume(3, 800)]), Ok(()));
    assert_eq!(
        judge(3_000, &[restart(1, 300), resume(1, 400)]),
        Ok(()),
        "rule: a restart of a running node and a resume of an unpaused one change nothing"
    );

    assert_eq!(
        judge(3_000, &[lose(1, 300), lose(2, 400)]),
        Ok(()),
        "rule: two losses leave three voters"
    );
    assert_eq!(
        judge(3_000, &[lose(1, 300), lose(2, 400), lose(3, 500)]),
        Err(Violation::TooManyLost { lost: 3 }),
        "rule: a third loss leaves fewer than three voters (§9.1)"
    );
    assert_eq!(
        judge(3_000, &[lose(1, 300), restart(1, 400)]),
        Ok(()),
        "rule: a lost node never returns and is not required to"
    );
    assert_eq!(
        judge(3_000, &[crash(4, 300), lose(4, 400), restart(4, 500)]),
        Ok(()),
        "rule: a lost node that was down is gone, not left down"
    );
}

#[test]
fn partitions_and_cuts_must_be_healed_when_chaos_ends() {
    let part = |at| {
        timed(
            at,
            Step::Partition {
                groups: vec![vec![node(1), node(2), node(3)], vec![node(4), node(5)]],
            },
        )
    };
    assert_eq!(judge(3_000, &[part(300), timed(900, Step::Heal)]), Ok(()));
    assert_eq!(
        judge(3_000, &[part(300)]),
        Err(Violation::LinksLeftCut { links: 12 }),
        "rule: a 3/2 partition cuts twelve directed links and leaves them cut"
    );
    let cut = timed(
        300,
        Step::Cut {
            from: node(1),
            to: node(2),
        },
    );
    let mend = timed(
        800,
        Step::Mend {
            from: node(1),
            to: node(2),
        },
    );
    assert_eq!(judge(3_000, &[cut.clone(), mend]), Ok(()));
    assert_eq!(
        judge(3_000, &[cut]),
        Err(Violation::LinksLeftCut { links: 1 })
    );
}

#[test]
fn links_must_be_healthy_and_voters_must_exist() {
    let slow = LinkSpec {
        max_delay_us: 400_000,
        ..HEALTHY_PEER
    };
    assert_eq!(
        judge(3_000, &[net(slow, HEALTHY_CLIENT, 300)]),
        Err(Violation::NetworkNotHealed),
        "rule: links slower than 50 ms are not healthy (§9.4)"
    );
    let edge = LinkSpec {
        max_delay_us: 50_000,
        ..HEALTHY_PEER
    };
    let over = LinkSpec {
        max_delay_us: 50_001,
        ..HEALTHY_PEER
    };
    assert_eq!(
        judge(3_000, &[net(edge, HEALTHY_CLIENT, 300)]),
        Ok(()),
        "rule: 50 ms is healthy"
    );
    assert_eq!(
        judge(3_000, &[net(over, HEALTHY_CLIENT, 300)]),
        Err(Violation::NetworkNotHealed)
    );
    let lossy = LinkSpec {
        loss_ppm: 1,
        ..HEALTHY_CLIENT
    };
    assert_eq!(
        judge(3_000, &[net(HEALTHY_PEER, lossy, 300)]),
        Err(Violation::NetworkNotHealed),
        "rule: a link that loses anything is not healthy"
    );
    assert_eq!(
        judge(
            3_000,
            &[
                net(slow, lossy, 300),
                net(HEALTHY_PEER, HEALTHY_CLIENT, 900)
            ]
        ),
        Ok(()),
        "rule: restoring the healthy profile ends the episode"
    );

    assert_eq!(
        judge(3_000, &[crash(6, 300)]),
        Err(Violation::UnknownNode { node: 6 }),
        "rule: only voters 1 to 5 exist"
    );
    assert_eq!(
        judge(
            3_000,
            &[timed(
                300,
                Step::Partition {
                    groups: vec![vec![node(1)], vec![node(1)]]
                }
            )]
        )
        .map_err(|v| matches!(v, Violation::BadStep { .. })),
        Err(true),
        "rule: a partition whose groups overlap cannot be built"
    );
}

#[test]
fn the_control_and_a_disabled_type_refuse_every_fault_but_not_a_quiet_schedule() {
    let swarm = Swarm::Faulty {
        faults: FaultSet::new(&[FaultKind::Pause]).unwrap(),
        intensity: Intensity::Low,
    };
    let steps = [
        timed(300, Step::Pause { node: node(1) }),
        timed(800, Step::Resume { node: node(1) }),
    ];
    assert_eq!(validate(swarm, 3_000, &steps, &plan()), Ok(()));
    assert_eq!(
        validate(Swarm::Control, 3_000, &steps, &plan()),
        Err(Violation::ControlInjects { at_ms: 300 })
    );
    assert_eq!(
        validate(Swarm::Control, 3_000, &[], &plan()),
        Ok(()),
        "rule: the control with nothing scheduled is valid"
    );
    let crash = [
        timed(
            300,
            Step::Crash {
                node: node(1),
                survival: Survival::All,
            },
        ),
        timed(900, Step::Restart { node: node(1) }),
    ];
    assert_eq!(
        validate(swarm, 3_000, &crash, &plan()),
        Err(Violation::DisabledFault {
            at_ms: 300,
            kind: FaultKind::CrashRestart
        })
    );
}

/// The state a scenario's run ends in: the trace hash after the chaos phase, a heal and a
/// 30 virtual second budget (§9.4), with the clients driven to the end of it.
fn run(scenario: &Scenario) -> (crate::cluster::Cluster, crate::clients::Clients) {
    let config = config();
    let (mut cluster, mut clients) = scenario.start(&config).unwrap();
    let until = VirtualTime::from_micros(scenario.chaos_end().as_micros() + 30_000_000);
    clients.run(&mut cluster, until).unwrap();
    (cluster, clients)
}

#[test]
fn start_schedules_the_faults_and_a_run_ends_with_three_voters_up() {
    for seed in 0..RUNS {
        let scenario = generated(seed, Profile::PutGet);
        let (cluster, _clients) = run(&scenario);
        let lost: Vec<NodeId> = (1..=VOTERS)
            .map(node)
            .filter(|n| cluster.is_lost(*n))
            .collect();
        let scheduled_losses = scenario
            .faults()
            .iter()
            .filter(|step| matches!(step.step, Step::Lose { .. }))
            .count();
        assert_eq!(
            lost.len(),
            scheduled_losses,
            "rule: exactly the scheduled losses happened; seed={seed} lost={lost:?}"
        );
        assert!(
            (1..=VOTERS).map(node).all(|n| !cluster.is_paused(n)),
            "rule: no node is left paused after chaos; seed={seed}"
        );
        if scenario.faults().is_empty() {
            assert_eq!(cluster.faults().count(), 0, "seed={seed}");
        }
    }
}

#[test]
fn a_scenario_file_reproduces_its_seeds_run_exactly() {
    for seed in 0..RUNS {
        let scenario = generated(seed, Profile::PutGet);
        let from_file = Scenario::parse(&scenario.to_text(), limits()).unwrap();
        let (original, _) = run(&scenario);
        let (replayed, _) = run(&from_file);
        assert_eq!(
            replayed.trace().hash(),
            original.trace().hash(),
            "rule: the scenario file reproduces its seed's run exactly (T-10, A-4); seed={seed}; the first of the two traces differing is the file's"
        );
        let (again, _) = run(&scenario);
        assert_eq!(
            again.trace().hash(),
            original.trace().hash(),
            "rule: and so does the seed again (P4); seed={seed}"
        );
    }
}

#[test]
fn the_control_run_differs_from_a_faulty_run_of_the_same_seed() {
    let seed = (0..SEEDS)
        .find(|seed| !generated(*seed, Profile::PutGet).faults().is_empty())
        .expect("some seed injects");
    let faulty = generated(seed, Profile::PutGet);
    let control = Scenario::control(MasterSeed::new(seed), limits(), Profile::PutGet);
    let (with_faults, _) = run(&faulty);
    let (without, _) = run(&control);
    assert_ne!(
        with_faults.trace().hash(),
        without.trace().hash(),
        "rule: faults change the run; seed={seed} faults={}",
        faulty.faults().len()
    );
    assert_eq!(
        without.faults().count(),
        0,
        "rule: the control injects nothing; seed={seed}"
    );
}

#[test]
fn the_local_config_starts_a_scenario_with_a_healthy_network() {
    let text = LOCAL.replace("max_value_bytes = 65536", "max_value_bytes = 1024");
    let config = Config::parse(&text, Path::new("config")).unwrap();
    let voters: Vec<u64> = config.voters().map(NodeId::get).collect();
    assert_eq!(
        voters,
        (1..=VOTERS).collect::<Vec<_>>(),
        "fixture: the local config has voters 1 to 5"
    );
    assert!(generated(0, Profile::PutGet).start(&config).is_ok());
    assert!(
        Scenario::net_config(&config).is_ok(),
        "rule: the healthy starting network builds"
    );
    assert!(HEALTHY_PEER.is_healthy() && HEALTHY_CLIENT.is_healthy());
}

#[test]
fn simultaneous_steps_that_could_disagree_are_scenario_invalid() {
    let conflicting: [(&str, [Timed; 2]); 6] = [
        (
            "a crash and a restart of one node",
            [crash(2, 1_000), restart(2, 1_000)],
        ),
        (
            "a restart and a crash of one node",
            [restart(2, 1_000), crash(2, 1_000)],
        ),
        (
            "a pause and a resume of one node",
            [pause(2, 1_000), resume(2, 1_000)],
        ),
        (
            "a loss and a crash of one node",
            [lose(2, 1_000), crash(2, 1_000)],
        ),
        (
            "two network changes",
            [
                net(HEALTHY_PEER, HEALTHY_CLIENT, 1_000),
                net(
                    LinkSpec {
                        loss_ppm: 1,
                        ..HEALTHY_PEER
                    },
                    HEALTHY_CLIENT,
                    1_000,
                ),
            ],
        ),
        (
            "a cut and a heal",
            [
                timed(
                    1_000,
                    Step::Cut {
                        from: node(1),
                        to: node(2),
                    },
                ),
                timed(1_000, Step::Heal),
            ],
        ),
    ];
    for (what, pair) in conflicting {
        let schedule = pair.to_vec();
        let verdict = judge(3_000, &schedule);
        assert_eq!(
            verdict,
            Err(Violation::SimultaneousSteps { at_ms: 1_000 }),
            "rule: {what} at one instant is refused, since the scheduler's tie-break, not the file's order, decides it (§10.16); schedule={schedule:?}"
        );
    }
    let commuting = [
        restart(2, 1_000),
        resume(2, 1_000),
        restart(2, 1_000),
        timed(1_000, Step::Heal),
    ];
    let mut schedule = vec![pause(2, 400), crash(2, 500)];
    schedule.extend(commuting);
    assert_eq!(
        judge(3_000, &schedule),
        Ok(()),
        "rule: recoveries commute, so they may share an instant; schedule={schedule:?}"
    );
}

#[test]
fn simultaneous_recoveries_end_in_one_state_whatever_their_order() {
    let schedules = [
        vec![
            pause(2, 300),
            crash(2, 400),
            restart(2, 1_000),
            resume(2, 1_000),
        ],
        vec![
            pause(2, 300),
            crash(2, 400),
            resume(2, 1_000),
            restart(2, 1_000),
        ],
    ];
    for seed in 0..20 {
        for schedule in &schedules {
            let scenario = Scenario::from_parts(
                seed,
                everything(),
                Profile::PutGet,
                3_000,
                schedule.clone(),
                plan(),
            )
            .unwrap();
            let (mut cluster, _) = scenario.start(&config()).unwrap();
            cluster.run_until(scenario.chaos_end()).unwrap();
            assert!(
                cluster.controller(node(2)).is_some() && !cluster.is_paused(node(2)),
                "rule: node 2 is running and not paused whatever the tie order; seed={seed} schedule={schedule:?}"
            );
        }
    }
}

#[test]
fn every_generated_scenario_ends_chaos_as_its_replay_says() {
    for seed in 0..150 {
        let scenario = generated(
            seed,
            if seed % 2 == 0 {
                Profile::PutGet
            } else {
                Profile::Mixed
            },
        );
        let (mut cluster, _) = scenario.start(&config()).unwrap();
        cluster.run_until(scenario.chaos_end()).unwrap();
        let lost: BTreeSet<u64> = scenario
            .faults()
            .iter()
            .filter_map(|timed| match timed.step {
                Step::Lose { node } => Some(node.get()),
                _ => None,
            })
            .collect();
        for raw in 1..=VOTERS {
            let n = node(raw);
            assert_eq!(
                cluster.is_lost(n),
                lost.contains(&raw),
                "rule: a node is lost exactly when a step loses it; seed={seed} node={raw}"
            );
            assert!(
                cluster.is_lost(n) || (cluster.controller(n).is_some() && !cluster.is_paused(n)),
                "rule: every surviving node is running and not paused when chaos ends (§9.4); seed={seed} node={raw}"
            );
        }
        let up = (1..=VOTERS)
            .filter(|raw| cluster.controller(node(*raw)).is_some())
            .count();
        assert!(
            up >= 3,
            "rule: at least three voters are up; seed={seed} up={up}"
        );
    }
}

#[test]
fn network_episodes_do_not_overlap_and_no_step_ties_a_conflicting_one() {
    for seed in 0..SEEDS {
        let scenario = generated(seed, Profile::Mixed);
        let mut open = false;
        for step in scenario.faults() {
            if let Step::Net { peer, client } = &step.step {
                let healthy = *peer == HEALTHY_PEER && *client == HEALTHY_CLIENT;
                assert_ne!(
                    open, !healthy,
                    "rule: a network episode opens only when none is open, and closes what it opened; seed={seed} at={}",
                    step.at_ms
                );
                open = !healthy;
            }
        }
        assert!(!open, "rule: no network episode is left open; seed={seed}");
        assert!(
            scenario
                .faults()
                .iter()
                .all(|step| step.at_ms < scenario.chaos_ms()),
            "rule: every step is before chaos ends, leaving a margin to settle; seed={seed}"
        );
    }
}

#[test]
fn a_file_cannot_ask_for_a_chaos_phase_or_schedule_beyond_the_bounds() {
    let base = generated(3, Profile::PutGet);
    for chaos_ms in [10_001, u64::MAX / 100, u64::MAX] {
        let text = edited(&base, |value| value["chaos_ms"] = json!(chaos_ms));
        is_invalid(
            Scenario::parse(&text, limits()),
            |violation| matches!(violation, Violation::ChaosTooLong { .. }),
            &format!("a chaos phase of {chaos_ms} ms"),
        );
    }
    let many = vec![pause(1, 300); crate::scenario::MAX_STEPS + 1];
    assert!(
        matches!(judge(3_000, &many), Err(Violation::TooManySteps { .. })),
        "rule: more than {} steps are refused",
        crate::scenario::MAX_STEPS
    );
    assert_eq!(
        judge(3_000, &[crash(2, 300), restart(2, 3_000)]),
        Err(Violation::AfterChaos {
            at_ms: 3_000,
            chaos_ms: 3_000
        }),
        "rule: a step at the end of chaos leaves no margin to settle"
    );
}

/// An edit of a scenario file's JSON.
type PlanEdit<'a> = Box<dyn Fn(&mut Value) + 'a>;

#[test]
fn a_file_cannot_hold_a_plan_clients_would_never_generate() {
    let base = generated(3, Profile::Mixed);
    let key = base.plan().keys[0].as_str().to_owned();
    let edits: [(&str, PlanEdit<'_>); 6] = [
        ("no clients", Box::new(|v| v["plan"]["clients"] = json!([]))),
        (
            "a client with no operations",
            Box::new(|v| v["plan"]["clients"][0]["ops"] = json!([])),
        ),
        (
            "a client with 21 operations",
            Box::new(|v| {
                let op = v["plan"]["clients"][0]["ops"][0].clone();
                let ops: Vec<Value> = (0..21)
                    .map(|n| {
                        let mut op = op.clone();
                        if op.get("request_id").is_some() {
                            op["request_id"] = json!(format!("{n:032x}"));
                        }
                        op
                    })
                    .collect();
                v["plan"]["clients"][0]["ops"] = json!(ops);
            }),
        ),
        (
            "a think time of 301 ms",
            Box::new(|v| v["plan"]["clients"][0]["ops"][0]["think_ms"] = json!(301)),
        ),
        (
            "a think time that would overflow",
            Box::new(|v| v["plan"]["clients"][0]["ops"][0]["think_ms"] = json!(u64::MAX)),
        ),
        (
            "an operation on a key outside the plan",
            Box::new(|v| {
                let ops = v["plan"]["clients"][0]["ops"].as_array_mut().unwrap();
                let at = ops.iter().position(|op| op.get("key").is_some()).unwrap();
                ops[at]["key"] = json!(format!("{key}-not-in-the-plan"));
            }),
        ),
    ];
    for (what, edit) in edits {
        let text = edited(&base, |value| edit(value));
        is_invalid(
            Scenario::parse(&text, limits()),
            |violation| matches!(violation, Violation::BadPlan { .. }),
            what,
        );
    }
}

#[test]
fn a_misspelt_or_unknown_field_is_refused_at_every_level() {
    let base = (0..SEEDS)
        .map(|seed| generated(seed, Profile::Mixed))
        .find(|scenario| {
            scenario
                .faults()
                .iter()
                .any(|step| matches!(step.step, Step::Net { .. }))
        })
        .expect("some seed changes the network");
    let put = first_put(&base.to_json());
    let net_at = base
        .faults()
        .iter()
        .position(|step| matches!(step.step, Step::Net { .. }))
        .unwrap();
    let places = [
        String::new(),
        "/swarm".to_owned(),
        "/plan".to_owned(),
        "/plan/clients/0".to_owned(),
        put,
        format!("/faults/{net_at}"),
        format!("/faults/{net_at}/peer"),
    ];
    for place in places {
        let text = edited(&base, |value| {
            let target = if place.is_empty() {
                &mut *value
            } else {
                value.pointer_mut(&place).unwrap()
            };
            target["surviva1"] = json!("all");
        });
        is_malformed(
            &Scenario::parse(&text, limits()),
            &format!("an unknown field at {place:?}"),
        );
    }
}

#[test]
fn a_scan_range_and_page_size_survive_the_file() {
    use crate::clients::{ClientPlan, Planned};
    use crate::trace::ClientId;
    use types::{PageSize, ScanRange};
    let base = generated(3, Profile::Mixed);
    let keys = base.plan().keys.clone();
    let range = ScanRange::new(
        Some(keys[0].clone().min(keys[1].clone())),
        Some(keys[0].clone().max(keys[1].clone())),
        Some(keys[0].clone().min(keys[1].clone())),
        PageSize::new(7).unwrap(),
    )
    .unwrap();
    let mut plan = base.plan().clone();
    plan.clients = vec![ClientPlan {
        client: ClientId::new(0),
        ops: (0..6)
            .map(|_| Planned {
                intent: crate::clients::Intent::Scan {
                    range: range.clone(),
                },
                think_ms: 5,
            })
            .collect(),
    }];
    let scenario =
        Scenario::from_parts(3, Swarm::Control, Profile::Mixed, 3_000, vec![], plan).unwrap();
    let read = Scenario::parse(&scenario.to_text(), limits()).unwrap();
    assert_eq!(
        read, scenario,
        "rule: a scan's bounds and page size are in the file, so it reproduces the run"
    );
}

fn arm(n: u64, point: CrashPoint, at: u64) -> Timed {
    timed(
        at,
        Step::Arm {
            node: node(n),
            point,
            survival: Survival::Nothing,
        },
    )
}

#[test]
fn every_crash_point_is_armed_by_some_seed_and_survives_the_file() {
    let mut points = BTreeSet::new();
    for seed in 0..SEEDS {
        let scenario = generated(seed, Profile::PutGet);
        let mut armed = false;
        for step in scenario.faults() {
            if let Step::Arm { point, .. } = step.step {
                points.insert(format!("{point:?}"));
                armed = true;
            }
        }
        if armed {
            let read = Scenario::parse(&scenario.to_text(), limits()).unwrap();
            assert_eq!(
                read, scenario,
                "rule: an armed crash is in the file and reads back (T-10); seed={seed}"
            );
        }
    }
    assert_eq!(
        points.len(),
        3,
        "rule: swarm campaigns arm BeforeSync, AfterSync and BeforeReply (§9.1); seen={points:?}"
    );
}

#[test]
fn an_arm_step_is_judged_like_a_crash() {
    assert_eq!(
        judge(
            10_000,
            &[arm(2, CrashPoint::AfterSync, 1_000), restart(2, 5_000)]
        ),
        Ok(()),
        "rule: an armed crash with its restart is inside the envelope"
    );
    assert_eq!(
        judge(10_000, &[arm(9, CrashPoint::BeforeSync, 1_000)]),
        Err(Violation::UnknownNode { node: 9 }),
        "rule: arming a node that is not a voter is refused"
    );
    let only_pauses = Swarm::Faulty {
        faults: FaultSet::new(&[FaultKind::Pause]).unwrap(),
        intensity: Intensity::Low,
    };
    assert!(
        matches!(
            validate(
                only_pauses,
                10_000,
                &[arm(2, CrashPoint::BeforeReply, 1_000)],
                &plan()
            ),
            Err(Violation::DisabledFault {
                kind: FaultKind::CrashRestart,
                ..
            })
        ),
        "rule: an armed crash is a crash-restart fault, so a swarm without it refuses it"
    );
    assert!(
        matches!(
            validate(
                Swarm::Control,
                10_000,
                &[arm(2, CrashPoint::BeforeReply, 1_000)],
                &plan()
            ),
            Err(Violation::ControlInjects { .. })
        ),
        "rule: the control injects nothing, armed crashes included"
    );
}

#[test]
fn an_armed_crash_fires_and_its_file_reproduces_the_run() {
    let faults: Vec<Timed> = (1..=5)
        .map(|n| arm(n, CrashPoint::BeforeSync, 1_000))
        .chain((1..=5).map(|n| restart(n, 8_000)))
        .collect();
    let scenario =
        Scenario::from_parts(1, everything(), Profile::PutGet, 10_000, faults, plan()).unwrap();
    let from_file = Scenario::parse(&scenario.to_text(), limits()).unwrap();
    assert_eq!(
        from_file, scenario,
        "rule: the arming steps are in the file"
    );
    let (original, _) = run(&scenario);
    let (replayed, _) = run(&from_file);
    let mut crashed = false;
    for (_, fault) in original.faults() {
        if let crate::nemesis::Fault::Crash { .. } = fault {
            crashed = true;
        }
    }
    assert!(
        crashed,
        "rule: an armed crash fires from the scenario's own step and logs a crash"
    );
    assert_eq!(
        replayed.trace().hash(),
        original.trace().hash(),
        "rule: a file with armed crashes reproduces its run's trace hash (T-10)"
    );
}

#[test]
fn an_armed_crash_must_be_closed_by_a_restart_and_a_bad_arm_line_is_malformed() {
    assert_eq!(
        judge(10_000, &[arm(2, CrashPoint::AfterSync, 1_000)]),
        Err(Violation::NodeLeftDown { node: 2 }),
        "rule: an armed crash may have fired, so chaos cannot end with it unclosed"
    );
    assert_eq!(
        judge(
            10_000,
            &[
                arm(2, CrashPoint::AfterSync, 1_000),
                restart(2, 3_000),
                arm(2, CrashPoint::BeforeSync, 4_000),
            ]
        ),
        Err(Violation::NodeLeftDown { node: 2 }),
        "rule: a restart closes only the arming before it"
    );
    let scenario = Scenario::from_parts(
        1,
        everything(),
        Profile::PutGet,
        10_000,
        vec![arm(2, CrashPoint::BeforeReply, 1_000), restart(2, 5_000)],
        plan(),
    )
    .unwrap();
    let text = scenario.to_text();
    let first_fault = |mutate: &dyn Fn(&mut Value)| {
        let mut value: Value = serde_json::from_str(&text).unwrap();
        mutate(&mut value["faults"][0]);
        Scenario::parse(&value.to_string(), limits())
    };
    is_malformed(
        &first_fault(&|f| f["point"] = Value::from("after_lunch")),
        "an unknown crash point",
    );
    is_malformed(
        &first_fault(&|f| {
            f.as_object_mut().unwrap().remove("point");
        }),
        "an arm without its point",
    );
    is_malformed(
        &first_fault(&|f| f["extra"] = Value::from(1)),
        "an arm with an unknown key",
    );
}
