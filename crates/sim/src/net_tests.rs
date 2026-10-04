//! W16.1's tests of the simulated network (PLAN.md §9.1, §10.2): delay and reordering,
//! loss, duplication, directed partitions, bounded link queues, client paths, determinism,
//! and real sealed peer envelopes on the wire.
#![cfg(test)]

// check: SIM-NET

use std::path::Path;

use raft_adapter::PeerCodec;
use types::{ByteCount, Config, InstanceId, ItemCount, NodeId, OutboundMessage};

use crate::entropy::SplitMix64;
use crate::net::{
    Arrival, DropReason, LinkProfile, NetConfig, NetCounts, Network, PacketId, Ppm, QueueBound,
};
use crate::scheduler::VirtualTime;
use crate::trace::{ClientId, Endpoint, PacketFate, Record};

fn node(id: u64) -> Endpoint {
    Endpoint::Node(NodeId::new(id).unwrap())
}

fn id(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

const LOCAL: &str = include_str!("../../../config/local.toml");

fn local() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

/// A network over `peer` and `client` paths, with the local config's queue bound.
fn network(peer: LinkProfile, client: LinkProfile, seed: u64) -> Network {
    Network::new(
        NetConfig {
            peer,
            client,
            queue: QueueBound::of(&local()),
            max_in_flight: ItemCount::new(1_000_000),
        },
        SplitMix64::new(seed),
    )
}

fn reliable(seed: u64) -> Network {
    network(LinkProfile::reliable(100), LinkProfile::reliable(100), seed)
}

/// Delivers every arrival of `sent` in time order, returning what each did.
fn deliver_all(net: &mut Network, arrivals: &mut Vec<(VirtualTime, PacketId)>) -> Vec<Arrival> {
    arrivals.sort();
    arrivals.drain(..).map(|(_, id)| net.arrive(id)).collect()
}

#[test]
fn delivery_follows_delay_not_send_order() {
    let mut net = network(
        LinkProfile::new(0, 10_000, Ppm::NEVER, Ppm::NEVER).unwrap(),
        LinkProfile::reliable(1),
        7,
    );
    let mut arrivals = Vec::new();
    for n in 0_u8..50 {
        let sent = net.send(VirtualTime::ZERO, node(1), node(2), &[n]);
        assert_eq!(
            sent.arrivals.len(),
            1,
            "rule: a reliable path makes one copy"
        );
        arrivals.extend(sent.arrivals);
    }
    let order: Vec<u8> = deliver_all(&mut net, &mut arrivals)
        .into_iter()
        .map(|arrival| match arrival {
            Arrival::Delivered(delivery, _) => delivery.bytes[0],
            Arrival::Dropped(_) | Arrival::Unknown => panic!("rule: nothing is lost"),
        })
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        (0..50).collect::<Vec<u8>>(),
        "rule: every message arrives once"
    );
    assert_ne!(
        order, sorted,
        "rule: delivery follows each message's own delay, not send order: the network is never \
         FIFO (§9.3)"
    );
    assert_eq!(
        net.counts(),
        NetCounts {
            sent: 50,
            delivered: 50,
            ..NetCounts::default()
        },
        "rule: every fate is counted"
    );
}

#[test]
fn every_delay_lies_in_its_range_and_the_same_seed_gives_the_same_fates() {
    let profile = LinkProfile::new(
        500,
        900,
        Ppm::new(200_000).unwrap(),
        Ppm::new(100_000).unwrap(),
    )
    .unwrap();
    let run = |seed| {
        let mut net = network(profile, profile, seed);
        (0_u8..200)
            .map(|n| net.send(VirtualTime::from_micros(1_000), node(1), node(3), &[n]))
            .collect::<Vec<_>>()
    };
    let first = run(5);
    for sent in &first {
        for (at, _) in &sent.arrivals {
            assert!(
                (1_500..=1_900).contains(&at.as_micros()),
                "rule: a copy arrives within its path's delay range; at={at:?}"
            );
        }
    }
    assert_eq!(first, run(5), "rule: same seed, same fates (T-3)");
    assert_ne!(first, run(6), "rule: another seed draws other fates");
}

