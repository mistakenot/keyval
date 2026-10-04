//! The node's watch (PLAN.md §7.12, CD-7, A-2; br-7tjt): the node's essential threads run
//! under it, and a panic on any of them halts the whole node instead of leaving it
//! half-alive.
//!
//! CD-7 makes a broken invariant a panic, in every build, so that the node stops. A panic
//! ends only its own thread, though. Unwatched, a dead controller would leave the clock, the
//! I/O and the HTTP threads running, client attempts accepted that nobody answers, and the
//! published snapshot frozen at `ready`. A dead I/O thread would leave the waiting Ready
//! unresolved for good (§7.12), and a dead clock would stop every election and deadline.
//! Under the watch, the first panic halts the node:
//! - the hand-off closes: later offers are refused at once, and the clock and the controller
//!   stop at their next wake;
//! - [`Watch::halted`] names the thread and its panic message, and the node's snapshot reads
//!   `failed` from then on (A-2: a node never presents itself as ready when it is not);
//! - the halt is logged as `thread_panicked`, while the log writer still runs;
//! - [`Watch::wait`] returns, so the process exits (fail-stop).
//!
//! The panic then goes on unwinding, so the thread's join still reports it.
//!
//! Connection threads are not watched: a panic serving one request closes that connection
//! only, so no request can stop the node.

use std::any::Any;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use serde_json::json;
use types::LogLevel;

use crate::inbox::Inbox;
use crate::logs::{Log, base};

/// Why the node halted: the first watched thread that panicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Halt {
    /// The thread's name.
    pub thread: String,
    /// Its panic message.
    pub message: String,
}

/// The watch over one node's threads.
#[derive(Debug)]
pub struct Watch {
    /// The halt, once a watched thread panicked.
    halt: Mutex<Option<Halt>>,
    /// Signalled when the node halts.
    halted: Condvar,
    /// The node's hand-off, closed by a halt.
    inbox: Arc<Inbox>,
    /// The node's log, from when it opens until the node shuts down: a sender the log
    /// writer would otherwise wait on forever.
    log: Mutex<Option<Log>>,
}

impl Watch {
    /// A watch over the node whose hand-off is `inbox`.
    #[must_use]
    pub const fn new(inbox: Arc<Inbox>) -> Self {
        Self {
            halt: Mutex::new(None),
            halted: Condvar::new(),
            inbox,
            log: Mutex::new(None),
        }
    }

    /// Logs a halt to `log` from now on.
    pub fn log_to(&self, log: Log) {
        *self.log.lock().unwrap_or_else(PoisonError::into_inner) = Some(log);
    }

