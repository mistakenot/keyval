//! W36.1's tests of the seeded simulation clients (PLAN.md §10.3, §10.2): the plans a seed
//! draws (bounds, contention, the edge-case corpus, the operation mix), and runs against a
//! five-node cluster whose client paths lose and duplicate messages: every send of a logical
//! write carries its one request ID and exact command, redirects are followed, ambiguity is
//! retried or resolved through the status lookup, a CAS expects what the client last saw,
//! and a seed decides the whole run.
#![cfg(test)]

// check: SIM-CLIENTS

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use types::{
    ClientOp, Config, ExpectedVersion, Key, KeyValueLimits, Mutation, Refusal, RequestState,
    Response, Version,
};

use crate::clients::{Clients, End, Intent, MAX_CLIENTS, Plan, Profile, SentAttempt};
use crate::cluster::Cluster;
use crate::entropy::{MasterSeed, Subsystem};
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, VirtualTime};

const LOCAL: &str = include_str!("../../../config/local.toml");

const SMALL: &str = include_str!("../../../config/fixtures/small-limits.toml");

/// Seeds the plan tests draw.
const PLANS: u64 = 300;

/// Seeds the run tests drive.
const RUNS: u64 = 4;

/// The virtual seconds a run may take.
const RUN_SECONDS: u64 = 240;

fn config(text: &str) -> Config {
    Config::parse(text, Path::new("config")).unwrap()
}

fn plan(seed: u64, limits: KeyValueLimits, profile: Profile) -> Plan {
    Plan::generate(
        &mut MasterSeed::new(seed).stream(Subsystem::Clients),
        limits,
        profile,
    )
}

/// The planned writes as (key, value, `Intent`) in client then operation order.
fn writes(plan: &Plan) -> impl Iterator<Item = &Intent> {
    plan.clients.iter().flat_map(|client| {
        client
            .ops
            .iter()
            .map(|planned| &planned.intent)
            .filter(|intent| intent.request_id().is_some())
    })
}

/// The run tests' config: `local.toml` with values of at most 1 KiB. A run whose writes carry
/// 64 KiB values is some 30 times slower in a debug build, and the plan tests prove the
/// maximum-size corpus.
fn run_config() -> Config {
    let config = config(&LOCAL.replace("max_value_bytes = 65536", "max_value_bytes = 1024"));
    assert_eq!(
        config.key_value_limits().max_value_bytes(),
        1_024,
        "fixture: the run config's values are at most 1 KiB"
    );
    config
}

/// A run of `profile`'s clients for `seed` against a five-node cluster whose client paths
/// lose 5% and duplicate 2% of messages; it ends when every client has finished, or after
/// [`RUN_SECONDS`].
fn run(seed: u64, profile: Profile) -> (Clients, Cluster) {
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
    let mut stream = master.stream(Subsystem::Clients);
    let plan = Plan::generate(&mut stream, config.key_value_limits(), profile);
    let mut clients = Clients::new(plan, &config, stream);
    clients
        .run(
            &mut cluster,
            VirtualTime::from_micros(RUN_SECONDS * 1_000_000),
        )
        .unwrap();
    (clients, cluster)
}

#[test]
fn plans_stay_within_their_bounds_and_the_configured_limits() {
    for text in [LOCAL, SMALL] {
        let limits = config(text).key_value_limits();
        for seed in 0..PLANS {
            let drawn = plan(seed, limits, Profile::PutGet);
            let keys: BTreeSet<&Key> = drawn.keys.iter().collect();
            let ids: Vec<_> = writes(&drawn).filter_map(Intent::request_id).collect();
            let distinct_ids: BTreeSet<_> = ids.iter().collect();
            let values_fit = writes(&drawn).all(|intent| match intent {
                Intent::Put { value, .. } | Intent::Cas { value, .. } => {
                    value.as_str().len() <= limits.max_value_bytes()
                }
                Intent::Delete { .. } | Intent::Get { .. } | Intent::Scan { .. } => true,
            });
            assert!(
                (1..=usize::try_from(MAX_CLIENTS).unwrap()).contains(&drawn.clients.len())
                    && (4..=32).contains(&drawn.keys.len())
                    && keys.len() == drawn.keys.len()
                    && drawn
                        .keys
                        .iter()
                        .all(|key| key.as_str().len() <= limits.max_key_bytes())
                    && (1..=drawn.keys.len()).contains(&drawn.hot)
                    && distinct_ids.len() == ids.len()
                    && values_fit
                    && drawn == plan(seed, limits, Profile::PutGet),
                "rule: a plan has 1 to 8 clients and 4 to 32 distinct keys, a non-empty hot \
                 subset, keys and values within the configured limits, a unique request ID per \
                 write, and is the same for the same seed (§10.3, P4); seed {seed}: {} clients, \
                 {} keys ({} distinct), hot {}, {} ids ({} distinct), values fit {values_fit}",
                drawn.clients.len(),
                drawn.keys.len(),
                keys.len(),
                drawn.hot,
                ids.len(),
                distinct_ids.len()
            );
        }
    }
}