#[test]
fn loss_follows_its_probability_on_each_path() {
    for (loss, low, high) in [
        (0, 0, 0),
        (300_000, 2_700, 3_300),
        (1_000_000, 10_000, 10_000),
    ] {
        let profile = LinkProfile::new(1, 1, Ppm::new(loss).unwrap(), Ppm::NEVER).unwrap();
        let mut net = network(profile, LinkProfile::reliable(1), 11);
        let mut lost = 0;
        let mut arrivals = Vec::new();
        for _ in 0..10_000 {
            let sent = net.send(VirtualTime::ZERO, node(1), node(2), b"m");
            if sent.dropped == Some(DropReason::Lost) {
                lost += 1;
                assert_eq!(
                    sent.records.last().map(fate),
                    Some(PacketFate::Dropped),
                    "rule: a loss is traced"
                );
            }
            arrivals.extend(sent.arrivals);
            // Keep the link queue empty, so only chance drops anything.
            deliver_all(&mut net, &mut arrivals);
        }
        assert!(
            (low..=high).contains(&lost),
            "rule: a {loss}-ppm path loses about that share; lost {lost} of 10000"
        );
        assert_eq!(net.counts().lost, lost, "rule: every loss is counted");
    }
}

fn fate(record: &Record) -> PacketFate {
    match record {
        Record::Packet { fate, .. } => *fate,
        other => panic!("fixture: not a packet record: {other:?}"),
    }
}

#[test]
fn a_duplicate_is_an_independent_copy() {
    let profile = LinkProfile::new(0, 1_000_000, Ppm::NEVER, Ppm::ALWAYS).unwrap();
    let mut net = network(profile, profile, 3);
    let (mut earlier, mut later) = (0, 0);
    let mut extra_times = std::collections::BTreeSet::new();
    for n in 0_u8..20 {
        let mut sent = net.send(VirtualTime::ZERO, node(4), node(5), &[n]);
        assert_eq!(
            (
                sent.arrivals.len(),
                sent.records.iter().map(fate).collect::<Vec<_>>()
            ),
            (2, vec![PacketFate::Sent, PacketFate::Duplicated]),
            "rule: a duplicated packet arrives twice, and the extra copy is traced"
        );
        assert_eq!(
            net.load(id(4), id(5)),
            (1, 1),
            "rule: only the original holds a sender queue slot; the duplicate is the network's"
        );
        let (original, extra) = (sent.arrivals[0].0, sent.arrivals[1].0);
        if extra < original {
            earlier += 1;
        } else if extra > original {
            later += 1;
        }
        extra_times.insert(extra);
        for arrival in deliver_all(&mut net, &mut sent.arrivals) {
            let Arrival::Delivered(delivery, _) = arrival else {
                panic!("rule: both copies are delivered");
            };
            assert_eq!(
                delivery.bytes,
                vec![n],
                "rule: each copy carries the same bytes"
            );
        }
    }
    assert!(
        earlier > 0 && later > 0 && extra_times.len() > 10,
        "rule: each copy draws its own delay, so a duplicate may arrive before or after the \
         original; earlier={earlier} later={later} distinct={}",
        extra_times.len()
    );
    assert_eq!(
        (net.counts().duplicated, net.counts().delivered),
        (20, 40),
        "rule: duplicates are counted"
    );
    assert_eq!(
        net.load(id(4), id(5)),
        (0, 0),
        "rule: a duplicate takes no sender queue slot, and the original's is freed"
    );
}

#[test]
fn a_symmetric_partition_cuts_both_ways_between_groups_only() {
    let mut net = reliable(1);
    net.links_mut()
        .partition(&[&[id(1), id(2)], &[id(3), id(4), id(5)]]);
    for (from, to, crosses) in [
        (1, 3, true),
        (3, 1, true),
        (2, 5, true),
        (1, 2, false),
        (4, 5, false),
    ] {
        let sent = net.send(VirtualTime::ZERO, node(from), node(to), b"m");
        assert_eq!(
            sent.dropped,
            crosses.then_some(DropReason::CutAtSend),
            "rule: a 2/3 split blocks every link across it, both ways, and none inside it; \
             {from}->{to}"
        );
    }
    net.links_mut().heal_all();
    let sent = net.send(VirtualTime::ZERO, node(1), node(3), b"m");
    assert_eq!(sent.dropped, None, "rule: healing restores the link");
}

