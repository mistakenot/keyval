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
//!   canonical bytes. A run keeps at most the structured-log queue's bound of them, each
//!   cut to [`DIAGNOSTIC_TEXT_BYTES`]; past the bound they are dropped. Both counts are
//!   deterministic, so a nonzero count closes the trace as a metric
//!   (`diagnostics.dropped`, `diagnostics.truncated`; CD-9, O-3).
//! - Required observations are bounded per step by the same queue (4,096 records, §7.11,
//!   registered as `types::Limit::LogQueueRecords`). Overflowing the bound fails closed:
//!   the observation is refused and the run is incomplete (P10). Oracle evidence is never
//!   silently dropped.
//! - The hash (64-bit FNV-1a over the canonical bytes) only speeds up a mismatch check.
//!   [`compare`] always checks every line, metrics included, and reports the first
//!   difference; callers keep both traces.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use types::{
    ByteCount, Incarnation, Limit, LogIndex, NodeId, RequestId, Role, StorageOp, SyncSeq, Term,
};

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

/// A simulated disk operation (§10.2 event model): what one storage effect did on the
/// device, a sync's completion reaching the node, or a storage failure.
///
/// Every storage effect has a record (`types::Effect::{Append, Truncate, Sync,
/// CreateJournal, SyncDir}`), and so does `types::Input::StorageFailed`. A completion or
/// failure names its sync and the incarnation that requested it. A stale completion
/// delivered to a later incarnation (§10.2, B-17) therefore reads differently from a
/// current one, and nothing inconvenient is left out of the trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskOp {
    /// Append progress: `len` bytes at `offset` reached the device.
    Append {
        /// The byte offset.
        offset: ByteCount,
        /// The byte count.
        len: ByteCount,
    },
    /// The journal was cut to `len` bytes, discarding a torn tail (DUR-6).
    Truncate {
        /// The length kept.
        len: ByteCount,
    },
    /// Sync `seq` made the journal durable up to byte `upto`.
    Sync {
        /// The sync.
        seq: SyncSeq,
        /// The durable length.
        upto: ByteCount,
    },
    /// The empty journal file was created. Its directory entry stays volatile until a
    /// directory sync (DUR-7).
    CreateJournal,
    /// Directory sync `seq` made every earlier file creation durable (DUR-7).
    SyncDir {
        /// The sync.
        seq: SyncSeq,
    },
    /// The completion of sync `seq` (a journal or directory sync) reached the node. It is
    /// stale if `requested_by` is not the node's current incarnation (B-17).
    SyncDone {
        /// The sync it completes.
        seq: SyncSeq,
        /// The incarnation that requested it.
        requested_by: Incarnation,
    },
    /// The storage effect `op`, requested by `requested_by`, failed (§7.11), and its
    /// failure reached the node.
    Failed {
        /// The effect that failed.
        op: StorageOp,
        /// The incarnation that requested it.
        requested_by: Incarnation,
    },
}

impl fmt::Display for DiskOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append { offset, len } => write!(f, "append {} {}", offset.get(), len.get()),
            Self::Truncate { len } => write!(f, "truncate {}", len.get()),
            Self::Sync { seq, upto } => write!(f, "sync {} {}", seq.get(), upto.get()),
            Self::CreateJournal => f.write_str("create-journal"),
            Self::SyncDir { seq } => write!(f, "sync-dir {}", seq.get()),
            Self::SyncDone { seq, requested_by } => {
                write!(f, "sync-done {} {}", seq.get(), requested_by.get())
            }
            Self::Failed { op, requested_by } => {
                write!(f, "failed {} {}", Op(*op), requested_by.get())
            }
        }
    }
}

/// A storage effect as `<kind> <sync or ->`: every kind has the same two fields.
struct Op(StorageOp);

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            StorageOp::Append => f.write_str("append -"),
            StorageOp::Truncate => f.write_str("truncate -"),
            StorageOp::Sync(seq) => write!(f, "sync {}", seq.get()),
            StorageOp::CreateJournal => f.write_str("create-journal -"),
            StorageOp::SyncDir(seq) => write!(f, "sync-dir {}", seq.get()),
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

/// The structured-log queue's bound (§7.11): required observations per step, and
/// diagnostics kept per run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationBound(u64);

