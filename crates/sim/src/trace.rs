//! The canonical trace (PLAN.md §10.2; T-3, T-11): a run's record of what happened,
//! byte-identical for the same seed and build.
//!
//! - One line per entry, with fields in a fixed order, integers in decimal and bytes in
//!   lowercase hex (`-` when empty). Event lines (`E`) mark each dispatched scheduler
//!   event; record lines (`R`) carry the step they belong to; metric lines (`M`) close the
//!   trace in name order.
//! - Endpoints are logical: `n<node>` or `n<node>/<incarnation>` for a node, `c<client>`
//!   for a client. Host data (PIDs, wall-clock times, paths, memory addresses) has no
//!   field, so it cannot enter; protocol decisions are never left out.
//! - Diagnostics are free text that may carry host data, so they stay out of the
//!   canonical bytes. They are counted, and dropped past the bound.
//! - Required observations are bounded per step by the structured-log queue (4,096
//!   records, §5 limits). Overflowing the bound fails closed: the observation is refused
//!   and the run is incomplete (P10). Oracle evidence is never silently dropped.
//! - The hash (64-bit FNV-1a over the canonical bytes) only speeds up a mismatch check.
//!   [`compare`] always checks every line, metrics included, and reports the first
//!   difference; callers keep both traces.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use types::{LogIndex, NodeId, RequestId, Role, Term};

use crate::scheduler::{Event, EventSeq, NodeIncarnation};

/// A name in the trace (an event kind, a metric or a checker): 1 to 64 ASCII letters,
/// digits, `_`, `-` or `.`, so it encodes verbatim and carries no host data.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    /// `text` as a name, or `None` unless it is 1 to 64 ASCII letters, digits, `_`, `-`
    /// or `.`.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let valid = (1..=64).contains(&text.len())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        valid.then(|| Self(text.to_owned()))
    }

    /// The name's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A logical packet endpoint: never a socket address or a host name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Endpoint {
    /// A node.
    Node(NodeId),
    /// A simulated client, numbered within the run.
    Client(u64),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Node(node) => write!(f, "n{}", node.get()),
            Self::Client(client) => write!(f, "c{client}"),
        }
    }
}

/// What happened to a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketFate {
    /// It left its source.
    Sent,
    /// It reached its destination.
    Delivered,
    /// The network lost it.
    Dropped,
    /// The network delivered an extra copy.
    Duplicated,
}

impl PacketFate {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Dropped => "dropped",
            Self::Duplicated => "duplicated",
        }
    }
}

/// A simulated disk operation (§10.2 event model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskOp {
    /// Append progress: `len` bytes at `offset` reached the device.
    Append {
        /// The byte offset.
        offset: u64,
        /// The byte count.
        len: u64,
    },
    /// Everything appended up to byte `upto` became durable.
    Sync {
        /// The durable length.
        upto: u64,
    },
    /// The sync's completion notification was delivered.
    SyncDone {
        /// The durable length it reports.
        upto: u64,
    },
}

impl fmt::Display for DiskOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append { offset, len } => write!(f, "append {offset} {len}"),
            Self::Sync { upto } => write!(f, "sync {upto}"),
            Self::SyncDone { upto } => write!(f, "sync-done {upto}"),
        }
    }
}

/// A node incarnation as `n<node>/<incarnation>`.
struct On(NodeIncarnation);

impl fmt::Display for On {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "n{}/{}", self.0.node.get(), self.0.incarnation.get())
    }
}