#[test]
fn an_asymmetric_cut_blocks_one_direction() {
    // A leader that can send but not receive.
    let mut net = reliable(1);
    for peer in 2..=5 {
        net.links_mut().cut(id(peer), id(1));
    }
    let out = net.send(VirtualTime::ZERO, node(1), node(2), b"append");
    let back = net.send(VirtualTime::ZERO, node(2), node(1), b"ack");
    assert_eq!(
        (out.dropped, back.dropped),
        (None, Some(DropReason::CutAtSend)),
        "rule: a directed cut blocks only its own direction (§9.1)"
    );
    // A directed cycle: 1 -> 2 -> 3 -> 1 cut, the reverse open.
    let mut net = reliable(1);
    for (from, to) in [(1, 2), (2, 3), (3, 1)] {
        net.links_mut().cut(id(from), id(to));
    }
    for (from, to, open) in [
        (1, 2, false),
        (2, 1, true),
        (2, 3, false),
        (3, 2, true),
        (3, 1, false),
        (1, 3, true),
    ] {
        assert_eq!(
            net.links().allows(id(from), id(to)),
            open,
            "rule: a directed cycle of cuts; {from}->{to}"
        );
    }
}

#[test]
fn a_cut_also_drops_traffic_already_in_flight() {
    let mut net = reliable(1);
    let mut sent = net.send(VirtualTime::ZERO, node(1), node(2), b"in flight");
    net.links_mut().cut(id(1), id(2));
    let (_, packet) = sent.arrivals.pop().unwrap();
    assert!(
        matches!(
            net.arrive(packet),
            Arrival::Dropped(Record::Packet {
                fate: PacketFate::Dropped,
                ..
            })
        ),
        "rule: a partition catches a packet in flight across it"
    );
    assert_eq!(net.counts().cut_in_flight, 1, "rule: and counts it");
    assert_eq!(
        net.load(id(1), id(2)),
        (0, 0),
        "rule: its queue slot is freed"
    );
    assert_eq!(
        net.arrive(packet),
        Arrival::Unknown,
        "rule: a copy arrives once"
    );
}

#[test]
fn a_full_link_queue_drops_and_counts_and_never_grows() {
    let config = local();
    assert_eq!(
        QueueBound::of(&config),
        QueueBound {
            messages: ItemCount::new(64),
            bytes: ByteCount::new(8 << 20),
        },
        "fixture: the local config's bound is §7.11's 64 messages / 8 MiB"
    );
    let mut net = Network::new(
        NetConfig {
            peer: LinkProfile::reliable(10),
            client: LinkProfile::reliable(10),
            queue: QueueBound {
                messages: ItemCount::new(3),
                bytes: ByteCount::new(100),
            },
            max_in_flight: ItemCount::new(1_000),
        },
        SplitMix64::new(1),
    );
    let mut arrivals = Vec::new();
    for _ in 0..3 {
        arrivals.extend(
            net.send(VirtualTime::ZERO, node(1), node(2), &[0; 10])
                .arrivals,
        );
    }
    let full = net.send(VirtualTime::ZERO, node(1), node(2), &[0; 10]);
    assert_eq!(
        (full.dropped, net.load(id(1), id(2))),
        (Some(DropReason::QueueFull), (3, 30)),
        "rule: a send past the message bound is dropped, and the queue never grows (§7.11)"
    );
    let other = net.send(VirtualTime::ZERO, node(1), node(3), &[0; 10]);
    assert_eq!(
        other.dropped, None,
        "rule: each directed link has its own queue"
    );
    let (_, first) = arrivals.remove(0);
    assert!(matches!(net.arrive(first), Arrival::Delivered(..)));
    let room = net.send(VirtualTime::ZERO, node(1), node(2), &[0; 10]);
    assert_eq!(room.dropped, None, "rule: an arrival frees its slot");
    let heavy = net.send(VirtualTime::ZERO, node(2), node(1), &[0; 101]);
    assert_eq!(
        heavy.dropped,
        Some(DropReason::QueueFull),
        "rule: a send past the byte bound is dropped too"
    );
    assert_eq!(
        net.counts().queue_full,
        2,
        "rule: every overflow is counted as a network failure"
    );
}

