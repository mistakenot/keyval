//! W36.2's tests of the observation recording (PLAN.md §10.2, §10.3): a real PUT/GET run's
//! document validates against `schemas/observation/observation.v1.schema.json`, keeps the
//! schema's stream rules, records the misuse stream as flagged attempts that are never
//! logical writes and that the system answers `request_id_conflict`, and a seed decides the
//! document byte for byte.
#![cfg(test)]

// check: SIM-OBSERVE

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;
use types::Config;

use crate::clients::{Clients, Plan, Profile};
use crate::cluster::Cluster;
use crate::entropy::{MasterSeed, Subsystem};
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::observe::{self, Mark, Meta, ObserveError, Phases};
use crate::scheduler::{EventBudget, VirtualTime};

const LOCAL: &str = include_str!("../../../config/local.toml");

const SCHEMA: &str = include_str!("../../../schemas/observation/observation.v1.schema.json");

/// The virtual seconds a run may take.
const RUN_SECONDS: u64 = 240;

/// Seeds the document tests drive.
const SEEDS: u64 = 4;

/// Every write that ends with a terminal result is followed by a misuse attempt.
const ALWAYS: u32 = 1_000_000;

/// `local.toml` with values of at most 1 KiB, as the W36.1 run tests use.
fn run_config() -> Config {
    Config::parse(
        &LOCAL.replace("max_value_bytes = 65536", "max_value_bytes = 1024"),
        Path::new("config"),
    )
    .unwrap()
}

/// One run of the PUT/GET clients for `seed`, with `misuse` ppm of misuse, over client paths
/// that lose 5% and duplicate 2% of messages; and its document.
fn run(seed: u64, misuse: u32) -> (Clients, Cluster, Config, Meta) {
    let config = run_config();
    let master = MasterSeed::new(seed);
    let client = LinkProfile::new(
        200,
        2_000,
        Ppm::new(50_000).unwrap(),
        Ppm::new(20_000).unwrap(),
    )
    .unwrap();
    let net = NetConfig {
        peer: LinkProfile::reliable(500),
        client,
        queue: QueueBound::of(&config),
        max_in_flight: EventBudget::DEFAULT,
    };
    let mut cluster = Cluster::new(config.clone(), master, net, EventBudget::DEFAULT).unwrap();
    let start = Mark::of(&cluster);
    let mut stream = master.stream(Subsystem::Clients);
    let plan = Plan::generate(&mut stream, config.key_value_limits(), Profile::PutGet);
    let mut clients = Clients::new(plan, &config, stream).with_misuse(misuse);
    clients
        .run(
            &mut cluster,
            VirtualTime::from_micros(RUN_SECONDS * 1_000_000),
        )
        .unwrap();
    let meta = Meta {
        seed,
        scenario_version: 1,
        profile: "put-get".to_owned(),
        build_id: "test-build".to_owned(),
        prng: "splitmix64".to_owned(),
        max_events: EventBudget::DEFAULT.get(),
        chaos_ticks: 1,
        heal_budget_ticks: 1,
        quiet_ticks: 1,
        phases: Phases {
            setup: start,
            baseline: start,
            chaos: start,
            heal: start,
            check: Mark::of(&cluster),
        },
    };
    (clients, cluster, config, meta)
}

fn document(seed: u64, misuse: u32) -> Value {
    let (clients, cluster, config, meta) = run(seed, misuse);
    observe::document(&cluster, &clients, &config, &meta).unwrap()
}

fn records(document: &Value) -> &Vec<Value> {
    document["records"].as_array().unwrap()
}

fn kind(record: &Value) -> &str {
    record["kind"].as_str().unwrap()
}

#[test]
fn a_real_run_validates_against_the_observation_schema() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    for seed in 0..SEEDS {
        for misuse in [0, ALWAYS] {
            let document = document(seed, misuse);
            let errors: Vec<String> = validator
                .iter_errors(&document)
                .map(|error| format!("{}: {error}", error.instance_path()))
                .collect();
            assert!(
                errors.is_empty(),
                "rule: a run's document is a valid keyval.observation/v1 (O-6); seed={seed} \
                 misuse={misuse}; errors={errors:?}"
            );
            let kinds: BTreeSet<&str> = records(&document).iter().map(kind).collect();
            for wanted in ["phase", "invoke", "response", "delivery", "resolution"] {
                assert!(
                    kinds.contains(wanted),
                    "rule: a PUT/GET run records {wanted} records; seed={seed} kinds={kinds:?}"
                );
            }
            assert_eq!(
                document["complete"],
                Value::Bool(true),
                "rule: a finished run within its event budget is complete (P10); seed={seed}"
            );
        }
    }
}

