//! The telemetry adapter (PLAN.md §7.2, O-3): the node's structured log file, one JSON
//! object per line.
//!
//! The core's `Log` and `Observe` effects, and the root's own records (boot, storage
//! failures, stop), go through a queue bounded by `log_queue_records` (4,096) to a writer
//! thread. A record that finds the queue full is dropped and counted, never waited for, so
//! logging can never stall the controller. The writer reports drops in the log itself (a
//! `log_records_dropped` record each time it catches up after some), and the count is in
//! the node's snapshot.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use types::{DroppedInput, Lifecycle, LogEvent, LogLevel, NodeId, Observation, Role};

/// The log's sending end: cheap to clone, never blocks.
#[derive(Clone, Debug)]
pub struct Log {
    node: NodeId,
    queue: SyncSender<Value>,
    dropped: Arc<AtomicU64>,
}

impl Log {
    /// Opens (appending to) the log file at `path` and starts its writer, with a queue of
    /// `capacity` records.
    ///
    /// # Errors
    ///
    /// The operating system's error if the file cannot be opened or the thread started.
    pub fn open(
        path: &Path,
        node: NodeId,
        capacity: usize,
    ) -> io::Result<(Self, JoinHandle<io::Result<()>>)> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Self::start(file, node, capacity)
    }

    /// Starts a writer to `out`, with a queue of `capacity` records.
    ///
    /// # Errors
    ///
    /// The operating system's error if the thread cannot be started.
    pub fn start<W: Write + Send + 'static>(
        out: W,
        node: NodeId,
        capacity: usize,
    ) -> io::Result<(Self, JoinHandle<io::Result<()>>)> {
        let (queue, records) = mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer = {
            let dropped = Arc::clone(&dropped);
            thread::Builder::new()
                .name("keyval-log".to_owned())
                .spawn(move || write_records(out, node, &records, &dropped))?
        };
        Ok((
            Self {
                node,
                queue,
                dropped,
            },
            writer,
        ))
    }

    /// Queues `record`, or drops and counts it if the queue is full.
    pub fn record(&self, mut record: Map<String, Value>) {
        record.insert("node".to_owned(), json!(self.node.get()));
        match self.queue.try_send(Value::Object(record)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Logs one of the core's events (O-3).
    pub fn event(&self, event: LogEvent) {
        self.record(event_record(event));
    }

    /// Logs one of the core's observations.
    pub fn observation(&self, observation: Observation) {
        self.record(observation_record(observation));
    }

    /// The records dropped so far because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A record's common fields: wall-clock time (for people; the core never reads it), the
/// level and the event's name.
#[must_use]
pub fn base(level: LogLevel, event: &str) -> Map<String, Value> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let mut record = Map::new();
    record.insert(
        "ts_ms".to_owned(),
        json!(u64::try_from(millis).unwrap_or(u64::MAX)),
    );
    record.insert("level".to_owned(), json!(level_name(level)));
    record.insert("event".to_owned(), json!(event));
    record
}

fn event_record(event: LogEvent) -> Map<String, Value> {
    let mut record = base(event.level(), event_name(event));
    match event {
        LogEvent::RoleChanged { role, term, leader } => {
            record.insert("role".to_owned(), json!(role_name(role)));
            record.insert("term".to_owned(), json!(term.get()));
            record.insert("leader".to_owned(), json!(leader.map(NodeId::get)));
        }
        LogEvent::LifecycleChanged { from, to } => {
            record.insert("from".to_owned(), json!(lifecycle_name(from)));
            record.insert("to".to_owned(), json!(lifecycle_name(to)));
        }
        LogEvent::Dropped { input } => {
            record.insert(
                "input".to_owned(),
                json!(match input {
                    DroppedInput::PeerMessage => "peer_message",
                }),
            );
        }
    }
    record
}

fn observation_record(observation: Observation) -> Map<String, Value> {
    match observation {
        Observation::Applied { index } => {
            let mut record = base(LogLevel::Info, "applied");
            record.insert("index".to_owned(), json!(index.get()));
            record
        }
        Observation::Responded { attempt } => {
            let mut record = base(LogLevel::Info, "responded");
            record.insert("attempt".to_owned(), json!(attempt.get()));
            record
        }
    }
}

const fn event_name(event: LogEvent) -> &'static str {
    match event {
        LogEvent::RoleChanged { .. } => "role_changed",
        LogEvent::LifecycleChanged { .. } => "lifecycle_changed",
        LogEvent::Dropped { .. } => "input_dropped",
    }
}