impl ObservationBound {
    /// The registered bound, [`Limit::LogQueueRecords`]: a fixed 4,096 records, read from
    /// the limits registry rather than restated here (CD-9).
    pub const DEFAULT: Self = Self(Limit::LogQueueRecords.maximum());

    /// A bound of `records`.
    #[must_use]
    pub const fn new(records: u64) -> Self {
        Self(records)
    }

    /// The number of records.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The most bytes of one diagnostic's text a trace keeps, a bound this type owns (CD-9).
/// Longer text is cut at a character boundary, and the cut is counted.
pub const DIAGNOSTIC_TEXT_BYTES: usize = 1024;

/// The metric a nonzero count of dropped diagnostics closes the trace with.
const DROPPED_METRIC: &str = "diagnostics.dropped";

/// The metric a nonzero count of truncated diagnostics closes the trace with.
const TRUNCATED_METRIC: &str = "diagnostics.truncated";

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

/// The line that ends an incomplete trace's canonical bytes (P10).
///
/// A required observation overflowed its bound, so the trace is not the run's full
/// evidence, and a checker that reads only the bytes must still refuse it.
pub const INCOMPLETE_MARKER: &str = "X incomplete";

/// Whether `canonical` is marked incomplete: its last line is [`INCOMPLETE_MARKER`].
#[must_use]
pub fn marks_incomplete(canonical: &str) -> bool {
    canonical.lines().next_back() == Some(INCOMPLETE_MARKER)
}

/// A run's canonical trace, built step by step.
#[derive(Clone, Debug)]
pub struct Trace {
    lines: Vec<String>,
    metrics: BTreeMap<Name, u64>,
    bound: ObservationBound,
    step: Option<EventSeq>,
    recorded_in_step: u64,
    diagnostics: Vec<String>,
    dropped_diagnostics: u64,
    truncated_diagnostics: u64,
    incomplete: bool,
}

impl Trace {
    /// An empty trace: `bound` required observations per step, and `bound` diagnostics
    /// kept per run.
    #[must_use]
    pub const fn new(bound: ObservationBound) -> Self {
        Self {
            lines: Vec::new(),
            metrics: BTreeMap::new(),
            bound,
            step: None,
            recorded_in_step: 0,
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            truncated_diagnostics: 0,
            incomplete: false,
        }
    }