/// Stream rule 11: one resolution per logical write, after every record of its attempts.
fn resolutions_hold(seed: u64, records: &[Value], responded: &BTreeSet<u64>) {
    let resolutions: Vec<(usize, &Value)> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| kind(record) == "resolution")
        .collect();
    let first_resolution = resolutions.first().map(|(position, _)| *position).unwrap();
    assert!(
        records[first_resolution..]
            .iter()
            .all(|record| kind(record) == "resolution"),
        "rule: a resolution follows every record of its write's attempts (stream rule \
             11); seed={seed}"
    );
    let ids: BTreeSet<&str> = resolutions
        .iter()
        .map(|(_, record)| record["request_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        resolutions.len(),
        "rule: each logical write has exactly one resolution (stream rule 11); seed={seed}"
    );
    let writes: BTreeSet<&str> = records
        .iter()
        .filter(|record| kind(record) == "invoke" && record["misuse"] == Value::Bool(false))
        .filter_map(|record| record["request_id"].as_str())
        .collect();
    assert_eq!(
        ids, writes,
        "rule: the resolved writes are exactly the invoked non-misuse writes (§10.3); \
             seed={seed}"
    );
    for (_, record) in &resolutions {
        let evidence = record["evidence"].as_array().unwrap();
        for attempt in evidence {
            assert!(
                responded.contains(&attempt.as_u64().unwrap()),
                "rule: a resolution's evidence is answered attempts; seed={seed} {record}"
            );
        }
    }
}

#[test]
fn the_stream_rules_hold_for_a_real_run() {
    for seed in 0..SEEDS {
        let document = document(seed, ALWAYS / 2);
        let records = records(&document);
        let mut last = (0, 0);
        for (position, record) in records.iter().enumerate() {
            let at = (
                record["seq"].as_u64().unwrap(),
                record["time"].as_u64().unwrap(),
            );
            assert!(
                at.0 >= last.0 && at.1 >= last.1,
                "rule: seq and time never decrease (stream rule 1); seed={seed} record \
                 {position} {record}"
            );
            last = at;
        }
        let phases: Vec<&str> = records
            .iter()
            .filter(|record| kind(record) == "phase")
            .map(|record| record["phase"].as_str().unwrap())
            .collect();
        assert_eq!(
            phases,
            ["setup", "baseline", "chaos", "heal", "check"],
            "rule: the phases appear once each, in order (stream rule 2); seed={seed}"
        );
        let mut invoked: BTreeMap<u64, usize> = BTreeMap::new();
        let mut responded = BTreeSet::new();
        let mut generations: BTreeMap<(u64, u64), u64> = BTreeMap::new();
        for (position, record) in records.iter().enumerate() {
            let attempt = record["attempt"].as_u64();
            match kind(record) {
                "invoke" => {
                    let attempt = attempt.unwrap();
                    assert!(
                        invoked.insert(attempt, position).is_none(),
                        "rule: every attempt is invoked exactly once (stream rule 3); \
                         seed={seed} attempt={attempt}"
                    );
                    assert!(
                        record["client"].as_u64().unwrap() >= 1,
                        "rule: clients are numbered from 1; seed={seed} record {record}"
                    );
                }
                "response" => {
                    let attempt = attempt.unwrap();
                    assert!(
                        invoked.contains_key(&attempt),
                        "rule: a response follows its invocation; seed={seed} attempt={attempt}"
                    );
                    assert!(
                        responded.insert(attempt),
                        "rule: an attempt gets at most one response; seed={seed} \
                         attempt={attempt}"
                    );
                    let from = (
                        record["from"]["node"].as_u64().unwrap(),
                        record["from"]["incarnation"].as_u64().unwrap(),
                    );
                    let generation = record["generation"].as_u64().unwrap();
                    let before = generations.insert(from, generation).unwrap_or(0);
                    assert!(
                        generation > before,
                        "rule: generations strictly increase per node incarnation (stream \
                         rule 5); seed={seed} from={from:?} {before} then {generation}"
                    );
                }
                "delivery" => {
                    let attempt = attempt.unwrap();
                    assert!(
                        responded.contains(&attempt),
                        "rule: every delivery follows a response to its attempt (stream rule \
                         4); seed={seed} attempt={attempt}"
                    );
                }
                _ => {}
            }
        }
        resolutions_hold(seed, records, &responded);
    }
}