    /// Lets go of the log, so its writer can finish once the node's other senders are gone:
    /// the node does this as it shuts down. A halt after it is not logged.
    pub fn close_log(&self) {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn state(&self) -> MutexGuard<'_, Option<Halt>> {
        self.halt.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Halts the node because the thread `thread` panicked with `message`. Only the first
    /// halt counts; later ones change nothing.
    pub fn halt(&self, thread: &str, message: &str) {
        let mut halt = self.state();
        if halt.is_some() {
            return;
        }
        *halt = Some(Halt {
            thread: thread.to_owned(),
            message: message.to_owned(),
        });
        drop(halt);
        self.inbox.close();
        let log = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(log) = log {
            let mut record = base(LogLevel::Error, "thread_panicked");
            record.insert("thread".to_owned(), json!(thread));
            record.insert("message".to_owned(), json!(message));
            log.record(record);
        }
        self.halted.notify_all();
    }

    /// Why the node halted, if it has.
    #[must_use]
    pub fn halted(&self) -> Option<Halt> {
        self.state().clone()
    }

    /// Waits until the node halts, and says why.
    #[must_use]
    pub fn wait(&self) -> Halt {
        let mut halt = self.state();
        loop {
            if let Some(halted) = halt.as_ref() {
                return halted.clone();
            }
            halt = self
                .halted
                .wait(halt)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Starts the thread `name` running `body` under the watch: if `body` panics, the node
    /// halts and the panic goes on unwinding, so the thread's join reports it.
    ///
    /// # Errors
    ///
    /// The operating system's error if the thread cannot be started.
    pub fn spawn<T, F>(self: &Arc<Self>, name: &str, body: F) -> io::Result<JoinHandle<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let watch = Arc::clone(self);
        let thread_name = name.to_owned();
        thread::Builder::new().name(name.to_owned()).spawn(move || {
            match panic::catch_unwind(AssertUnwindSafe(body)) {
                Ok(value) => value,
                Err(cause) => {
                    watch.halt(&thread_name, &panic_message(cause.as_ref()));
                    panic::resume_unwind(cause)
                }
            }
        })
    }
}

/// A panic's message: its text, or a placeholder for a payload of another type.
fn panic_message(cause: &(dyn Any + Send)) -> String {
    cause
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| cause.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "a panic with a payload that is not text".to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use fs_storage::{FsStorage, create_data_dir};
    use serde_json::Value;
    use types::{
        AttemptId, ClientOp, Config, Effect, Incarnation, InstanceId, Key, KeyValueLimits,
        Lifecycle, LogEvent, NodeId, Role,
    };

    use super::{Halt, Watch};
    use crate::inbox::{Inbox, Refused};
    use crate::io::Io;
    use crate::logs::Log;
    use crate::runtime::{Busy, Node, Options, Outputs, start};

    const ONE_VOTER: &str = include_str!("../../../config/fixtures/one-voter.toml");

    fn inbox() -> Arc<Inbox> {
        Arc::new(Inbox::new(4, 4))
    }

    /// A fresh directory under the cargo target directory (never `/tmp`), not yet holding
    /// a data directory.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exe = std::env::current_exe().unwrap();
        // <target>/<profile>/deps/<test binary> -> <target>
        let target = exe.ancestors().nth(3).unwrap().to_path_buf();
        let dir = target.join("server-watch-scratch").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An in-memory log file.
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

    /// Waits up to 10 s for the node to halt.
    fn halted_within(watch: &Watch) -> Halt {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(halt) = watch.halted() {
                return halt;
            }
            assert!(
                Instant::now() < deadline,
                "fixture: the node halts within 10 s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_panicking_watched_thread_halts_the_node_once_and_its_join_still_reports_it() {
        let inbox = inbox();
        let watch = Arc::new(Watch::new(Arc::clone(&inbox)));
        let first = watch.spawn("keyval-first", || -> u8 { panic!("broken invariant") });
        let joined = first.unwrap().join();
        let second = watch.spawn("keyval-second", || -> u8 { panic!("later") });
        let _ = second.unwrap().join();
        let offered = inbox.offer(types::Input::Tick).map_err(|(why, _)| why);
        // Fails within 10 s, rather than block, if no halt came; then `wait` returns at once.
        let halt = halted_within(&watch);
        assert_eq!(
            (joined.is_err(), halt, watch.wait(), offered),
            (
                true,
                Halt {
                    thread: "keyval-first".to_owned(),
                    message: "broken invariant".to_owned(),
                },
                Halt {
                    thread: "keyval-first".to_owned(),
                    message: "broken invariant".to_owned(),
                },
                Err(Refused::Closed),
            ),
            "rule: the first panic on a watched thread halts the node: it names that thread \
             and its message, closes the hand-off so nothing more is taken, and still \
             reaches the thread's join (CD-7, §7.12, br-7tjt)"
        );
    }

    #[test]
    fn a_watched_thread_that_returns_does_not_halt_the_node() {
        let inbox = inbox();
        let watch = Arc::new(Watch::new(Arc::clone(&inbox)));
        let joined = watch.spawn("keyval-fine", || 7).unwrap().join().unwrap();
        assert_eq!(
            (joined, watch.halted(), inbox.is_closed()),
            (7, None, false),
            "rule: only a panic halts the node (br-7tjt)"
        );
    }

    #[test]
    fn the_halt_is_logged_with_its_thread_and_message() {
        let watch = Arc::new(Watch::new(inbox()));
        let buffer = Buffer::default();
        let (log, writer) = Log::start(buffer.clone(), NodeId::new(1).unwrap(), 8, &watch).unwrap();
        watch.log_to(log.clone());
        let _ = watch
            .spawn("keyval-clock", || -> u8 { panic!("the clock broke") })
            .unwrap()
            .join();
        // As the node's shutdown does: the writer thread runs under the watch, so the watch
        // must let go of its log before the writer can finish.
        watch.close_log();
        drop(log);
        writer.join().unwrap().unwrap();
        let records: Vec<Value> = String::from_utf8(buffer.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let halted: Vec<(&str, &str, &str)> = records
            .iter()
            .map(|record| {
                (
                    record["event"].as_str().unwrap(),
                    record["thread"].as_str().unwrap_or(""),
                    record["message"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            halted,
            vec![("thread_panicked", "keyval-clock", "the clock broke")],
            "rule: the halt is in the node's log, naming the thread and its panic (O-3, \
             br-7tjt)"
        );
    }

    #[test]
    fn a_panic_on_the_io_thread_halts_the_node() {
        let dir = scratch("io").join("data");
        let _created = create_data_dir(&dir).unwrap();
        let storage = FsStorage::open(&dir).unwrap();
        let inbox = inbox();
        let watch = Arc::new(Watch::new(Arc::clone(&inbox)));
        let (log, _writer) =
            Log::start(Buffer::default(), NodeId::new(1).unwrap(), 8, &watch).unwrap();
        let (io, thread) = Io::spawn(
            storage,
            Incarnation::new(1),
            Arc::clone(&inbox),
            log,
            &watch,
        )
        .unwrap();
        // A non-storage effect breaks IO-STORAGE-ONLY on the I/O thread: a real invariant.
        io.push(Effect::Log(LogEvent::RoleChanged {
            role: Role::Leader,
            term: types::Term::new(1).unwrap(),
            leader: None,
        }));
        let halt = halted_within(&watch);
        assert!(
            halt.thread == "keyval-io"
                && halt.message.contains("IO-STORAGE-ONLY")
                && thread.join().is_err()
                && inbox.is_closed(),
            "rule: a panic on the I/O thread halts the node, so a waiting Ready is never left \
             silently unresolved (§7.12, br-7tjt); halt={halt:?}"
        );
    }

    /// A started one-voter node with `tick_ms` 5, answering through `respond`.
    fn started(
        name: &str,
        respond: impl FnMut(AttemptId, types::Response) + Send + 'static,
    ) -> Node {
        let dir = scratch(name);
        let text = ONE_VOTER
            .replace("nodes/one-voter/1", "node-1")
            .replace("tick_ms = 50", "tick_ms = 5");
        let config = Config::parse(&text, &dir).unwrap();
        let node = NodeId::new(1).unwrap();
        let instance = InstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
        crate::init::init(&config, node, instance).unwrap();
        let prepared = crate::run::prepare(&config, node).unwrap();
        start(
            prepared,
            &config,
            node,
            Outputs {
                respond: Box::new(respond),
                send: Box::new(|_, _| {}),
            },
            &Options {
                log_file: dir.join("node.log"),
            },
        )
        .unwrap()
    }

    /// Waits up to 10 s for the node to lead.
    fn leads(node: &Node) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(node.snapshot().lifecycle == Lifecycle::Ready
            && node.snapshot().role == Role::Leader)
        {
            assert!(
                Instant::now() < deadline,
                "fixture: the node leads within 10 s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn get() -> ClientOp {
        ClientOp::Get {
            key: Key::parse("k".to_owned(), KeyValueLimits::MAXIMA).unwrap(),
        }
    }

    #[test]
    fn a_controller_panic_halts_the_whole_node_which_reports_failed_and_takes_nothing_more() {
        let node = started("controller", |_, _| panic!("the responder broke"));
        leads(&node);
        assert_eq!(
            node.client(AttemptId::new(1), get()),
            Ok(()),
            "fixture: the first attempt is taken"
        );
        let halt = halted_within(node.watch());
        let after = node.client(AttemptId::new(2), get());
        let lifecycle = node.snapshot().lifecycle;
        let stopped = node.shutdown();
        assert!(
            halt.thread == "keyval-controller"
                && halt.message == "the responder broke"
                && lifecycle == Lifecycle::Failed
                && after == Err(Busy)
                && stopped.as_ref().is_err_and(
                    |why| why.contains("keyval-controller panicked: the responder broke")
                ),
            "rule: a panic on the controller thread halts the whole node: it reads failed, so \
             health is never ready over a dead core (A-2), it takes no attempt it cannot \
             answer, and it stops, reporting the panic (CD-7, §7.12, br-7tjt); halt={halt:?} \
             lifecycle={lifecycle:?} after={after:?} stopped={stopped:?}"
        );
    }

    #[test]
    fn a_halt_from_any_thread_stops_the_controller_and_the_clock_and_reads_failed() {
        let node = started("any", |_, _| {});
        leads(&node);
        node.watch().halt("keyval-io", "injected");
        let lifecycle = node.snapshot().lifecycle;
        let after = node.client(AttemptId::new(1), get());
        let stopped = node.shutdown();
        assert!(
            lifecycle == Lifecycle::Failed
                && after == Err(Busy)
                && stopped
                    .as_ref()
                    .is_err_and(|why| why.contains("keyval-io panicked: injected")),
            "rule: whichever thread halts the node, the controller and the clock stop, the \
             node reads failed and takes nothing more, and shutdown reports why (br-7tjt); \
             lifecycle={lifecycle:?} after={after:?} stopped={stopped:?}"
        );
    }
}