    /// Starts the step of a dispatched event: its sequence, virtual time, causal parent,
    /// node incarnation and `kind`. The per-step bound on required observations starts
    /// again; the diagnostics bound is per run.
    pub fn event<E>(&mut self, event: &Event<E>, kind: &Name) {
        self.step = Some(event.seq);
        self.recorded_in_step = 0;
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

    /// Notes diagnostic text, which never enters the canonical bytes.
    ///
    /// A run keeps at most the bound's worth of diagnostics, each cut to
    /// [`DIAGNOSTIC_TEXT_BYTES`] at a character boundary. Past the bound a diagnostic is
    /// dropped. Both are counted (CD-9, O-3).
    pub fn diagnostic(&mut self, text: &str) {
        if u64::try_from(self.diagnostics.len()).unwrap_or(u64::MAX) >= self.bound.get() {
            self.dropped_diagnostics += 1;
            return;
        }
        let kept = prefix_within(text, DIAGNOSTIC_TEXT_BYTES);
        if kept.len() < text.len() {
            self.truncated_diagnostics += 1;
        }
        self.diagnostics.push(kept.to_owned());
    }

    /// Sets a deterministic metric; metrics close the trace in name order.
    pub fn metric(&mut self, name: Name, value: u64) {
        self.metrics.insert(name, value);
    }

    /// Whether every required observation was recorded. Once false it stays false, and
    /// the canonical bytes end with [`INCOMPLETE_MARKER`].
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

    /// Diagnostics whose text was cut to [`DIAGNOSTIC_TEXT_BYTES`].
    #[must_use]
    pub const fn truncated_diagnostics(&self) -> u64 {
        self.truncated_diagnostics
    }

    /// The canonical bytes: every line, then the metrics, each ending in a newline. An
    /// incomplete trace ends with [`INCOMPLETE_MARKER`], so incompleteness is part of the
    /// bytes that are compared, not only of this value (P10).
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut metrics = self.metrics.clone();
        for (metric, count) in [
            (DROPPED_METRIC, self.dropped_diagnostics),
            (TRUNCATED_METRIC, self.truncated_diagnostics),
        ] {
            if let Some(name) = Name::new(metric).filter(|_| count > 0) {
                metrics.insert(name, count);
            }
        }
        let metrics = metrics
            .iter()
            .map(|(name, value)| format!("M {} {value}", name.as_str()));
        let marker = self.incomplete.then(|| INCOMPLETE_MARKER.to_owned());
        self.lines
            .iter()
            .cloned()
            .chain(metrics)
            .chain(marker)
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

/// The longest prefix of `text` that is at most `bytes` long and ends on a character
/// boundary.
fn prefix_within(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
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
        /// The line holding the first differing byte.
        line: usize,
        /// The left trace's line, exactly, with its terminator if it has one.
        left: Option<String>,
        /// The right trace's line, exactly, with its terminator if it has one.
        right: Option<String>,
    },
}

/// Compares two canonical traces byte for byte (§10.5 K-14), events and metrics alike.
///
/// It reports the line holding the first differing byte. A missing final newline or a
/// `\r\n` counts as a difference. The hash never decides equality.
#[must_use]
pub fn compare(left: &str, right: &str) -> Comparison {
    let at = left
        .bytes()
        .zip(right.bytes())
        .take_while(|(here, there)| here == there)
        .count();
    if at == left.len() && at == right.len() {
        return Comparison::Identical;
    }
    let line = left.bytes().take(at).filter(|&byte| byte == b'\n').count() + 1;
    let piece = |text: &str| text.split_inclusive('\n').nth(line - 1).map(str::to_owned);
    Comparison::Differs {
        line,
        left: piece(left),
        right: piece(right),
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
                        offset: ByteCount::new(4_096),
                        len: ByteCount::new(512),
                    },
                },
                "disk n1/2 append 4096 512",
            ),
            (
                Record::Disk {
                    node: n,
                    op: DiskOp::Sync {
                        seq: SyncSeq::new(7),
                        upto: ByteCount::new(4_608),
                    },
                },
                "disk n1/2 sync 7 4608",
            ),
            (
                Record::Disk {
                    node: n,
                    op: DiskOp::SyncDone {
                        seq: SyncSeq::new(7),
                        requested_by: Incarnation::new(2),
                    },
                },
                "disk n1/2 sync-done 7 2",
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
        assert_eq!(
            trace.canonical(),
            "R - commit n1/1 1\nR - commit n1/1 2\nX incomplete\n",
            "rule: the refused record is absent and the bytes end with the incomplete marker"
        );
        assert_eq!(
            types::Limit::LogQueueRecords.source(),
            types::LimitSource::Fixed(ObservationBound::DEFAULT.get()),
            "rule: the bound is the registered structured-log queue (§7.11, CD-9)"
        );
    }

    #[test]
    fn retained_diagnostics_are_bounded_per_run_and_the_drops_are_reported() {
        let mut scheduler = Scheduler::new(
            MasterSeed::new(4).stream(Subsystem::Scheduler),
            EventBudget::DEFAULT,
        );
        let mut trace = Trace::new(ObservationBound::new(3));
        let mut step = |trace: &mut Trace, micros| {
            scheduler
                .schedule(VirtualTime::from_micros(micros), None, ())
                .unwrap();
            let Step::Event(event) = scheduler.step() else {
                panic!("expected an event");
            };
            trace.event(&event, &name("tick"));
        };
        step(&mut trace, 1);
        trace.diagnostic("one");
        trace.diagnostic("two");
        assert_eq!(trace.diagnostics().len(), 2, "rule: N-1 are kept");
        step(&mut trace, 2);
        trace.diagnostic("three");
        assert_eq!(trace.diagnostics().len(), 3, "rule: N are kept");
        trace.diagnostic("four");
        step(&mut trace, 3);
        trace.diagnostic("five");
        assert_eq!(
            (trace.diagnostics(), trace.dropped_diagnostics()),
            (["one", "two", "three"].map(str::to_owned).as_slice(), 2),
            "rule: N+1 and later are dropped and counted; a new step does not reset the \
             bound, which is per run (CD-9)"
        );
        assert!(
            trace.is_complete(),
            "rule: dropping diagnostics never fails a run"
        );
        assert!(
            trace.canonical().ends_with("M diagnostics.dropped 2\n"),
            "rule: the drop count is reported, as a deterministic metric; canonical={:?}",
            trace.canonical()
        );
    }