/// The first record of `kind` for `attempt` with `delivered` as given (`None`: any).
fn position(
    records: &[Value],
    kind_wanted: &str,
    attempt: u64,
    delivered: Option<bool>,
) -> Option<usize> {
    records.iter().position(|record| {
        kind(record) == kind_wanted
            && record["attempt"].as_u64() == Some(attempt)
            && delivered.is_none_or(|wanted| record["delivered"] == Value::Bool(wanted))
    })
}

#[test]
fn misuse_follows_a_terminal_result_is_a_conflict_and_is_no_logical_write() {
    let mut misused = 0;
    let mut conflicts = 0;
    for seed in 0..SEEDS {
        let (clients, cluster, config, meta) = run(seed, ALWAYS);
        let document = observe::document(&cluster, &clients, &config, &meta).unwrap();
        let records = records(&document);
        let resolutions: BTreeMap<&str, &Value> = records
            .iter()
            .filter(|record| kind(record) == "resolution")
            .map(|record| (record["request_id"].as_str().unwrap(), record))
            .collect();
        let mut once = BTreeSet::new();
        for invoke in records
            .iter()
            .filter(|record| kind(record) == "invoke" && record["misuse"] == Value::Bool(true))
        {
            misused += 1;
            let id = invoke["request_id"].as_str().unwrap();
            let attempt = invoke["attempt"].as_u64().unwrap();
            assert!(
                once.insert(id),
                "rule: a misuse attempt is never retried, so a request ID has one; seed={seed} id={id}"
            );
            let original = resolutions
                .get(id)
                .unwrap_or_else(|| panic!("rule: a misuse reuses a logical write's ID; {invoke}"));
            assert_eq!(
                original["resolved"], "executed",
                "rule: misuse follows an observed terminal result (§10.3); seed={seed} id={id}"
            );
            let decided = original["evidence"][0].as_u64().unwrap();
            let decided_invoke = &records[position(records, "invoke", decided, None).unwrap()];
            let own_write = decided_invoke["misuse"] == Value::Bool(false)
                && decided_invoke["request_id"].as_str() == Some(id);
            let own_status = decided_invoke["operation"]["op"] == "status"
                && decided_invoke["operation"]["request_id"].as_str() == Some(id);
            assert!(
                own_write || own_status,
                "rule: the evidence is one of the write's own attempts or its status lookup; \
                 seed={seed} id={id} {decided_invoke}"
            );
            let heard = position(records, "delivery", decided, Some(true)).unwrap_or_else(|| {
                panic!("rule: the evidence reached its client; seed={seed} id={id}")
            });
            let sent = position(records, "invoke", attempt, None).unwrap();
            assert!(
                heard < sent,
                "rule: a misuse is invoked after the client OBSERVED the terminal result, its \
                 delivery (§10.3); seed={seed} id={id}"
            );
            let original_command = records.iter().find(|record| {
                kind(record) == "invoke"
                    && record["misuse"] == Value::Bool(false)
                    && record["request_id"] == invoke["request_id"]
            });
            assert_ne!(
                original_command.map(|record| &record["operation"]),
                Some(&invoke["operation"]),
                "rule: a misuse attempt carries a different command (§10.3); seed={seed} id={id}"
            );
            if let Some(response) = records
                .iter()
                .find(|record| kind(record) == "response" && record["attempt"] == attempt)
            {
                let outcome = &response["outcome"];
                assert_eq!(
                    outcome["code"], "request_id_conflict",
                    "rule: a different command under a used request ID is answered \
                     request_id_conflict and never executes (§7.5 step 3); seed={seed} {response}"
                );
                conflicts += 1;
                assert_eq!(
                    outcome["original_index"], original["index"],
                    "rule: the conflict names the original's index (§8.4); seed={seed} id={id}"
                );
            }
        }
        let logical_writes = clients
            .outcomes()
            .iter()
            .filter(|outcome| outcome.request_id.is_some())
            .count();
        assert_eq!(
            resolutions.len(),
            logical_writes,
            "rule: misuse attempts are excluded from logical-write counts (§10.3); seed={seed}"
        );
    }
    assert!(
        misused > 0 && conflicts * 2 >= misused,
        "rule: most misuse attempts reach the node that answered and are answered \
         request_id_conflict; misused={misused} conflicts={conflicts}"
    );
}

