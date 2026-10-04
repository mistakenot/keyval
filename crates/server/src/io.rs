//! The byte-storage adapter's I/O thread (PLAN.md §7.2, §7.4, §7.12): file writes and syncs
//! run off the controller and come back as completions.
//!
//! The thread owns the locked data directory's journal handle. It executes the core's
//! storage effects in the order the core emitted them (an append before the sync that makes
//! it durable) and hands each completion (`SyncDone`, or `StorageFailed` with the cause
//! logged) back through the unrefusable lane of the hand-off. Its queue is bounded by
//! construction: the core emits a Ready's storage effects and then waits for that Ready's
//! sync, so at most one Ready's effects are ever queued (§7.11). `IO-QUEUE` asserts it.

use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use fs_storage::FsStorage;
use serde_json::json;
use types::{Effect, Executed, Incarnation, LogLevel, execute, invariant};

use crate::inbox::Inbox;
use crate::logs::{Log, base};

/// The storage effects one Ready can leave outstanding, and more: a create or truncate, an
/// append, and their syncs.
pub const MAX_QUEUED_EFFECTS: usize = 8;

/// The controller's end of the I/O queue.
#[derive(Debug)]
pub struct Io {
    queue: SyncSender<Effect>,
}

impl Io {
    /// Starts the I/O thread over `storage`, for completions of `incarnation`.
    ///
    /// # Errors
    ///
    /// The operating system's error if the thread cannot be started.
    pub fn spawn(
        mut storage: FsStorage,
        incarnation: Incarnation,
        inbox: Arc<Inbox>,
        log: Log,
    ) -> std::io::Result<(Self, JoinHandle<FsStorage>)> {
        let (queue, effects) = mpsc::sync_channel::<Effect>(MAX_QUEUED_EFFECTS);
        let thread = thread::Builder::new()
            .name("keyval-io".to_owned())
            .spawn(move || {
                for effect in effects {
                    match execute(&mut storage, &effect, incarnation) {
                        Executed::Done => {}
                        Executed::Completion(input) => {
                            if let types::Input::StorageFailed { op, .. } = &input {
                                let mut record = base(LogLevel::Error, "storage_failed");
                                record.insert("op".to_owned(), json!(format!("{op:?}")));
                                record.insert(
                                    "cause".to_owned(),
                                    json!(storage.last_failure().map(|failure| format!(
                                        "{} {}: {}",
                                        failure.op,
                                        failure.path.display(),
                                        failure.cause
                                    ))),
                                );
                                log.record(record);
                            }
                            inbox.complete(input);
                        }
                        Executed::NotStorage => {
                            invariant!(
                                false,
                                "IO-STORAGE-ONLY",
                                "the I/O thread was handed a non-storage effect: {effect:?}"
                            );
                        }
                    }
                }
                storage
            })?;
        Ok((Self { queue }, thread))
    }

    /// Queues a storage effect, in emission order.
    ///
    /// # Panics
    ///
    /// `IO-QUEUE` if more storage effects are queued than one Ready can leave outstanding,
    /// or the I/O thread is gone.
    pub fn push(&self, effect: Effect) {
        let queued = self.queue.try_send(effect);
        invariant!(
            queued.is_ok(),
            "IO-QUEUE",
            "a storage effect could not be queued ({}): more than {MAX_QUEUED_EFFECTS} are \
             outstanding, or the I/O thread stopped",
            match queued {
                Ok(()) => "queued",
                Err(TrySendError::Full(_)) => "full",
                Err(TrySendError::Disconnected(_)) => "disconnected",
            }
        );
    }
}
