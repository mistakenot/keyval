//! The simulated network (PLAN.md §9.1, §10.2; W16.1): the fault surface for loss, delay,
//! reordering, duplication and partitions.
//!
//! The network carries the bytes it is given, the same sealed peer envelopes production
//! sends (§8.2, P3), between logical [`Endpoint`]s. It does not own the scheduler: a send
//! hands back one arrival per copy, each a time and a [`PacketId`], for the caller to
//! schedule as its own event, and [`Network::arrive`] says what became of it.
//!
//! - **Randomness** comes only from the run's [`Subsystem::Network`](crate::Subsystem)
//!   stream, drawn in a fixed order per send: loss, then duplication, then one delay per
//!   copy. The same seed gives the same fates (T-3).
//! - **Delay and reordering.** Every copy gets its own delay, uniform over its path's
//!   range, so delivery follows delay, not send order: the network is never FIFO (§9.3).
//! - **Loss and duplication** have their own probabilities, in parts per million, on peer
//!   links ([`NetConfig::peer`]) and on client paths ([`NetConfig::client`], so a response
//!   can be lost after a committed application, §9.1). A duplicate is an independent copy
//!   with its own delay.
//! - **Partitions.** A directed [`LinkMatrix`] cuts node-to-node links one way or both:
//!   symmetric splits and asymmetric cuts alike. A packet is dropped if its link is cut when
//!   it is sent or when it arrives, so a partition also catches traffic in flight. Clients
//!   are never partitioned.
//! - **Bounded queues** (§7.11, CD-9). Each directed node pair carries at most
//!   `peer_send_queue_messages` packets and `peer_send_queue_bytes` bytes in flight. A send
//!   past either bound is dropped and counted, never queued: the queue never grows. A
//!   duplicate is the network's own copy and takes no sender slot.
//! - **Copies in flight** are bounded by the run's event budget ([`NetConfig::max_in_flight`],
//!   an [`EventBudget`], whose default is the registered `Limit::SimEventsPerRun`, §7.11):
//!   each copy in flight is one pending arrival event. A send whose copies would not all fit
//!   is refused with [`NetError::InFlightFull`]: nothing is inserted or counted, and the run
//!   is incomplete from then on, as when the scheduler's queue is full (P10). It is never a
//!   silent drop, and no copy is ever in flight while its send reads as dropped (br-n6is,
//!   br-xoqp).
//! - **Packets are not tied to incarnations** (§9.1): one addressed to a node reaches
//!   whatever incarnation is running when it arrives, as an ordinary stale message.
//!
//! Every fate is counted ([`NetCounts`]) and traced as a [`Record::Packet`]: a drop names its
//! [`DropReason`], and a duplicate's records are marked as the duplicate's
//! ([`PacketCopy`]), so two runs that differ in a packet's fate differ at that packet
//! (T-11). [`Network::record_metrics`] writes the counts into the trace (br-ktgc).

use std::collections::{BTreeMap, BTreeSet};

use types::{BoundedMap, ByteCount, Config, ItemCount, NodeId};

use crate::entropy::SplitMix64;
use crate::scheduler::{EventBudget, VirtualTime};
pub use crate::trace::DropReason;
use crate::trace::{Endpoint, Name, PacketCopy, PacketFate, Record, Trace};

/// A probability in parts per million: exact and integer, so it draws the same on every
/// platform.
///
/// Invariant: at most 1,000,000.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ppm(u32);

impl Ppm {
    /// Never.
    pub const NEVER: Self = Self(0);
    /// Always.
    pub const ALWAYS: Self = Self(1_000_000);

    /// `ppm` parts per million, or `None` above 1,000,000.
    #[must_use]
    pub const fn new(ppm: u32) -> Option<Self> {
        if ppm <= 1_000_000 {
            Some(Self(ppm))
        } else {
            None
        }
    }

    /// The parts per million.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// How one kind of path treats each message: its delay range in virtual microseconds and
/// its loss and duplication probabilities.
///
/// Invariant: the shortest delay is at most the longest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkProfile {
    /// The shortest delay, in virtual microseconds.
    min_delay: u64,
    /// The longest delay, in virtual microseconds.
    max_delay: u64,
    /// The chance a message is lost.
    loss: Ppm,
    /// The chance a message is delivered twice.
    duplicate: Ppm,
}