#[test]
fn every_lost_response_is_recorded_as_a_lost_delivery() {
    let mut lost = 0;
    for seed in 0..SEEDS {
        let document = document(seed, 0);
        let records = records(&document);
        lost += records
            .iter()
            .filter(|record| {
                kind(record) == "delivery" && record["delivered"] == Value::Bool(false)
            })
            .count();
    }
    assert!(
        lost > 0,
        "rule: with 5% client-path loss some responses are lost, and a loss is a delivery \
         record with delivered=false (README); lost={lost}"
    );
}

#[test]
fn a_phase_follows_every_record_made_before_its_mark() {
    for seed in 0..2 {
        let config = run_config();
        let master = MasterSeed::new(seed);
        let net = NetConfig {
            peer: LinkProfile::reliable(500),
            client: LinkProfile::reliable(300),
            queue: QueueBound::of(&config),
            max_in_flight: EventBudget::DEFAULT,
        };
        let mut cluster = Cluster::new(config.clone(), master, net, EventBudget::DEFAULT).unwrap();
        let mut stream = master.stream(Subsystem::Clients);
        let plan = Plan::generate(&mut stream, config.key_value_limits(), Profile::PutGet);
        let mut clients = Clients::new(plan, &config, stream);
        let mut marks = vec![Mark::of(&cluster)];
        for seconds in [1, 2, 3, RUN_SECONDS] {
            clients
                .run(&mut cluster, VirtualTime::from_micros(seconds * 1_000_000))
                .unwrap();
            marks.push(Mark::of(&cluster));
        }
        let meta = Meta {
            seed,
            scenario_version: 1,
            profile: "put-get".to_owned(),
            build_id: "test-build".to_owned(),
            prng: "splitmix64".to_owned(),
            max_events: EventBudget::DEFAULT.get(),
            chaos_ticks: 1,
            heal_budget_ticks: 1,
            quiet_ticks: 1,
            phases: Phases {
                setup: marks[0],
                baseline: marks[1],
                chaos: marks[2],
                heal: marks[3],
                check: marks[4],
            },
        };
        let document = observe::document(&cluster, &clients, &config, &meta).unwrap();
        let records = records(&document);
        assert!(
            marks.windows(2).all(|pair| pair[0].seq < pair[1].seq),
            "rule: the test's marks are distinct mid-run points; seed={seed} marks={marks:?}"
        );
        for (position, record) in records.iter().enumerate() {
            if kind(record) != "phase" {
                continue;
            }
            let name = record["phase"].as_str().unwrap();
            let mark = match name {
                "setup" => marks[0],
                "baseline" => marks[1],
                "chaos" => marks[2],
                "heal" => marks[3],
                _ => marks[4],
            };
            for (other, record) in records.iter().enumerate() {
                if kind(record) == "phase" || kind(record) == "resolution" {
                    continue;
                }
                let seq = record["seq"].as_u64().unwrap();
                assert!(
                    (other < position) == (seq <= mark.seq),
                    "rule: a phase marked after N events follows every record of those events \
                     and precedes later ones (§10.2); seed={seed} phase={name} mark={mark:?} \
                     record {other} (seq {seq}) vs phase at {position}"
                );
            }
        }
    }
}

#[test]
fn without_misuse_no_attempt_is_flagged_and_a_seed_gives_one_document() {
    let first = document(7, 0);
    assert!(
        records(&first)
            .iter()
            .filter(|record| kind(record) == "invoke")
            .all(|record| record["misuse"] == Value::Bool(false)),
        "rule: the misuse stream is opt-in; no attempt is flagged by default"
    );
    for misuse in [0, ALWAYS / 2] {
        let a = serde_json::to_string(&document(7, misuse)).unwrap();
        let b = serde_json::to_string(&document(7, misuse)).unwrap();
        assert_eq!(
            a, b,
            "rule: a seed decides the document byte for byte (P4); misuse={misuse}"
        );
    }
}

#[test]
fn a_seed_the_document_cannot_carry_is_an_error_not_a_repaired_document() {
    let (clients, cluster, config, mut meta) = run(1, 0);
    meta.seed = 1 << 53;
    assert_eq!(
        observe::document(&cluster, &clients, &config, &meta),
        Err(ObserveError::TooLarge {
            what: "seed",
            value: 1 << 53
        }),
        "rule: integers carry below 2^53 (§8.1), and a larger one fails the build (P10)"
    );
}