/// Bytes as lowercase hex, or `-` when there are none.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("-");
        }
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// One canonical record (§10.2 "Contains").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// A packet between logical endpoints, with its encoded bytes.
    Packet {
        /// What happened to it.
        fate: PacketFate,
        /// Its source.
        from: Endpoint,
        /// Its destination.
        to: Endpoint,
        /// Its encoded bytes.
        bytes: Vec<u8>,
    },
    /// A disk operation or completion.
    Disk {
        /// The node incarnation whose disk it is.
        node: NodeIncarnation,
        /// What happened.
        op: DiskOp,
    },
    /// A node's role and term.
    Role {
        /// The node incarnation.
        node: NodeIncarnation,
        /// Its role.
        role: Role,
        /// Its current term.
        term: Term,
    },
    /// A node's last log entry.
    Log {
        /// The node incarnation.
        node: NodeIncarnation,
        /// The last entry's index (zero when the log is empty).
        last_index: LogIndex,
        /// The last entry's term (zero when the log is empty).
        last_term: Term,
    },
    /// A node's commit index.
    Commit {
        /// The node incarnation.
        node: NodeIncarnation,
        /// Its commit index.
        index: LogIndex,
    },
    /// A node's applied index.
    Applied {
        /// The node incarnation.
        node: NodeIncarnation,
        /// Its applied index.
        index: LogIndex,
    },
    /// A node's journal: how far appended and how far durable.
    Journal {
        /// The node incarnation.
        node: NodeIncarnation,
        /// The last appended index.
        appended: LogIndex,
        /// The last durable index.
        synced: LogIndex,
    },
    /// A client invoked an operation.
    Invoked {
        /// The client.
        client: u64,
        /// The request ID.
        request: RequestId,
        /// The encoded operation.
        op: Vec<u8>,
    },
    /// A client received a response.
    Responded {
        /// The client.
        client: u64,
        /// The request ID.
        request: RequestId,
        /// The encoded response.
        result: Vec<u8>,
    },
    /// A request's first execution in the replicated log.
    FirstExecution {
        /// The request ID.
        request: RequestId,
        /// The log index that executed it.
        index: LogIndex,
    },
    /// A checker's verdict.
    Verdict {
        /// The checker.
        checker: Name,
        /// Whether it passed.
        passed: bool,
    },
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packet {
                fate,
                from,
                to,
                bytes,
            } => write!(f, "packet {} {from} {to} {}", fate.as_str(), Hex(bytes)),
            Self::Disk { node, op } => write!(f, "disk {} {op}", On(*node)),
            Self::Role { node, role, term } => {
                write!(f, "role {} {} {}", On(*node), role.as_str(), term.get())
            }
            Self::Log {
                node,
                last_index,
                last_term,
            } => write!(
                f,
                "log {} {} {}",
                On(*node),
                last_index.get(),
                last_term.get()
            ),
            Self::Commit { node, index } => write!(f, "commit {} {}", On(*node), index.get()),
            Self::Applied { node, index } => write!(f, "applied {} {}", On(*node), index.get()),
            Self::Journal {
                node,
                appended,
                synced,
            } => write!(
                f,
                "journal {} {} {}",
                On(*node),
                appended.get(),
                synced.get()
            ),
            Self::Invoked {
                client,
                request,
                op,
            } => write!(f, "invoke c{client} {request} {}", Hex(op)),
            Self::Responded {
                client,
                request,
                result,
            } => write!(f, "respond c{client} {request} {}", Hex(result)),
            Self::FirstExecution { request, index } => {
                write!(f, "first {request} {}", index.get())
            }
            Self::Verdict { checker, passed } => write!(
                f,
                "verdict {} {}",
                checker.as_str(),
                if *passed { "pass" } else { "fail" }
            ),
        }
    }
}

/// The bound on required observations, and on diagnostics, per step (§5 limits: the
/// structured-log queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationBound(usize);

impl ObservationBound {
    /// The structured-log queue's bound: 4,096 records.
    pub const DEFAULT: Self = Self(4_096);

    /// A bound of `records` per step.
    #[must_use]
    pub const fn new(records: usize) -> Self {
        Self(records)
    }

    /// Records per step.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

/// A required observation the step had no room for. The run is incomplete (P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow {
    /// The bound that was reached.
    pub bound: ObservationBound,
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a step exceeded its {} required observations; the run is incomplete",
            self.bound.get()
        )
    }
}

impl Error for Overflow {}

/// A run's canonical trace, built step by step.
#[derive(Clone, Debug)]
pub struct Trace {
    lines: Vec<String>,
    metrics: BTreeMap<Name, u64>,
    bound: ObservationBound,
    step: Option<EventSeq>,
    recorded_in_step: usize,
    diagnosed_in_step: usize,
    diagnostics: Vec<String>,
    dropped_diagnostics: u64,
    incomplete: bool,
}