#[test]
fn at_least_half_of_the_writes_target_the_hot_subset() {
    let limits = config(LOCAL).key_value_limits();
    let (mut hot, mut all) = (0_u64, 0_u64);
    for seed in 0..PLANS {
        let drawn = plan(seed, limits, Profile::PutGet);
        let hot_keys = &drawn.keys[..drawn.hot];
        for intent in writes(&drawn) {
            all += 1;
            hot += u64::from(intent.key().is_some_and(|key| hot_keys.contains(key)));
        }
    }
    assert!(
        all > 0 && hot * 2 >= all,
        "rule: at least half of the writes target the hot subset, so writes contend (§10.3); \
         {hot} of {all} writes are hot"
    );
}

#[test]
fn the_corpus_reaches_every_key_and_value_edge_case() {
    let limits = config(LOCAL).key_value_limits();
    let longest_key = limits.max_key_bytes();
    let longest_value = limits.max_value_bytes();
    let mut reached: BTreeMap<&str, u32> = BTreeMap::new();
    for seed in 0..PLANS {
        let drawn = plan(seed, limits, Profile::PutGet);
        let has = |text: &str| drawn.keys.iter().any(|key| key.as_str() == text);
        let mut count = |name: &'static str, holds: bool| {
            if holds {
                *reached.entry(name).or_insert(0) += 1;
            }
        };
        count("ordering a aa b", has("a") && has("aa") && has("b"));
        count("composed and decomposed", has("\u{e9}") && has("e\u{301}"));
        count(
            "non-ASCII",
            drawn.keys.iter().any(|key| !key.as_str().is_ascii()),
        );
        count(
            "URL-hostile",
            drawn
                .keys
                .iter()
                .any(|key| key.as_str().contains(['/', '?', '&', '%'])),
        );
        count(
            "longest ASCII key",
            drawn
                .keys
                .iter()
                .any(|key| key.as_str().is_ascii() && key.as_str().len() == longest_key),
        );
        count(
            "longest two-byte key",
            drawn
                .keys
                .iter()
                .any(|key| !key.as_str().is_ascii() && key.as_str().len() + 1 >= longest_key),
        );
        let mut last: BTreeMap<&Key, &str> = BTreeMap::new();
        for intent in writes(&drawn) {
            if let Intent::Put { key, value, .. } = intent {
                let text = value.as_str();
                count("empty value", text.is_empty());
                count("longest value", text.len() == longest_value);
                count(
                    "JSON-heavy value",
                    text.contains('"') && text.contains('\\') && text.contains('\u{1}'),
                );
                count(
                    "repeated same-value write",
                    last.get(key).is_some_and(|previous| *previous == text),
                );
                last.insert(key, text);
            }
        }
    }
    let classes = [
        "ordering a aa b",
        "composed and decomposed",
        "non-ASCII",
        "URL-hostile",
        "longest ASCII key",
        "longest two-byte key",
        "empty value",
        "longest value",
        "JSON-heavy value",
        "repeated same-value write",
    ];
    let missing: Vec<&str> = classes
        .iter()
        .copied()
        .filter(|class| !reached.contains_key(class))
        .collect();
    assert!(
        missing.is_empty(),
        "rule: the plans reach every §10.3 key and value edge case: ordering, non-ASCII, \
         composed and decomposed forms, / ? & %, maximum-length keys, empty, maximum-size, \
         JSON-expanding and repeated same-value writes; missing {missing:?}; reached {reached:?}"
    );
}