/// A level's name in the log.
#[must_use]
pub const fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// A role's name in the log.
#[must_use]
pub const fn role_name(role: Role) -> &'static str {
    match role {
        Role::Follower => "follower",
        Role::PreCandidate => "pre_candidate",
        Role::Candidate => "candidate",
        Role::Leader => "leader",
    }
}

/// A lifecycle's name in the log (O-1's names).
#[must_use]
pub const fn lifecycle_name(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Starting => "starting",
        Lifecycle::Recovering => "recovering",
        Lifecycle::Ready => "ready",
        Lifecycle::Failed => "failed",
    }
}

/// The writer: one line per record, flushed as written. Whenever the drop count has grown
/// since it last said so, it writes a `log_records_dropped` record first.
fn write_records<W: Write>(
    out: W,
    node: NodeId,
    records: &Receiver<Value>,
    dropped: &AtomicU64,
) -> io::Result<()> {
    let mut out = BufWriter::new(out);
    let mut reported = 0;
    let report = |out: &mut BufWriter<W>, reported: &mut u64| -> io::Result<()> {
        let total = dropped.load(Ordering::Relaxed);
        if total > *reported {
            let mut record = base(LogLevel::Warn, "log_records_dropped");
            record.insert("node".to_owned(), json!(node.get()));
            record.insert("count".to_owned(), json!(total - *reported));
            record.insert("total".to_owned(), json!(total));
            writeln!(out, "{}", Value::Object(record))?;
            *reported = total;
        }
        Ok(())
    };
    for record in records {
        report(&mut out, &mut reported)?;
        writeln!(out, "{record}")?;
        out.flush()?;
    }
    report(&mut out, &mut reported)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, PoisonError};

    use serde_json::{Value, json};
    use types::{Lifecycle, LogEvent, LogIndex, LogLevel, NodeId, Observation, Role, Term};

    use super::{Log, base, write_records};

    /// An in-memory log file, readable while the writer holds a handle to it.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn records(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn events_and_observations_are_written_as_json_lines() {
        let buffer = Buffer::default();
        let node = NodeId::new(3).unwrap();
        let (log, writer) = Log::start(buffer.clone(), node, 16).unwrap();
        log.event(LogEvent::RoleChanged {
            role: Role::Leader,
            term: Term::new(2).unwrap(),
            leader: Some(node),
        });
        log.event(LogEvent::LifecycleChanged {
            from: Lifecycle::Recovering,
            to: Lifecycle::Ready,
        });
        log.observation(Observation::Applied {
            index: LogIndex::new(5).unwrap(),
        });
        drop(log);
        writer.join().unwrap().unwrap();
        let records = buffer.records();
        let summary: Vec<(&str, &str)> = records
            .iter()
            .map(|record| {
                (
                    record["event"].as_str().unwrap(),
                    record["level"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            (
                summary,
                records[0]["role"].as_str(),
                records[0]["term"].as_u64(),
                records[0]["leader"].as_u64(),
                records[1]["to"].as_str(),
                records[2]["index"].as_u64(),
                records
                    .iter()
                    .all(|record| record["node"].as_u64() == Some(3)
                        && record["ts_ms"].as_u64().is_some()),
            ),
            (
                vec![
                    ("role_changed", "info"),
                    ("lifecycle_changed", "info"),
                    ("applied", "info"),
                ],
                Some("leader"),
                Some(2),
                Some(3),
                Some("ready"),
                Some(5),
                true,
            ),
            "rule: the node's log is one JSON object per line, naming the node, the time, the \
             level and the event with its fields (O-3)"
        );
    }

    #[test]
    fn a_full_queue_drops_and_counts_records_and_the_log_reports_them() {
        let node = NodeId::new(1).unwrap();
        let (queue, records) = mpsc::sync_channel(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let log = Log {
            node,
            queue,
            dropped: Arc::clone(&dropped),
        };
        for n in 0..5_u64 {
            let mut record = base(LogLevel::Info, "probe");
            record.insert("n".to_owned(), json!(n));
            log.record(record);
        }
        let counted = log.dropped();
        drop(log);
        let buffer = Buffer::default();
        write_records(buffer.clone(), node, &records, &dropped).unwrap();
        let written = buffer.records();
        let events: Vec<&str> = written
            .iter()
            .map(|record| record["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            (counted, events, written[0]["count"].as_u64()),
            (3, vec!["log_records_dropped", "probe", "probe"], Some(3)),
            "rule: with the queue full a record is dropped and counted, never waited for, and \
             the log itself reports how many were dropped (O-3, §7.11)"
        );
    }
}