impl Trace {
    /// An empty trace with `bound` per step.
    #[must_use]
    pub const fn new(bound: ObservationBound) -> Self {
        Self {
            lines: Vec::new(),
            metrics: BTreeMap::new(),
            bound,
            step: None,
            recorded_in_step: 0,
            diagnosed_in_step: 0,
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            incomplete: false,
        }
    }

    /// Starts the step of a dispatched event: its sequence, virtual time, causal parent,
    /// node incarnation and `kind`. The bounds start again.
    pub fn event<E>(&mut self, event: &Event<E>, kind: &Name) {
        self.step = Some(event.seq);
        self.recorded_in_step = 0;
        self.diagnosed_in_step = 0;
        let parent = event
            .parent
            .map_or_else(|| "-".to_owned(), |parent| parent.get().to_string());
        let node = event
            .node
            .map_or_else(|| "-".to_owned(), |node| On(node).to_string());
        self.lines.push(format!(
            "E {} {} {parent} {node} {}",
            event.seq.get(),
            event.time.as_micros(),
            kind.as_str()
        ));
    }

    /// Records a required observation in the current step (`-` before the first event).
    ///
    /// # Errors
    ///
    /// [`Overflow`] if the step already holds its bound of observations. Nothing is
    /// recorded, and the trace is incomplete from then on.
    pub fn record(&mut self, record: &Record) -> Result<(), Overflow> {
        if self.recorded_in_step >= self.bound.get() {
            self.incomplete = true;
            return Err(Overflow { bound: self.bound });
        }
        self.recorded_in_step += 1;
        let step = self
            .step
            .map_or_else(|| "-".to_owned(), |seq| seq.get().to_string());
        self.lines.push(format!("R {step} {record}"));
        Ok(())
    }

    /// Notes diagnostic text. It never enters the canonical bytes; past the step's bound
    /// it is dropped and counted.
    pub fn diagnostic(&mut self, text: &str) {
        if self.diagnosed_in_step >= self.bound.get() {
            self.dropped_diagnostics += 1;
            return;
        }
        self.diagnosed_in_step += 1;
        self.diagnostics.push(text.to_owned());
    }

    /// Sets a deterministic metric; metrics close the trace in name order.
    pub fn metric(&mut self, name: Name, value: u64) {
        self.metrics.insert(name, value);
    }

    /// Whether every required observation was recorded.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !self.incomplete
    }

    /// The diagnostics kept, outside the canonical trace.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Diagnostics dropped by the bound.
    #[must_use]
    pub const fn dropped_diagnostics(&self) -> u64 {
        self.dropped_diagnostics
    }

    /// The canonical bytes: every line, then the metrics, each ending in a newline.
    #[must_use]
    pub fn canonical(&self) -> String {
        let metrics = self
            .metrics
            .iter()
            .map(|(name, value)| format!("M {} {value}", name.as_str()));
        self.lines
            .iter()
            .cloned()
            .chain(metrics)
            .fold(String::new(), |mut text, line| {
                text.push_str(&line);
                text.push('\n');
                text
            })
    }

    /// The hash of the canonical bytes.
    #[must_use]
    pub fn hash(&self) -> TraceHash {
        TraceHash::of(&self.canonical())
    }
}

/// A trace's 64-bit FNV-1a hash: a fast mismatch test, never proof of equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceHash(u64);

impl TraceHash {
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;

    /// The hash of `canonical`.
    #[must_use]
    pub fn of(canonical: &str) -> Self {
        Self(canonical.bytes().fold(Self::OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(Self::PRIME)
        }))
    }

    /// The 64-bit value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TraceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// How two canonical traces compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comparison {
    /// Every line, metrics included, is the same.
    Identical,
    /// They differ first at `line` (1-based); a side is `None` where its trace ended.
    Differs {
        /// The first differing line.
        line: usize,
        /// The left trace's line.
        left: Option<String>,
        /// The right trace's line.
        right: Option<String>,
    },
}