impl LinkProfile {
    /// A profile delaying each message between `min_delay` and `max_delay` virtual
    /// microseconds, losing it with probability `loss` and duplicating it with probability
    /// `duplicate`, or `None` if `min_delay` exceeds `max_delay`.
    #[must_use]
    pub const fn new(min_delay: u64, max_delay: u64, loss: Ppm, duplicate: Ppm) -> Option<Self> {
        if min_delay <= max_delay {
            Some(Self {
                min_delay,
                max_delay,
                loss,
                duplicate,
            })
        } else {
            None
        }
    }

    /// A path that delivers every message once, after exactly `delay` microseconds.
    #[must_use]
    pub const fn reliable(delay: u64) -> Self {
        Self {
            min_delay: delay,
            max_delay: delay,
            loss: Ppm::NEVER,
            duplicate: Ppm::NEVER,
        }
    }
}

/// The bound on each directed peer link's packets in flight (§7.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueBound {
    /// At most this many packets.
    pub messages: ItemCount,
    /// At most this many bytes.
    pub bytes: ByteCount,
}

impl QueueBound {
    /// The bound `config` sets: `peer_send_queue_messages` and `peer_send_queue_bytes`.
    #[must_use]
    pub const fn of(config: &Config) -> Self {
        Self {
            messages: config.peer_send_queue_messages(),
            bytes: config.peer_send_queue_bytes(),
        }
    }
}

/// The network's settings for one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetConfig {
    /// Node-to-node links.
    pub peer: LinkProfile,
    /// Paths to and from clients.
    pub client: LinkProfile,
    /// Each directed peer link's bound.
    pub queue: QueueBound,
    /// The most copies in flight at once: the run's event budget (CD-9, §7.11). Each copy
    /// in flight is one pending arrival event, so the network never holds more than the
    /// scheduler could; [`EventBudget::DEFAULT`] is the registered `Limit::SimEventsPerRun`.
    /// A send past it is refused and makes the run incomplete ([`NetError::InFlightFull`]).
    pub max_in_flight: EventBudget,
}

/// One copy of a packet in flight. Each copy has its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PacketId(u64);

impl PacketId {
    /// The identifier's number: copies are numbered from 0 in send order.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A packet the network has delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// Its source.
    pub from: Endpoint,
    /// Its destination.
    pub to: Endpoint,
    /// Its bytes, as sent.
    pub bytes: Vec<u8>,
}

/// Why a send could not be made at all (§7.11, P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// The packet's copies would not all fit within the run's event budget
    /// ([`NetConfig::max_in_flight`]), now or at an earlier send. Nothing was inserted or
    /// counted, and the run is incomplete from then on, which fails (P10, br-n6is).
    InFlightFull {
        /// The bound: the run's event budget.
        bound: EventBudget,
    },
}

/// What became of every packet so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetCounts {
    /// Packets sent.
    pub sent: u64,
    /// Copies delivered.
    pub delivered: u64,
    /// Extra copies made.
    pub duplicated: u64,
    /// Packets lost by chance.
    pub lost: u64,
    /// Packets dropped because their link was cut when sent.
    pub cut_at_send: u64,
    /// Copies dropped because their link was cut while in flight.
    pub cut_in_flight: u64,
    /// Packets dropped on a full link queue.
    pub queue_full: u64,
}

impl NetCounts {
    /// Counts one drop for `reason`.
    const fn record(&mut self, reason: DropReason) {
        let count = match reason {
            DropReason::Lost => &mut self.lost,
            DropReason::CutAtSend => &mut self.cut_at_send,
            DropReason::CutInFlight => &mut self.cut_in_flight,
            DropReason::QueueFull => &mut self.queue_full,
        };
        *count = count.saturating_add(1);
    }
}

/// What a send did: the arrivals to schedule, one per copy, and the trace records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    /// When each copy arrives, and which copy it is.
    pub arrivals: Vec<(VirtualTime, PacketId)>,
    /// Why the packet was dropped at once, if it was.
    pub dropped: Option<DropReason>,
    /// The packet's trace records: sent, then dropped or duplicated.
    pub records: Vec<Record>,
}

/// What an arrival did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The copy was delivered.
    Delivered(Delivery, Record),
    /// The copy's link was cut while it was in flight.
    Dropped(Record),
    /// No copy with that identifier is in flight: it already arrived.
    Unknown,
}

/// Which directed node-to-node links are cut (§9.1 partitions).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkMatrix {
    /// The cut links, as (from, to).
    cut: BTreeSet<(NodeId, NodeId)>,
}

impl LinkMatrix {
    /// Cuts the link from `from` to `to` only: messages the other way still pass.
    pub fn cut(&mut self, from: NodeId, to: NodeId) {
        self.cut.insert((from, to));
    }

    /// Restores the link from `from` to `to`.
    pub fn heal(&mut self, from: NodeId, to: NodeId) {
        self.cut.remove(&(from, to));
    }