    #[test]
    fn oversized_diagnostic_text_is_cut_at_a_character_boundary_and_counted() {
        let mut trace = trace_of(&[]);
        let exact = "a".repeat(DIAGNOSTIC_TEXT_BYTES);
        trace.diagnostic(&exact);
        // A two-byte character straddles the limit, so the cut falls before it.
        let straddling = format!("{}\u{e9}tail", "b".repeat(DIAGNOSTIC_TEXT_BYTES - 1));
        trace.diagnostic(&straddling);
        assert_eq!(
            trace.diagnostics(),
            [exact, "b".repeat(DIAGNOSTIC_TEXT_BYTES - 1)],
            "rule: text up to the limit is kept whole; longer text is cut at a character \
             boundary (CD-9)"
        );
        assert_eq!(trace.truncated_diagnostics(), 1);
        assert!(
            trace.canonical().ends_with("M diagnostics.truncated 1\n"),
            "rule: the cut is counted and reported; canonical={:?}",
            trace.canonical()
        );
    }

    #[test]
    fn incompleteness_is_sticky_and_ends_the_canonical_bytes() {
        let verdict = Record::Verdict {
            checker: name("K-1"),
            passed: true,
        };
        let mut trace = Trace::new(ObservationBound::new(1));
        assert_eq!(trace.record(&verdict), Ok(()));
        assert!(
            trace.record(&verdict).is_err(),
            "test setup: the step overflows"
        );
        let mut scheduler = Scheduler::new(
            MasterSeed::new(1).stream(Subsystem::Scheduler),
            EventBudget::DEFAULT,
        );
        scheduler
            .schedule(VirtualTime::from_micros(1), None, ())
            .unwrap();
        let Step::Event(event) = scheduler.step() else {
            panic!("expected an event");
        };
        trace.event(&event, &name("tick"));
        assert_eq!(
            trace.record(&verdict),
            Ok(()),
            "rule: the next step has a fresh bound"
        );
        trace.metric(name("events"), 1);
        assert!(
            !trace.is_complete(),
            "rule: incompleteness is sticky across later steps (P10)"
        );
        let canonical = trace.canonical();
        assert!(
            canonical.ends_with("M events 1\nX incomplete\n"),
            "rule: the marker is the last line, after the metrics; canonical={canonical:?}"
        );
        assert!(marks_incomplete(&canonical));
        assert!(
            !marks_incomplete(&trace_of(&[]).canonical()),
            "rule: a complete trace carries no marker"
        );
    }

    #[test]
    fn each_event_starts_a_fresh_observation_bound() {
        let verdict = Record::Verdict {
            checker: name("K-2"),
            passed: true,
        };
        let mut scheduler = Scheduler::new(
            MasterSeed::new(2).stream(Subsystem::Scheduler),
            EventBudget::DEFAULT,
        );
        let mut trace = Trace::new(ObservationBound::new(2));
        for step in 0..3 {
            scheduler
                .schedule(VirtualTime::from_micros(step + 1), None, ())
                .unwrap();
            let Step::Event(event) = scheduler.step() else {
                panic!("expected an event");
            };
            trace.event(&event, &name("tick"));
            assert_eq!(trace.record(&verdict), Ok(()), "rule: N-1 fit; step={step}");
            assert_eq!(trace.record(&verdict), Ok(()), "rule: N fit; step={step}");
        }
        assert!(
            trace.is_complete(),
            "rule: the bound is per step, and event() resets it"
        );
        assert_eq!(
            trace.record(&verdict),
            Err(Overflow {
                bound: ObservationBound::new(2),
            }),
            "rule: N+1 in one step overflows"
        );
    }