/// Compares two canonical traces completely, events and metrics alike, and reports the
/// first differing line. The hash never decides equality.
#[must_use]
pub fn compare(left: &str, right: &str) -> Comparison {
    let mut lefts = left.lines();
    let mut rights = right.lines();
    let mut line = 1;
    loop {
        match (lefts.next(), rights.next()) {
            (None, None) => return Comparison::Identical,
            (here, there) if here == there => line += 1,
            (here, there) => {
                return Comparison::Differs {
                    line,
                    left: here.map(str::to_owned),
                    right: there.map(str::to_owned),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // check: SIM-TRACE
    use super::*;
    use crate::entropy::{MasterSeed, Subsystem};
    use crate::scheduler::{EventBudget, Scheduler, Step, VirtualTime};
    use types::Incarnation;

    fn name(text: &str) -> Name {
        Name::new(text).unwrap()
    }

    fn node(raw: u64) -> NodeId {
        NodeId::new(raw).unwrap()
    }

    fn on(raw: u64, incarnation: u64) -> NodeIncarnation {
        NodeIncarnation {
            node: node(raw),
            incarnation: Incarnation::new(incarnation),
        }
    }

    fn request(last: u8) -> RequestId {
        let mut bytes = [0xAB; 16];
        bytes[15] = last;
        RequestId::from_bytes(bytes)
    }

    fn index(raw: u64) -> LogIndex {
        LogIndex::new(raw).unwrap()
    }

    fn term(raw: u64) -> Term {
        Term::new(raw).unwrap()
    }

    /// A trace whose step 4 holds `records`.
    fn trace_of(records: &[Record]) -> Trace {
        let mut scheduler = Scheduler::new(
            MasterSeed::new(1).stream(Subsystem::Scheduler),
            EventBudget::DEFAULT,
        );
        for payload in 0..5_u32 {
            scheduler
                .schedule(VirtualTime::from_micros(u64::from(payload)), None, payload)
                .unwrap();
        }
        let mut trace = Trace::new(ObservationBound::DEFAULT);
        while let Step::Event(event) = scheduler.step() {
            trace.event(&event, &name("tick"));
            if event.seq.get() == 4 {
                for record in records {
                    trace.record(record).unwrap();
                }
            }
        }
        trace
    }

    #[test]
    fn packets_keep_their_logical_endpoints_and_encoded_bytes() {
        let packet = Record::Packet {
            fate: PacketFate::Delivered,
            from: Endpoint::Node(node(2)),
            to: Endpoint::Client(7),
            bytes: vec![0x00, 0xFF, 0x10],
        };
        assert_eq!(
            packet.to_string(),
            "packet delivered n2 c7 00ff10",
            "rule: the trace keeps logical endpoints and the packet's bytes (§10.2)"
        );
        let empty = Record::Packet {
            fate: PacketFate::Dropped,
            from: Endpoint::Client(1),
            to: Endpoint::Node(node(3)),
            bytes: Vec::new(),
        };
        assert_eq!(empty.to_string(), "packet dropped c1 n3 -");
    }

    #[test]
    fn node_records_have_fixed_canonical_lines() {
        let n = on(1, 2);
        let cases = [
            (
                Record::Disk {
                    node: n,
                    op: DiskOp::Append {
                        offset: 4_096,
                        len: 512,
                    },
                },
                "disk n1/2 append 4096 512",
            ),
            (
                Record::Disk {
                    node: n,
                    op: DiskOp::Sync { upto: 4_608 },
                },
                "disk n1/2 sync 4608",
            ),
            (
                Record::Disk {
                    node: n,
                    op: DiskOp::SyncDone { upto: 4_608 },
                },
                "disk n1/2 sync-done 4608",
            ),
            (
                Record::Role {
                    node: n,
                    role: Role::Leader,
                    term: term(5),
                },
                "role n1/2 leader 5",
            ),
            (
                Record::Log {
                    node: n,
                    last_index: index(12),
                    last_term: term(5),
                },
                "log n1/2 12 5",
            ),
            (
                Record::Commit {
                    node: n,
                    index: index(11),
                },
                "commit n1/2 11",
            ),
            (
                Record::Applied {
                    node: n,
                    index: index(10),
                },
                "applied n1/2 10",
            ),
            (
                Record::Journal {
                    node: n,
                    appended: index(12),
                    synced: index(11),
                },
                "journal n1/2 12 11",
            ),
        ];
        for (record, line) in cases {
            assert_eq!(
                record.to_string(),
                line,
                "rule: fixed field order and formats"
            );
        }
    }

    #[test]
    fn client_and_checker_records_have_fixed_canonical_lines() {
        let cases = [
            (
                Record::Invoked {
                    client: 3,
                    request: request(1),
                    op: b"put".to_vec(),
                },
                "invoke c3 ababababababababababababababab01 707574",
            ),
            (
                Record::Responded {
                    client: 3,
                    request: request(1),
                    result: Vec::new(),
                },
                "respond c3 ababababababababababababababab01 -",
            ),
            (
                Record::FirstExecution {
                    request: request(1),
                    index: index(12),
                },
                "first ababababababababababababababab01 12",
            ),
            (
                Record::Verdict {
                    checker: name("K-8"),
                    passed: false,
                },
                "verdict K-8 fail",
            ),
        ];
        for (record, line) in cases {
            assert_eq!(
                record.to_string(),
                line,
                "rule: fixed field order and formats"
            );
        }
    }

    #[test]
    fn event_lines_carry_sequence_time_parent_node_and_kind() {
        let mut scheduler = Scheduler::new(
            MasterSeed::new(3).stream(Subsystem::Scheduler),
            EventBudget::DEFAULT,
        );
        scheduler
            .schedule(VirtualTime::from_micros(250), Some(on(4, 1)), ())
            .unwrap();
        let mut trace = Trace::new(ObservationBound::DEFAULT);
        let Step::Event(boot) = scheduler.step() else {
            panic!("expected the boot event");
        };
        trace.event(&boot, &name("boot"));
        scheduler
            .schedule(VirtualTime::from_micros(300), None, ())
            .unwrap();
        let Step::Event(child) = scheduler.step() else {
            panic!("expected the child event");
        };
        trace.event(&child, &name("tick"));
        trace
            .record(&Record::Commit {
                node: on(4, 1),
                index: index(1),
            })
            .unwrap();
        assert_eq!(
            trace.canonical(),
            "E 0 250 - n4/1 boot\nE 1 300 0 - tick\nR 1 commit n4/1 1\n"
        );
    }

    #[test]
    fn diagnostics_never_enter_the_canonical_bytes() {
        let records = [Record::Commit {
            node: on(1, 1),
            index: index(3),
        }];
        let plain = trace_of(&records);
        let mut noisy = trace_of(&records);
        noisy.diagnostic("opened /home/someone/keyval/n1/journal, pid 4242, at 0x7ffd5e8c");
        noisy.diagnostic("wall clock 2026-10-03T21:56:00Z");
        assert_eq!(
            noisy.canonical(),
            plain.canonical(),
            "rule: host paths, PIDs, addresses and wall-clock times stay out (§7.14)"
        );
        assert_eq!(noisy.hash(), plain.hash());
        assert_eq!(noisy.diagnostics().len(), 2);
    }

    #[test]
    fn diagnostics_past_the_bound_are_dropped_and_counted() {
        let mut trace = Trace::new(ObservationBound::new(2));
        for text in ["a", "b", "c", "d"] {
            trace.diagnostic(text);
        }
        assert_eq!(trace.diagnostics(), ["a", "b"]);
        assert_eq!(trace.dropped_diagnostics(), 2);
        assert!(
            trace.is_complete(),
            "rule: dropping diagnostics never makes a run incomplete (O-3)"
        );
    }

    #[test]
    fn required_observations_past_the_bound_make_the_run_incomplete() {
        let mut trace = Trace::new(ObservationBound::new(2));
        let commit = |raw| Record::Commit {
            node: on(1, 1),
            index: index(raw),
        };
        assert_eq!(trace.record(&commit(1)), Ok(()));
        assert_eq!(trace.record(&commit(2)), Ok(()));
        assert_eq!(
            trace.record(&commit(3)),
            Err(Overflow {
                bound: ObservationBound::new(2),
            }),
            "rule: a full step refuses required evidence instead of dropping it"
        );
        assert!(
            !trace.is_complete(),
            "rule: overflowing the bound fails closed, the run is incomplete (P10)"
        );
        assert_eq!(trace.canonical().lines().count(), 2);
        assert_eq!(ObservationBound::DEFAULT.get(), 4_096, "rule: §5 limits");
    }

    #[test]
    fn metrics_close_the_trace_in_name_order() {
        let mut trace = trace_of(&[]);
        trace.metric(name("packets.sent"), 7);
        trace.metric(name("elections"), 2);
        let canonical = trace.canonical();
        assert!(
            canonical.ends_with("M elections 2\nM packets.sent 7\n"),
            "canonical={canonical:?}"
        );
        let mut other = trace_of(&[]);
        other.metric(name("packets.sent"), 8);
        other.metric(name("elections"), 2);
        assert_eq!(
            compare(&canonical, &other.canonical()),
            Comparison::Differs {
                line: 7,
                left: Some("M packets.sent 7".to_owned()),
                right: Some("M packets.sent 8".to_owned()),
            },
            "rule: comparisons include the metrics"
        );
    }

    #[test]
    fn compare_reports_the_first_differing_line_and_keeps_both_sides() {
        let commit = Record::Commit {
            node: on(1, 1),
            index: index(3),
        };
        let left = trace_of(std::slice::from_ref(&commit)).canonical();
        assert_eq!(compare(&left, &left), Comparison::Identical);
        let right = trace_of(&[Record::Commit {
            node: on(1, 1),
            index: index(4),
        }])
        .canonical();
        assert_eq!(
            compare(&left, &right),
            Comparison::Differs {
                line: 6,
                left: Some("R 4 commit n1/1 3".to_owned()),
                right: Some("R 4 commit n1/1 4".to_owned()),
            }
        );
        let longer = trace_of(&[commit.clone(), commit]).canonical();
        assert_eq!(
            compare(&left, &longer),
            Comparison::Differs {
                line: 7,
                left: None,
                right: Some("R 4 commit n1/1 3".to_owned()),
            },
            "rule: a trace that stops early differs where it stops"
        );
    }

    #[test]
    fn the_hash_is_fnv1a_64_over_the_canonical_bytes() {
        // FNV-1a 64 reference values, computed independently.
        assert_eq!(TraceHash::of("").get(), 0xCBF2_9CE4_8422_2325);
        assert_eq!(TraceHash::of("a").get(), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(TraceHash::of("foobar").get(), 0x8594_4171_F739_67E8);
        let trace = trace_of(&[]);
        assert_eq!(trace.hash(), TraceHash::of(&trace.canonical()));
        assert_eq!(
            TraceHash::of("E 0 0 - - boot\n").to_string(),
            "2859d10b0b2657be"
        );
    }

    #[test]
    fn names_carry_no_host_text() {
        for bad in ["", "two words", "/home/x", "a\nb", "x".repeat(65).as_str()] {
            assert_eq!(Name::new(bad), None, "rule: {bad:?} is not a name");
        }
        for good in ["K-8", "packets.sent", "client_op", "x".repeat(64).as_str()] {
            assert_eq!(
                Name::new(good).map(|valid| valid.as_str().to_owned()),
                Some(good.to_owned())
            );
        }
    }

    #[test]
    fn the_same_steps_give_the_same_trace() {
        let records = [
            Record::Role {
                node: on(1, 1),
                role: Role::Candidate,
                term: term(2),
            },
            Record::Packet {
                fate: PacketFate::Sent,
                from: Endpoint::Node(node(1)),
                to: Endpoint::Node(node(2)),
                bytes: vec![1, 2, 3],
            },
        ];
        assert_eq!(
            trace_of(&records).canonical(),
            trace_of(&records).canonical()
        );
        assert_eq!(trace_of(&records).hash(), trace_of(&records).hash());
        assert_ne!(trace_of(&records).hash(), trace_of(&records[..1]).hash());
    }
}