#[test]
fn each_profile_plans_its_operation_mix() {
    let limits = config(LOCAL).key_value_limits();
    let mut mix: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for seed in 0..PLANS {
        for (profile, name) in [(Profile::PutGet, "put-get"), (Profile::Mixed, "mixed")] {
            for client in plan(seed, limits, profile).clients {
                for planned in client.ops {
                    let kind = match planned.intent {
                        Intent::Put { .. } => "put",
                        Intent::Delete { .. } => "delete",
                        Intent::Cas { .. } => "cas",
                        Intent::Get { .. } => "get",
                        Intent::Scan { .. } => "scan",
                    };
                    *mix.entry((name, kind)).or_insert(0) += 1;
                }
            }
        }
    }
    let total = |profile: &str| -> u64 {
        mix.iter()
            .filter(|((name, _), _)| *name == profile)
            .map(|(_, count)| *count)
            .sum()
    };
    let share = |profile: &str, kind: &str| {
        mix.get(&(profile, kind)).copied().unwrap_or(0) * 100 / total(profile).max(1)
    };
    let put_get_only = mix
        .keys()
        .filter(|(name, _)| *name == "put-get")
        .all(|(_, kind)| ["put", "get"].contains(kind));
    let mixed_near = [
        ("put", 30),
        ("delete", 15),
        ("cas", 25),
        ("get", 20),
        ("scan", 10),
    ]
    .iter()
    .all(|(kind, target)| share("mixed", kind).abs_diff(*target) <= 3);
    assert!(
        put_get_only && share("put-get", "put").abs_diff(60) <= 3 && mixed_near,
        "rule: the PUT/GET profile plans only PUTs and GETs, three writes for two reads (W36, \
         §12.2); the mixed profile plans §10.3's 30/15/25/20/10 mix of PUT, DELETE, CAS, GET \
         and SCAN; counts {mix:?}"
    );
}

/// Whether `response` is a `not_leader` naming a leader, and which.
const fn redirect(response: &Response) -> Option<types::NodeId> {
    match response {
        Response::Refused(Refusal::NotLeader { leader }) => Some(*leader),
        Response::Refused(
            Refusal::Busy
            | Refusal::Unavailable
            | Refusal::Recovering { .. }
            | Refusal::UnknownOutcome
            | Refusal::HistoryFull { .. },
        )
        | Response::Written { .. }
        | Response::Conflict { .. }
        | Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_)
        | Response::Status { .. } => None,
    }
}

/// Checks one run (see the test below) and counts what it reached.
fn check_identity(
    seed: u64,
    clients: &Clients,
    cluster: &Cluster,
    reached: &mut BTreeMap<&str, u32>,
) {
    let mut count = |name: &'static str| *reached.entry(name).or_insert(0) += 1;
    assert!(
        clients.finished(),
        "fixture: seed {seed}'s clients finish within {RUN_SECONDS} virtual seconds; {} outcomes",
        clients.outcomes().len()
    );
    let planned: usize = clients
        .plan()
        .clients
        .iter()
        .map(|client| client.ops.len())
        .sum();
    let ended: BTreeSet<(u64, usize)> = clients
        .outcomes()
        .iter()
        .map(|outcome| (outcome.client.get(), outcome.op))
        .collect();
    assert_eq!(
        (ended.len(), clients.outcomes().len()),
        (planned, planned),
        "rule: every planned operation ends exactly once (done, refused or unresolved, never \
         lost); seed {seed}"
    );
    let mut by_op: BTreeMap<(u64, usize), Vec<&SentAttempt>> = BTreeMap::new();
    for sent in clients.sends() {
        by_op
            .entry((sent.client.get(), sent.op))
            .or_default()
            .push(sent);
    }
    let answers: BTreeMap<types::AttemptId, &Response> = cluster
        .answers()
        .map(|answer| (answer.attempt, &answer.response))
        .collect();
    for sends in by_op.values() {
        let first = &sends[0].request;
        let identity = match first {
            ClientOp::Write { request_id, .. } => Some(*request_id),
            ClientOp::Get { .. } | ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
        };
        for sent in sends {
            let same = match &sent.request {
                ClientOp::Status { request_id } => {
                    count("status lookups");
                    identity == Some(*request_id)
                }
                request => request == first,
            };
            assert!(
                same,
                "rule: every send of a logical operation is the same request, and a status \
                 lookup asks for the write's own request ID (§10.3, §8.7); seed {seed}: first \
                 {first:?}, then {:?}",
                sent.request
            );
        }
        for pair in sends.windows(2) {
            if let Some(leader) = answers
                .get(&pair[0].attempt)
                .and_then(|answer| redirect(answer))
            {
                count("redirects");
                assert_eq!(
                    pair[1].node, leader,
                    "rule: not_leader is followed to the named leader (§8.4); seed {seed}"
                );
            }
        }
        if sends.len() > 1 {
            count("retried operations");
        }
    }
    for outcome in clients.outcomes() {
        count(match &outcome.end {
            End::Done(Response::Written { .. }) => "written",
            End::Done(Response::Status {
                state: RequestState::Known(_),
                ..
            }) => "resolved by status",
            End::Done(Response::Found { .. } | Response::NotFound { .. }) => "read",
            End::Done(_) => "other answer",
            End::Refused => "refused",
            End::Unresolved(_) => "unresolved",
            End::NotExecuted(_) => "not executed",
        });
    }
}