    /// Restores every link.
    pub fn heal_all(&mut self) {
        self.cut.clear();
    }

    /// Splits `groups` from each other: every link between nodes of different groups is
    /// cut, both ways. Links within a group, and links of nodes in no group, are untouched.
    pub fn partition(&mut self, groups: &[&[NodeId]]) {
        for (at, group) in groups.iter().enumerate() {
            for other in groups.iter().skip(at + 1) {
                for &a in *group {
                    for &b in *other {
                        self.cut(a, b);
                        self.cut(b, a);
                    }
                }
            }
        }
    }

    /// Whether a message from `from` reaches `to`.
    #[must_use]
    pub fn allows(&self, from: NodeId, to: NodeId) -> bool {
        !self.cut.contains(&(from, to))
    }
}

/// One copy in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InFlight {
    /// Its source.
    from: Endpoint,
    /// Its destination.
    to: Endpoint,
    /// Its bytes.
    bytes: Vec<u8>,
    /// Whether it holds a slot in its link's queue: the original copy on a peer link.
    queued: bool,
    /// Which copy it is.
    copy: PacketCopy,
}

/// A directed peer link's load: the packets and bytes it holds in flight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Load {
    /// Packets in flight.
    messages: u64,
    /// Bytes in flight.
    bytes: u64,
}

/// The simulated network of one run.
///
/// Invariants: every link's load is within [`NetConfig::queue`] and is the sum over its
/// queued copies in flight (`NET-LOAD`, asserted on every hold and every free); at most
/// [`NetConfig::max_in_flight`] copies are in flight (`NET-IN-FLIGHT`); every packet sent and
/// every extra copy made is delivered, dropped or still in flight.
#[derive(Clone, Debug)]
pub struct Network {
    /// Its settings.
    config: NetConfig,
    /// Which links are cut.
    links: LinkMatrix,
    /// The run's network stream.
    rng: SplitMix64,
    /// Each directed peer link's load. Only node pairs that have sent hold an entry: at
    /// most one per ordered pair of voters (five nodes: 20).
    loads: BTreeMap<(NodeId, NodeId), Load>,
    /// The copies in flight.
    in_flight: BoundedMap<PacketId, InFlight>,
    /// The next copy's identifier.
    next: u64,
    /// What became of every packet so far.
    counts: NetCounts,
    /// Whether a send was refused for want of room in flight: the run is incomplete (P10).
    incomplete: bool,
}

/// The packet trace record for `fate`.
fn record(
    fate: PacketFate,
    copy: PacketCopy,
    from: Endpoint,
    to: Endpoint,
    bytes: &[u8],
) -> Record {
    Record::Packet {
        fate,
        copy,
        from,
        to,
        bytes: bytes.to_vec(),
    }
}

