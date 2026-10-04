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
    Arrival, DropReason, LinkProfile, NetConfig, NetCounts, NetError, Network, PacketId, Ppm,
    QueueBound,
};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::trace::{ClientId, Endpoint, ObservationBound, PacketFate, Record, Trace};

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
            max_in_flight: EventBudget::DEFAULT,
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
        let sent = net.send(VirtualTime::ZERO, node(1), node(2), &[n]).unwrap();
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
            .map(|n| {
                net.send(VirtualTime::from_micros(1_000), node(1), node(3), &[n])
                    .unwrap()
            })
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
            let sent = net.send(VirtualTime::ZERO, node(1), node(2), b"m").unwrap();
            if sent.dropped == Some(DropReason::Lost) {
                lost += 1;
                assert_eq!(
                    sent.records.last().map(fate),
                    Some(PacketFate::Dropped(DropReason::Lost)),
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
        let mut sent = net.send(VirtualTime::ZERO, node(4), node(5), &[n]).unwrap();
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
        let sent = net
            .send(VirtualTime::ZERO, node(from), node(to), b"m")
            .unwrap();
        assert_eq!(
            sent.dropped,
            crosses.then_some(DropReason::CutAtSend),
            "rule: a 2/3 split blocks every link across it, both ways, and none inside it; \
             {from}->{to}"
        );
    }
    net.links_mut().heal_all();
    let sent = net.send(VirtualTime::ZERO, node(1), node(3), b"m").unwrap();
    assert_eq!(sent.dropped, None, "rule: healing restores the link");
}