#[test]
fn every_send_of_a_logical_write_carries_its_identity_and_redirects_are_followed() {
    let mut reached: BTreeMap<&str, u32> = BTreeMap::new();
    for seed in 0..RUNS {
        let (clients, cluster) = run(seed, Profile::PutGet);
        check_identity(seed, &clients, &cluster, &mut reached);
    }
    let wanted = ["written", "read", "redirects", "retried operations"];
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|name| !reached.contains_key(name))
        .collect();
    assert!(
        missing.is_empty() && !reached.contains_key("other answer"),
        "fixture: the runs reach written results, reads, redirects and retries, and no write \
         ends with a non-write answer; missing {missing:?}; reached {reached:?}"
    );
}

#[test]
fn a_cas_expects_the_version_its_client_last_saw() {
    let mut cases = 0;
    for seed in 0..RUNS {
        let (clients, _) = run(seed, Profile::Mixed);
        // Each client's first send of each operation, in order, with how each ended.
        let ends: BTreeMap<(u64, usize), &End> = clients
            .outcomes()
            .iter()
            .map(|outcome| ((outcome.client.get(), outcome.op), &outcome.end))
            .collect();
        let mut firsts: BTreeMap<(u64, usize), &ClientOp> = BTreeMap::new();
        for sent in clients.sends() {
            firsts
                .entry((sent.client.get(), sent.op))
                .or_insert(&sent.request);
        }
        let mut seen: BTreeMap<(u64, Key), Option<Version>> = BTreeMap::new();
        for ((client, op), request) in &firsts {
            if let ClientOp::Write {
                mutation: Mutation::Cas { key, expected, .. },
                ..
            } = request
            {
                let wanted = seen
                    .get(&(*client, key.clone()))
                    .copied()
                    .flatten()
                    .map_or(ExpectedVersion::Absent, ExpectedVersion::Version);
                assert_eq!(
                    *expected, wanted,
                    "rule: a CAS expects the version its client last saw for the key (from a \
                     read or its own write), or absence (§10.3); seed {seed} client {client} op \
                     {op}"
                );
                cases += 1;
            }
            let key = match request {
                ClientOp::Write { mutation, .. } => Some(mutation.key().clone()),
                ClientOp::Get { key } => Some(key.clone()),
                ClientOp::Scan { .. } | ClientOp::Status { .. } => None,
            };
            let Some(key) = key else { continue };
            let learned = match ends.get(&(*client, *op)) {
                Some(End::Done(Response::Found { version, .. })) => Some(Some(*version)),
                Some(End::Done(Response::NotFound { .. })) => Some(None),
                Some(End::Done(
                    Response::Written { result, .. }
                    | Response::Status {
                        state: RequestState::Known(result),
                        ..
                    },
                )) => match result.decision() {
                    types::Decision::Set => Some(result.version()),
                    types::Decision::Deleted { .. } => Some(None),
                    types::Decision::VersionMismatch { current } => Some(current),
                    types::Decision::CapacityExceeded { .. } => None,
                },
                Some(_) | None => None,
            };
            if let Some(version) = learned {
                seen.insert((*client, key), version);
            }
        }
    }
    assert!(
        cases > 0,
        "fixture: the mixed runs send compare-and-swaps, so the rule is exercised"
    );
}