/// `len` as a byte count.
fn byte_len(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

impl Network {
    /// A network with `config`, drawing from `rng`, the run's
    /// [`Subsystem::Network`](crate::Subsystem::Network) stream, with every link up.
    #[must_use]
    pub fn new(config: NetConfig, rng: SplitMix64) -> Self {
        let max_in_flight = usize::try_from(config.max_in_flight.get()).unwrap_or(usize::MAX);
        Self {
            config,
            links: LinkMatrix::default(),
            rng,
            loads: BTreeMap::new(),
            in_flight: BoundedMap::new(max_in_flight),
            next: 0,
            counts: NetCounts::default(),
            incomplete: false,
        }
    }

    /// The link matrix, to cut and heal links.
    pub const fn links_mut(&mut self) -> &mut LinkMatrix {
        &mut self.links
    }

    /// The link matrix.
    #[must_use]
    pub const fn links(&self) -> &LinkMatrix {
        &self.links
    }

    /// Replaces the peer and client path profiles for every later send: a run's fault-free
    /// baseline, its faults and their healing, scheduled mid-run (§10.2, §10.16; W37's
    /// nemesis). A copy already in flight keeps the arrival time drawn when it was sent and
    /// is delivered or cut as before. The link-queue and in-flight bounds stay the run's.
    /// The change draws nothing, so the same changes at the same points replay identically
    /// (T-3).
    pub const fn set_profiles(&mut self, peer: LinkProfile, client: LinkProfile) {
        self.config.peer = peer;
        self.config.client = client;
    }

    /// The peer and client path profiles in force.
    #[must_use]
    pub const fn profiles(&self) -> (LinkProfile, LinkProfile) {
        (self.config.peer, self.config.client)
    }

    /// What became of every packet so far.
    #[must_use]
    pub const fn counts(&self) -> NetCounts {
        self.counts
    }

    /// Copies in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether a send was refused for want of room in flight: the run is incomplete, which
    /// fails (P10, br-n6is).
    #[must_use]
    pub const fn incomplete(&self) -> bool {
        self.incomplete
    }

    /// Writes what became of every packet so far into `trace` as deterministic metrics
    /// (§10.2), so §7.11's network failures are in the canonical bytes: sent, delivered,
    /// duplicated, each drop reason, and the copies still in flight (br-ktgc).
    pub fn record_metrics(&self, trace: &mut Trace) {
        let c = self.counts;
        let in_flight = u64::try_from(self.in_flight.len()).unwrap_or(u64::MAX);
        for (name, value) in [
            ("net.sent", c.sent),
            ("net.delivered", c.delivered),
            ("net.duplicated", c.duplicated),
            ("net.lost", c.lost),
            ("net.cut_at_send", c.cut_at_send),
            ("net.cut_in_flight", c.cut_in_flight),
            ("net.queue_full", c.queue_full),
            ("net.in_flight", in_flight),
        ] {
            let metric = Name::new(name);
            types::invariant!(
                metric.is_some(),
                "NET-METRIC-NAME",
                "{name} is not a valid trace name"
            );
            if let Some(metric) = metric {
                trace.metric(metric, value);
            }
        }
    }

    /// The packets and bytes in flight on the link from `from` to `to`.
    #[must_use]
    pub fn load(&self, from: NodeId, to: NodeId) -> (u64, u64) {
        self.loads
            .get(&(from, to))
            .map_or((0, 0), |load| (load.messages, load.bytes))
    }

    /// Sends `bytes` from `from` to `to` at `now` (module documentation).
    ///
    /// # Errors
    ///
    /// [`NetError::InFlightFull`] if the packet's copies would not all fit within
    /// [`NetConfig::max_in_flight`], now or at an earlier send: nothing is inserted or
    /// counted, and the run is incomplete from then on (P10, br-n6is).
    pub fn send(
        &mut self,
        now: VirtualTime,
        from: Endpoint,
        to: Endpoint,
        bytes: &[u8],
    ) -> Result<Sent, NetError> {
        let full = NetError::InFlightFull {
            bound: self.config.max_in_flight,
        };
        if self.incomplete {
            return Err(full);
        }
        let mut sent = Sent::default();
        sent.records.push(record(
            PacketFate::Sent,
            PacketCopy::Original,
            from,
            to,
            bytes,
        ));
        let link = match (from, to) {
            (Endpoint::Node(a), Endpoint::Node(b)) => Some((a, b)),
            (Endpoint::Node(_) | Endpoint::Client(_), Endpoint::Client(_))
            | (Endpoint::Client(_), Endpoint::Node(_)) => None,
        };
        let profile = if link.is_some() {
            self.config.peer
        } else {
            self.config.client
        };
        let len = byte_len(bytes.len());
        let refusal = if link.is_some_and(|link| !self.fits(link, len)) {
            Some(DropReason::QueueFull)
        } else if link.is_some_and(|(a, b)| !self.links.allows(a, b)) {
            Some(DropReason::CutAtSend)
        } else if self.chance(profile.loss) {
            Some(DropReason::Lost)
        } else {
            None
        };
        if let Some(reason) = refusal {
            self.counts.sent = self.counts.sent.saturating_add(1);
            self.counts.record(reason);
            sent.dropped = Some(reason);
            sent.records.push(record(
                PacketFate::Dropped(reason),
                PacketCopy::Original,
                from,
                to,
                bytes,
            ));
            return Ok(sent);
        }
        let copies: usize = if self.chance(profile.duplicate) { 2 } else { 1 };
        // Room for every copy, or none is inserted (br-n6is, br-xoqp).
        let bound = usize::try_from(self.config.max_in_flight.get()).unwrap_or(usize::MAX);
        if copies > bound.saturating_sub(self.in_flight.len()) {
            self.incomplete = true;
            return Err(full);
        }
        self.counts.sent = self.counts.sent.saturating_add(1);
        for copy in 0..copies {
            let at = VirtualTime::from_micros(now.as_micros().saturating_add(self.delay(profile)));
            let queued = copy == 0 && link.is_some();
            let id = PacketId(self.next);
            self.next = self.next.saturating_add(1);
            let packet = InFlight {
                from,
                to,
                bytes: bytes.to_vec(),
                queued,
                copy: if copy == 0 {
                    PacketCopy::Original
                } else {
                    PacketCopy::Duplicate
                },
            };
            let inserted = self.in_flight.insert(id, packet).is_ok();
            types::invariant!(
                inserted,
                "NET-IN-FLIGHT",
                "copy {} of a packet from {from} to {to} did not fit in flight after room for \
                 {copies} copies was checked",
                id.get()
            );
            if let Some(link) = link.filter(|_| queued) {
                self.hold(link, len);
            }
            if copy > 0 {
                self.counts.duplicated = self.counts.duplicated.saturating_add(1);
                sent.records.push(record(
                    PacketFate::Duplicated,
                    PacketCopy::Duplicate,
                    from,
                    to,
                    bytes,
                ));
            }
            sent.arrivals.push((at, id));
        }
        Ok(sent)
    }

    /// The copy `id` arrives: delivered, or dropped if its link has been cut meanwhile.
    pub fn arrive(&mut self, id: PacketId) -> Arrival {
        let Some(packet) = self.in_flight.remove(&id) else {
            return Arrival::Unknown;
        };
        let link = match (packet.from, packet.to) {
            (Endpoint::Node(a), Endpoint::Node(b)) => Some((a, b)),
            (Endpoint::Node(_) | Endpoint::Client(_), Endpoint::Client(_))
            | (Endpoint::Client(_), Endpoint::Node(_)) => None,
        };
        if let Some(link) = link.filter(|_| packet.queued) {
            self.free(link, byte_len(packet.bytes.len()), id);
        }
        if link.is_some_and(|(a, b)| !self.links.allows(a, b)) {
            self.counts.record(DropReason::CutInFlight);
            return Arrival::Dropped(record(
                PacketFate::Dropped(DropReason::CutInFlight),
                packet.copy,
                packet.from,
                packet.to,
                &packet.bytes,
            ));
        }
        self.counts.delivered = self.counts.delivered.saturating_add(1);
        let traced = record(
            PacketFate::Delivered,
            packet.copy,
            packet.from,
            packet.to,
            &packet.bytes,
        );
        Arrival::Delivered(
            Delivery {
                from: packet.from,
                to: packet.to,
                bytes: packet.bytes,
            },
            traced,
        )
    }

    /// Whether the link `link` has room for one more packet of `len` bytes.
    fn fits(&self, link: (NodeId, NodeId), len: u64) -> bool {
        let load = self.loads.get(&link).copied().unwrap_or_default();
        load.messages < self.config.queue.messages.get()
            && load.bytes.saturating_add(len) <= self.config.queue.bytes.get()
    }

    /// Adds a queued copy of `len` bytes to `link`'s load. [`Self::fits`] checked the room
    /// before the send, so the load stays within the bound (`NET-LOAD`, br-byxl).
    fn hold(&mut self, link: (NodeId, NodeId), len: u64) {
        let bound = self.config.queue;
        let load = self.loads.entry(link).or_default();
        load.messages = load.messages.saturating_add(1);
        load.bytes = load.bytes.saturating_add(len);
        types::invariant!(
            load.messages <= bound.messages.get() && load.bytes <= bound.bytes.get(),
            "NET-LOAD",
            "link n{}->n{} holds {} packets and {} bytes in flight, past its bound of {} and {}",
            link.0,
            link.1,
            load.messages,
            load.bytes,
            bound.messages.get(),
            bound.bytes.get()
        );
    }

    /// Frees queued copy `id`'s slot and `len` bytes from `link`'s load. They were held when
    /// it was sent and only its arrival frees them, so a shortfall is broken accounting, never
    /// clamped (`NET-LOAD`, br-byxl).
    fn free(&mut self, link: (NodeId, NodeId), len: u64, id: PacketId) {
        let load = self.loads.entry(link).or_default();
        types::invariant!(
            load.messages >= 1 && load.bytes >= len,
            "NET-LOAD",
            "link n{}->n{} frees copy {} of {len} bytes but holds only {} packets and {} bytes \
             in flight",
            link.0,
            link.1,
            id.get(),
            load.messages,
            load.bytes
        );
        load.messages -= 1;
        load.bytes -= len;
    }

    /// One draw: true with probability `p`.
    fn chance(&mut self, p: Ppm) -> bool {
        self.rng.next_u64() % 1_000_000 < u64::from(p.get())
    }

    /// One delay from `profile`'s range.
    const fn delay(&mut self, profile: LinkProfile) -> u64 {
        let span = profile.max_delay - profile.min_delay;
        let draw = self.rng.next_u64();
        let offset = if span == u64::MAX {
            draw
        } else {
            draw % (span + 1)
        };
        profile.min_delay + offset
    }
}