#[test]
fn an_asymmetric_cut_blocks_one_direction() {
    // A leader that can send but not receive.
    let mut net = reliable(1);
    for peer in 2..=5 {
        net.links_mut().cut(id(peer), id(1));
    }
    let out = net
        .send(VirtualTime::ZERO, node(1), node(2), b"append")
        .unwrap();
    let back = net
        .send(VirtualTime::ZERO, node(2), node(1), b"ack")
        .unwrap();
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
    let mut sent = net
        .send(VirtualTime::ZERO, node(1), node(2), b"in flight")
        .unwrap();
    net.links_mut().cut(id(1), id(2));
    let (_, packet) = sent.arrivals.pop().unwrap();
    assert!(
        matches!(
            net.arrive(packet),
            Arrival::Dropped(Record::Packet {
                fate: PacketFate::Dropped(DropReason::CutInFlight),
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
            max_in_flight: EventBudget::new(1_000),
        },
        SplitMix64::new(1),
    );
    let mut arrivals = Vec::new();
    for _ in 0..3 {
        arrivals.extend(
            net.send(VirtualTime::ZERO, node(1), node(2), &[0; 10])
                .unwrap()
                .arrivals,
        );
    }
    let full = net
        .send(VirtualTime::ZERO, node(1), node(2), &[0; 10])
        .unwrap();
    assert_eq!(
        (full.dropped, net.load(id(1), id(2))),
        (Some(DropReason::QueueFull), (3, 30)),
        "rule: a send past the message bound is dropped, and the queue never grows (§7.11)"
    );
    let other = net
        .send(VirtualTime::ZERO, node(1), node(3), &[0; 10])
        .unwrap();
    assert_eq!(
        other.dropped, None,
        "rule: each directed link has its own queue"
    );
    let (_, first) = arrivals.remove(0);
    assert!(matches!(net.arrive(first), Arrival::Delivered(..)));
    let room = net
        .send(VirtualTime::ZERO, node(1), node(2), &[0; 10])
        .unwrap();
    assert_eq!(room.dropped, None, "rule: an arrival frees its slot");
    let heavy = net
        .send(VirtualTime::ZERO, node(2), node(1), &[0; 101])
        .unwrap();
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
    let reply = net
        .send(VirtualTime::ZERO, node(1), client, b"response")
        .unwrap();
    assert_eq!(
        reply.dropped,
        Some(DropReason::Lost),
        "rule: a client response can be lost after a committed application (§9.1)"
    );
    let mut net = network(LinkProfile::reliable(1), LinkProfile::reliable(5), 1);
    net.links_mut().partition(&[&[id(1)], &[id(2), id(3)]]);
    let reply = net
        .send(VirtualTime::ZERO, node(1), client, b"response")
        .unwrap();
    assert_eq!(
        (
            reply.dropped,
            reply.arrivals.first().map(|(at, _)| at.as_micros())
        ),
        (None, Some(5)),
        "rule: clients are never partitioned, and their paths use the client profile"
    );
}

/// A network of reliable paths duplicating with probability `duplicate`, holding at most
/// `bound` copies in flight.
fn bounded(bound: u64, duplicate: Ppm) -> Network {
    let path = LinkProfile::new(1, 1, Ppm::NEVER, duplicate).unwrap();
    Network::new(
        NetConfig {
            peer: path,
            client: path,
            queue: QueueBound::of(&local()),
            max_in_flight: EventBudget::new(bound),
        },
        SplitMix64::new(1),
    )
}

/// Every packet sent and every extra copy made is delivered, dropped or still in flight.
fn assert_counts_settle(net: &Network) {
    let c = net.counts();
    let fates = c.delivered + c.lost + c.cut_at_send + c.cut_in_flight + c.queue_full;
    assert_eq!(
        c.sent + c.duplicated,
        fates + u64::try_from(net.in_flight()).unwrap(),
        "rule: every packet made is delivered, dropped or in flight (br-xoqp); counts={c:?}"
    );
}

#[test]
fn a_send_past_the_in_flight_bound_is_refused_and_the_run_is_incomplete() {
    let mut net = bounded(2, Ppm::NEVER);
    let mut arrivals = Vec::new();
    for _ in 0..2 {
        arrivals.extend(
            net.send(VirtualTime::ZERO, node(1), node(2), b"m")
                .unwrap()
                .arrivals,
        );
    }
    let full = Err(NetError::InFlightFull {
        bound: EventBudget::new(2),
    });
    assert_eq!(
        net.send(VirtualTime::ZERO, node(1), node(2), b"m"),
        full,
        "rule: past the run's event budget a send is refused, never dropped silently (CD-9, \
         §7.11, br-n6is)"
    );
    assert_eq!(
        (net.in_flight(), net.incomplete(), net.counts().sent),
        (2, true, 2),
        "rule: nothing was inserted or counted, and the run is incomplete (P10)"
    );
    deliver_all(&mut net, &mut arrivals);
    assert_eq!(
        net.send(VirtualTime::ZERO, node(1), node(2), b"m"),
        full,
        "rule: once incomplete, the run stays incomplete, even with room again"
    );
    assert_counts_settle(&net);
}

#[test]
fn a_duplicated_send_never_puts_some_copies_in_flight_and_drops_others() {
    // br-xoqp: one below the bound, a duplicated send used to insert its original and
    // drop its duplicate, reporting the packet dropped while the original was in flight.
    let mut net = bounded(3, Ppm::ALWAYS);
    let first = net.send(VirtualTime::ZERO, node(1), node(2), b"m").unwrap();
    assert_eq!(
        (first.arrivals.len(), first.dropped),
        (2, None),
        "fixture: the first send fits both copies"
    );
    assert_eq!(
        net.send(VirtualTime::ZERO, node(1), node(2), b"m"),
        Err(NetError::InFlightFull {
            bound: EventBudget::new(3)
        }),
        "rule: a send whose copies do not all fit is refused whole (br-xoqp)"
    );
    assert_eq!(
        (net.in_flight(), net.load(id(1), id(2)).0),
        (2, 1),
        "rule: no copy of the refused send was inserted or took a link slot"
    );
    assert_counts_settle(&net);
}

#[test]
fn every_drop_reason_and_copy_reads_differently_in_the_trace() {
    let final_line = |sent: &crate::net::Sent| sent.records.last().unwrap().to_string();
    let mut chancy = network(
        LinkProfile::new(1, 1, Ppm::ALWAYS, Ppm::NEVER).unwrap(),
        LinkProfile::reliable(1),
        1,
    );
    let lost = chancy
        .send(VirtualTime::ZERO, node(1), node(2), b"m")
        .unwrap();
    let mut cut = reliable(1);
    cut.links_mut().cut(id(1), id(2));
    let at_send = cut.send(VirtualTime::ZERO, node(1), node(2), b"m").unwrap();
    let mut tight = Network::new(
        NetConfig {
            peer: LinkProfile::reliable(1),
            client: LinkProfile::reliable(1),
            queue: QueueBound {
                messages: ItemCount::new(1),
                bytes: ByteCount::new(1 << 20),
            },
            max_in_flight: EventBudget::DEFAULT,
        },
        SplitMix64::new(1),
    );
    tight
        .send(VirtualTime::ZERO, node(1), node(2), b"m")
        .unwrap();
    let full = tight
        .send(VirtualTime::ZERO, node(1), node(2), b"m")
        .unwrap();
    let doubled = || {
        network(
            LinkProfile::new(1, 1, Ppm::NEVER, Ppm::ALWAYS).unwrap(),
            LinkProfile::reliable(1),
            1,
        )
    };
    let mut severed = doubled();
    let both = severed
        .send(VirtualTime::ZERO, node(1), node(2), b"m")
        .unwrap();
    severed.links_mut().cut(id(1), id(2));
    let mut dropped_copies = Vec::new();
    for &(_, copy) in &both.arrivals {
        let Arrival::Dropped(record) = severed.arrive(copy) else {
            panic!("fixture: the cut drops both copies in flight");
        };
        dropped_copies.push(record.to_string());
    }
    let mut kept = doubled();
    let twice = kept
        .send(VirtualTime::ZERO, node(1), node(2), b"m")
        .unwrap();
    let mut delivered_copies = Vec::new();
    for &(_, copy) in &twice.arrivals {
        let Arrival::Delivered(_, record) = kept.arrive(copy) else {
            panic!("fixture: both copies are delivered");
        };
        delivered_copies.push(record.to_string());
    }
    assert_eq!(
        [
            final_line(&lost),
            final_line(&at_send),
            final_line(&full),
            dropped_copies[0].clone(),
            dropped_copies[1].clone(),
            both.records[1].to_string(),
            delivered_copies[0].clone(),
            delivered_copies[1].clone(),
        ],
        [
            "packet dropped:lost n1 n2 6d",
            "packet dropped:cut_at_send n1 n2 6d",
            "packet dropped:queue_full n1 n2 6d",
            "packet dropped:cut_in_flight n1 n2 6d",
            "packet dropped:cut_in_flight/dup n1 n2 6d",
            "packet duplicated/dup n1 n2 6d",
            "packet delivered n1 n2 6d",
            "packet delivered/dup n1 n2 6d",
        ]
        .map(str::to_owned),
        "rule: every drop names its reason and a duplicate's records are marked, so runs that \
         differ in a packet's fate differ at that packet (T-11, K-14, br-ktgc)"
    );
}

#[test]
fn the_counts_reach_the_trace_as_metrics_and_settle() {
    let mut net = network(
        LinkProfile::new(
            0,
            50,
            Ppm::new(200_000).unwrap(),
            Ppm::new(200_000).unwrap(),
        )
        .unwrap(),
        LinkProfile::reliable(1),
        5,
    );
    let mut arrivals = Vec::new();
    for at in 0..200_u64 {
        if at == 100 {
            net.links_mut().cut(id(2), id(1));
        }
        let (from, to) = if at % 2 == 0 {
            (node(1), node(2))
        } else {
            (node(2), node(1))
        };
        arrivals.extend(
            net.send(VirtualTime::ZERO, from, to, b"m")
                .unwrap()
                .arrivals,
        );
    }
    deliver_all(&mut net, &mut arrivals);
    assert_counts_settle(&net);
    let c = net.counts();
    assert!(
        c.lost > 0 && c.duplicated > 0 && c.cut_at_send > 0 && c.cut_in_flight > 0,
        "fixture: the run loses, duplicates and cuts at send and in flight; counts={c:?}"
    );
    let mut trace = Trace::new(ObservationBound::new(16));
    net.record_metrics(&mut trace);
    let canonical = trace.canonical();
    for (name, value) in [
        ("net.sent", c.sent),
        ("net.delivered", c.delivered),
        ("net.duplicated", c.duplicated),
        ("net.lost", c.lost),
        ("net.cut_at_send", c.cut_at_send),
        ("net.cut_in_flight", c.cut_in_flight),
        ("net.queue_full", c.queue_full),
        ("net.in_flight", 0),
    ] {
        assert!(
            canonical.contains(&format!("M {name} {value}\n")),
            "rule: the network's counts are deterministic metrics in the trace (§10.2, §7.11, \
             br-ktgc); missing {name}={value}; canonical={canonical:?}"
        );
    }
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
    let mut sent = net
        .send(
            VirtualTime::ZERO,
            Endpoint::Node(from),
            Endpoint::Node(to),
            &envelope,
        )
        .unwrap();
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