    #[test]
    fn every_storage_effect_and_failure_has_a_fixed_disk_line() {
        let n = on(1, 3);
        let disk = |op| Record::Disk { node: n, op }.to_string();
        let cases = [
            (
                DiskOp::Truncate {
                    len: ByteCount::new(4_096),
                },
                "disk n1/3 truncate 4096",
            ),
            (DiskOp::CreateJournal, "disk n1/3 create-journal"),
            (
                DiskOp::SyncDir {
                    seq: SyncSeq::new(1),
                },
                "disk n1/3 sync-dir 1",
            ),
            (
                DiskOp::Failed {
                    op: StorageOp::Append,
                    requested_by: Incarnation::new(3),
                },
                "disk n1/3 failed append - 3",
            ),
            (
                DiskOp::Failed {
                    op: StorageOp::Truncate,
                    requested_by: Incarnation::new(3),
                },
                "disk n1/3 failed truncate - 3",
            ),
            (
                DiskOp::Failed {
                    op: StorageOp::Sync(SyncSeq::new(9)),
                    requested_by: Incarnation::new(3),
                },
                "disk n1/3 failed sync 9 3",
            ),
            (
                DiskOp::Failed {
                    op: StorageOp::CreateJournal,
                    requested_by: Incarnation::new(3),
                },
                "disk n1/3 failed create-journal - 3",
            ),
            (
                DiskOp::Failed {
                    op: StorageOp::SyncDir(SyncSeq::new(2)),
                    requested_by: Incarnation::new(3),
                },
                "disk n1/3 failed sync-dir 2 3",
            ),
        ];
        for (op, line) in cases {
            assert_eq!(
                disk(op),
                line,
                "rule: every storage effect and StorageFailed has a fixed-format disk line \
                 (§10.2: the trace contains disk operations and completions)"
            );
        }
    }

    #[test]
    fn a_stale_completion_reads_differently_from_a_current_one() {
        // Node 1 restarted as incarnation 3; incarnation 2's sync 7 completes after it.
        let at = on(1, 3);
        let done = |requested_by| {
            Record::Disk {
                node: at,
                op: DiskOp::SyncDone {
                    seq: SyncSeq::new(7),
                    requested_by: Incarnation::new(requested_by),
                },
            }
            .to_string()
        };
        assert_eq!(done(2), "disk n1/3 sync-done 7 2");
        assert_ne!(
            done(2),
            done(3),
            "rule: a completion names the incarnation that asked for it, so a stale one \
             delivered to the next incarnation is visible in the trace (B-17)"
        );
        let failed = |requested_by| {
            Record::Disk {
                node: at,
                op: DiskOp::Failed {
                    op: StorageOp::Sync(SyncSeq::new(7)),
                    requested_by: Incarnation::new(requested_by),
                },
            }
            .to_string()
        };
        assert_ne!(failed(2), failed(3), "rule: the same holds for a failure");
    }

    #[test]
    fn compare_decides_on_exact_bytes() {
        assert_eq!(
            compare("", ""),
            Comparison::Identical,
            "rule: two empty traces are the same bytes; K-14 refuses them elsewhere"
        );
        assert_eq!(
            compare("", "M a 1\n"),
            Comparison::Differs {
                line: 1,
                left: None,
                right: Some("M a 1\n".to_owned()),
            }
        );
        assert_eq!(
            compare("M a 1\nM b 2\n", "M a 1\nM b 2"),
            Comparison::Differs {
                line: 2,
                left: Some("M b 2\n".to_owned()),
                right: Some("M b 2".to_owned()),
            },
            "rule: a missing final newline differs (§10.5: byte-for-byte)"
        );
        assert_eq!(
            compare("M a 1\r\n", "M a 1\n"),
            Comparison::Differs {
                line: 1,
                left: Some("M a 1\r\n".to_owned()),
                right: Some("M a 1\n".to_owned()),
            },
            "rule: CRLF differs from LF"
        );
        assert_eq!(
            compare("M \u{e9} 1\n", "M \u{e8} 1\n"),
            Comparison::Differs {
                line: 1,
                left: Some("M \u{e9} 1\n".to_owned()),
                right: Some("M \u{e8} 1\n".to_owned()),
            },
            "rule: a difference inside a multibyte character is reported, never a panic"
        );
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
                left: Some("M packets.sent 7\n".to_owned()),
                right: Some("M packets.sent 8\n".to_owned()),
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
                left: Some("R 4 commit n1/1 3\n".to_owned()),
                right: Some("R 4 commit n1/1 4\n".to_owned()),
            }
        );
        let longer = trace_of(&[commit.clone(), commit]).canonical();
        assert_eq!(
            compare(&left, &longer),
            Comparison::Differs {
                line: 7,
                left: None,
                right: Some("R 4 commit n1/1 3\n".to_owned()),
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