#[test]
fn a_seed_decides_the_whole_run() {
    let (first, first_cluster) = run(7, Profile::PutGet);
    let (second, second_cluster) = run(7, Profile::PutGet);
    assert!(
        first.sends() == second.sends()
            && first.outcomes() == second.outcomes()
            && first_cluster.trace().hash() == second_cluster.trace().hash(),
        "rule: the same seed gives the same sends, outcomes and trace (P4, T-3)"
    );
}

/// The operation counts of `profile` over the plan seeds, by kind.
fn shares(profile: Profile, limits: KeyValueLimits) -> BTreeMap<&'static str, u64> {
    let mut mix = BTreeMap::new();
    for seed in 0..PLANS {
        for client in plan(seed, limits, profile).clients {
            for planned in client.ops {
                let kind = match planned.intent {
                    Intent::Put { .. } => "put",
                    Intent::Delete { .. } => "delete",
                    Intent::Cas { .. } => "cas",
                    Intent::Get { .. } => "get",
                    Intent::Scan { .. } => "scan",
                };
                *mix.entry(kind).or_insert(0) += 1;
            }
        }
    }
    mix
}

#[test]
fn the_dedicated_profiles_shape_their_plans() {
    let limits = config(LOCAL).key_value_limits();
    let percent = |mix: &BTreeMap<&str, u64>, kinds: &[&str]| {
        let total: u64 = mix.values().sum();
        kinds.iter().filter_map(|kind| mix.get(kind)).sum::<u64>() * 100 / total.max(1)
    };
    let cas = shares(Profile::CasContention, limits);
    let read = shares(Profile::ReadIsolation, limits);
    let contended_keys: BTreeSet<Key> = (0..PLANS)
        .flat_map(|seed| plan(seed, limits, Profile::CasContention).clients)
        .flat_map(|client| client.ops)
        .filter(|planned| planned.intent.request_id().is_some())
        .filter_map(|planned| planned.intent.key().cloned())
        .collect();
    let per_seed_single_key = (0..PLANS).all(|seed| {
        let keys: BTreeSet<Key> = plan(seed, limits, Profile::CasContention)
            .clients
            .into_iter()
            .flat_map(|client| client.ops)
            .filter(|planned| planned.intent.request_id().is_some())
            .filter_map(|planned| planned.intent.key().cloned())
            .collect();
        keys.len() <= 1
    });
    assert!(
        percent(&cas, &["cas"]).abs_diff(55) <= 3 && per_seed_single_key,
        "rule: CAS contention plans 55% CAS and aims every write of a run at one key (§10.3); \
         mix {cas:?}; distinct write keys across seeds {}",
        contended_keys.len()
    );
    assert!(
        percent(&read, &["get", "scan"]).abs_diff(70) <= 3,
        "rule: read isolation plans 70% reads (GET and SCAN) beside a light write load; \
         mix {read:?}"
    );
}

#[test]
fn boundary_values_draw_only_the_edge_case_values() {
    let limits = config(LOCAL).key_value_limits();
    let longest = limits.max_value_bytes();
    let mut seen = 0_u64;
    for seed in 0..PLANS {
        for client in plan(seed, limits, Profile::BoundaryValues).clients {
            for planned in client.ops {
                if let Intent::Put { value, .. } | Intent::Cas { value, .. } = planned.intent {
                    seen += 1;
                    let text = value.as_str();
                    assert!(
                        text.is_empty() || text.len() == longest || text.contains('\u{1}'),
                        "rule: the boundary profile writes only empty, maximum-size or \
                         escape-heavy values (§10.3); seed={seed} value={text:?}"
                    );
                }
            }
        }
    }
    assert!(
        seen > 0,
        "rule: the boundary profile plans writes; seen={seen}"
    );
}

#[test]
fn profile_names_round_trip_and_only_misuse_runs_misuse() {
    for profile in Profile::ALL {
        assert_eq!(
            Profile::parse(profile.name()),
            Some(profile),
            "rule: a profile's name reads back to it; profile={profile:?}"
        );
        assert_eq!(
            profile.misuse_ppm() > 0,
            profile == Profile::Misuse,
            "rule: only the misuse profile turns the misuse stream on; profile={profile:?}"
        );
    }
    assert_eq!(
        Profile::parse("nonsense"),
        None,
        "rule: unknown names are refused"
    );
}
