//! Path profiles changed mid-run (br-gxxv; PLAN.md §10.2, §10.16): a fault-free baseline,
//! faults switched on, and their healing, each from the next send. Copies already in flight
//! keep the arrival time they drew, and the same changes replay identically.
#![cfg(test)]

use std::path::Path;

use types::{Config, NodeId};

use crate::entropy::SplitMix64;
use crate::net::{Arrival, DropReason, LinkProfile, NetConfig, Network, Ppm, QueueBound, Sent};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::trace::{ClientId, Endpoint};

const LOCAL: &str = include_str!("../../../config/local.toml");

fn node(id: u64) -> Endpoint {
    Endpoint::Node(NodeId::new(id).unwrap())
}

fn at(micros: u64) -> VirtualTime {
    VirtualTime::from_micros(micros)
}

/// A network whose paths all deliver after exactly `delay` microseconds.
fn network(delay: u64, seed: u64) -> Network {
    let config = Config::parse(LOCAL, Path::new("config")).unwrap();
    Network::new(
        NetConfig {
            peer: LinkProfile::reliable(delay),
            client: LinkProfile::reliable(delay),
            queue: QueueBound::of(&config),
            max_in_flight: EventBudget::DEFAULT,
        },
        SplitMix64::new(seed),
    )
}

/// A profile that loses or duplicates every message, after `delay` microseconds.
fn faulty(delay: u64, loss: Ppm, duplicate: Ppm) -> LinkProfile {
    LinkProfile::new(delay, delay, loss, duplicate).unwrap()
}

/// The arrival times of `sent`'s copies.
fn times(sent: &Sent) -> Vec<u64> {
    sent.arrivals
        .iter()
        .map(|(time, _)| time.as_micros())
        .collect()
}

#[test]
fn a_new_profile_applies_from_the_next_send_on_both_paths() {
    let mut net = network(100, 1);
    let before = net.send(at(0), node(1), node(2), b"before").unwrap();
    let slow = LinkProfile::reliable(5_000);
    net.set_profiles(slow, LinkProfile::reliable(7_000));
    assert_eq!(
        net.profiles(),
        (slow, LinkProfile::reliable(7_000)),
        "rule: the profiles in force are the ones set"
    );
    let peer = net.send(at(10), node(1), node(2), b"peer").unwrap();
    let client = net
        .send(
            at(10),
            node(1),
            Endpoint::Client(ClientId::new(1)),
            b"client",
        )
        .unwrap();
    assert_eq!(
        (times(&before), times(&peer), times(&client)),
        (vec![100], vec![5_010], vec![7_010]),
        "rule: a send draws from the profiles in force when it is sent, peer and client \
         paths alike (br-gxxv)"
    );
}

#[test]
fn copies_in_flight_keep_their_arrival_times_and_are_still_delivered() {
    let mut net = network(100, 2);
    let mut early: Vec<Sent> = (0..5)
        .map(|n| net.send(at(n), node(1), node(2), b"early").unwrap())
        .collect();
    let early_times: Vec<Vec<u64>> = early.iter().map(times).collect();
    net.set_profiles(
        faulty(9_000, Ppm::ALWAYS, Ppm::NEVER),
        faulty(9_000, Ppm::ALWAYS, Ppm::NEVER),
    );
    let later = net.send(at(50), node(1), node(2), b"later").unwrap();
    assert_eq!(
        later.dropped,
        Some(DropReason::Lost),
        "fixture: the new profile loses every later send"
    );
    for (sent, drawn) in early.iter_mut().zip(&early_times) {
        assert_eq!(
            &times(sent),
            drawn,
            "rule: a copy in flight keeps the arrival time it drew when sent"
        );
        for (_, id) in sent.arrivals.drain(..) {
            let arrival = net.arrive(id);
            assert!(
                match arrival {
                    Arrival::Delivered(..) => true,
                    Arrival::Dropped(_) | Arrival::Unknown => false,
                },
                "rule: a profile change never reaches a copy already in flight; got \
                 {arrival:?}"
            );
        }
    }
}

#[test]
fn faults_start_and_heal_mid_run() {
    let mut net = network(100, 3);
    let baseline = LinkProfile::reliable(100);
    let mut fates = Vec::new();
    for (phase, peer) in [
        ("baseline", baseline),
        ("loss", faulty(100, Ppm::ALWAYS, Ppm::NEVER)),
        ("duplication", faulty(100, Ppm::NEVER, Ppm::ALWAYS)),
        ("healed", baseline),
    ] {
        net.set_profiles(peer, baseline);
        let sent = net.send(at(0), node(1), node(2), phase.as_bytes()).unwrap();
        fates.push((phase, sent.dropped, sent.arrivals.len()));
    }
    assert_eq!(
        fates,
        vec![
            ("baseline", None, 1),
            ("loss", Some(DropReason::Lost), 0),
            ("duplication", None, 2),
            ("healed", None, 1),
        ],
        "rule: a fault-free baseline, then loss, then duplication, then healed: each \
         phase's profile decides that phase's sends (§10.2, §10.16)"
    );
    let counts = net.counts();
    assert_eq!(
        (counts.sent, counts.lost, counts.duplicated),
        (4, 1, 1),
        "rule: the counts follow the phases"
    );
}

#[test]
fn the_same_changes_at_the_same_points_replay_identically() {
    let run = |seed: u64| {
        let mut net = network(100, seed);
        let lossy = LinkProfile::new(
            10,
            20_000,
            Ppm::new(300_000).unwrap(),
            Ppm::new(200_000).unwrap(),
        )
        .unwrap();
        (0..60_u64)
            .map(|n| {
                if n == 20 {
                    net.set_profiles(lossy, lossy);
                }
                if n == 40 {
                    net.set_profiles(LinkProfile::reliable(100), LinkProfile::reliable(100));
                }
                let sent = net
                    .send(at(n * 10), node(1 + n % 3), node(1 + (n + 1) % 3), b"m")
                    .unwrap();
                (times(&sent), sent.dropped, sent.records)
            })
            .collect::<Vec<_>>()
    };
    let first = run(9);
    assert_eq!(
        first,
        run(9),
        "rule: the same seed and changes give the same run (T-3)"
    );
    assert!(
        first[20..40]
            .iter()
            .any(|(times, dropped, _)| dropped.is_some() || times.len() > 1)
            && first[40..]
                .iter()
                .all(|(times, dropped, _)| dropped.is_none() && times.len() == 1),
        "fixture: the lossy phase loses or duplicates something, and the healed phase \
         neither"
    );
}