#[test]
fn client_paths_have_their_own_profile_and_are_never_partitioned() {
    let mut net = network(
        LinkProfile::reliable(1),
        LinkProfile::new(1, 1, Ppm::ALWAYS, Ppm::NEVER).unwrap(),
        1,
    );
    net.links_mut().partition(&[&[id(1)], &[id(2), id(3)]]);
    let client = Endpoint::Client(ClientId::new(9));
    let reply = net.send(VirtualTime::ZERO, node(1), client, b"response");
    assert_eq!(
        reply.dropped,
        Some(DropReason::Lost),
        "rule: a client response can be lost after a committed application (§9.1)"
    );
    let mut net = network(LinkProfile::reliable(1), LinkProfile::reliable(5), 1);
    net.links_mut().partition(&[&[id(1)], &[id(2), id(3)]]);
    let reply = net.send(VirtualTime::ZERO, node(1), client, b"response");
    assert_eq!(
        (
            reply.dropped,
            reply.arrivals.first().map(|(at, _)| at.as_micros())
        ),
        (None, Some(5)),
        "rule: clients are never partitioned, and their paths use the client profile"
    );
}

#[test]
fn too_many_copies_in_flight_overflow_and_are_counted() {
    let mut net = Network::new(
        NetConfig {
            peer: LinkProfile::reliable(1),
            client: LinkProfile::reliable(1),
            queue: QueueBound::of(&local()),
            max_in_flight: ItemCount::new(2),
        },
        SplitMix64::new(1),
    );
    let outcomes: Vec<_> = (0..3)
        .map(|_| net.send(VirtualTime::ZERO, node(1), node(2), b"m").dropped)
        .collect();
    assert_eq!(
        (outcomes, net.in_flight(), net.counts().overflow),
        (vec![None, None, Some(DropReason::Overflow)], 2, 1),
        "rule: the network holds at most its bound in flight; past it a copy is dropped and \
         counted (CD-9)"
    );
}

#[test]
fn real_sealed_peer_envelopes_cross_the_network_unchanged() {
    let config = local();
    let instance = InstanceId::from_bytes([3; 16]);
    let codec = |node: NodeId| {
        PeerCodec::new(
            instance,
            config.semantic_fingerprint(),
            node,
            config.voters(),
        )
        .unwrap()
    };
    let voters: Vec<NodeId> = config.voters().collect();
    let (from, to) = (voters[0], voters[1]);
    // A heartbeat, as raft-rs encodes it: MsgHeartbeat (8), to, from, term 1.
    let mut message = vec![0x08, 0x08, 0x10];
    message.push(u8::try_from(to.get()).unwrap());
    message.push(0x18);
    message.push(u8::try_from(from.get()).unwrap());
    message.extend_from_slice(&[0x20, 0x01]);
    let envelope = codec(from)
        .seal(&OutboundMessage {
            to,
            bytes: message.clone(),
        })
        .unwrap();
    let mut net = reliable(1);
    let mut sent = net.send(
        VirtualTime::ZERO,
        Endpoint::Node(from),
        Endpoint::Node(to),
        &envelope,
    );
    let Some(Arrival::Delivered(delivery, _)) = deliver_all(&mut net, &mut sent.arrivals).pop()
    else {
        panic!("rule: a reliable path delivers it");
    };
    assert_eq!(
        delivery.bytes, envelope,
        "rule: the network carries the sealed bytes unchanged"
    );
    assert_eq!(
        codec(to).open(from, &delivery.bytes),
        Ok(message),
        "rule: the destination opens the production envelope (§8.2, P3)"
    );
}
