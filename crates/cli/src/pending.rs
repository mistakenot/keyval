//! The CLI's pending store (PLAN.md O-4, D-59, RISK-15).
//!
//! Before a write is sent, its request ID and exact command are persisted here; the record
//! is deleted only after a terminal result. A crash or power loss between persist and
//! response must never lose a record, so the order is fixed (D-59):
//!
//! 1. write the record to a temporary file in the store directory,
//! 2. `fsync` the temporary file,
//! 3. rename it to its final name,
//! 4. `fsync` the directory (after the rename, or the final name is not durable),
//! 5. only then may the caller send.
//!
//! Deleting is the mirror image: unlink, then `fsync` the directory. [`PendingStore::list`]
//! and [`PendingStore::load`] first recover what an interrupted persist left behind: a
//! complete temporary file is finished (synced, renamed); an incomplete one belongs to a
//! persist that never returned, so nothing was sent under it, and it is removed.
//!
//! Every filesystem step goes through [`Disk`], so the crash tests run the real store over
//! a simulated disk that loses what was not synced, and crash it at every step.
//!
//! This module is the store layer of W33; `pending` and `resume` build on it.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use types::RequestId;

/// The schema identifier stored in every record file.
pub const RECORD_SCHEMA: &str = "keyval.cli.pending/v1";
/// The suffix of a persisted record's file name.
const RECORD_SUFFIX: &str = ".json";
/// The suffix of the file a record is written to first.
const TEMP_SUFFIX: &str = ".json.tmp";
/// The name of the lock file; it matches no record or temporary file.
const LOCK_NAME: &str = ".lock";
/// The most records a store holds (CD-9); a person's unresolved writes stay far below it.
pub const MAX_RECORDS: usize = 10_000;
/// The largest record file, in bytes (CD-9): the 512 KiB request body, escaped in the
/// worst case (six bytes per byte as `\u00XX`), plus the envelope.
pub const MAX_RECORD_BYTES: usize = 3 * 1024 * 1024 + 4096;

/// The HTTP method of a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `PUT /v1/kv`.
    Put,
    /// `DELETE /v1/kv`.
    Delete,
    /// `POST /v1/cas`.
    Post,
}

impl Method {
    /// The method's name as sent.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Post => "POST",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Put, Self::Delete, Self::Post]
            .into_iter()
            .find(|method| method.as_str() == text)
    }
}

/// One write that has been persisted and not yet resolved.
///
/// Invariant: the request ID is valid by construction; `method`, `target` and `body` are
/// the exact request that was or will be sent, replayed verbatim by `resume`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The write's request ID (F-6), sent as `Idempotency-Key`.
    pub request_id: RequestId,
    /// The HTTP method.
    pub method: Method,
    /// The request target: path and query, as sent.
    pub target: String,
    /// The exact request body; empty for a bodiless request.
    pub body: String,
}

/// The record as stored: versioned, and strict about unknown fields so that a newer format
/// is refused rather than silently truncated.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema: String,
    request_id: String,
    method: String,
    target: String,
    body: String,
}

impl Record {
    fn encode(&self) -> Result<Vec<u8>, PendingError> {
        serde_json::to_vec(&Wire {
            schema: RECORD_SCHEMA.to_owned(),
            request_id: self.request_id.to_string(),
            method: self.method.as_str().to_owned(),
            target: self.target.clone(),
            body: self.body.clone(),
        })
        .map_err(|err| PendingError::BadRecord {
            name: self.request_id.to_string(),
            why: err.to_string(),
        })
    }

    fn decode(name: &str, bytes: &[u8]) -> Result<Self, PendingError> {
        let bad = |why: String| PendingError::BadRecord {
            name: name.to_owned(),
            why,
        };
        let wire: Wire = serde_json::from_slice(bytes).map_err(|err| bad(err.to_string()))?;
        if wire.schema != RECORD_SCHEMA {
            return Err(bad(format!(
                "schema {:?}, not {RECORD_SCHEMA}",
                wire.schema
            )));
        }
        let request_id = RequestId::parse(&wire.request_id).map_err(|err| bad(err.to_string()))?;
        let method = Method::parse(&wire.method)
            .ok_or_else(|| bad(format!("unknown method {:?}", wire.method)))?;
        Ok(Self {
            request_id,
            method,
            target: wire.target,
            body: wire.body,
        })
    }
}

/// Why a pending-store operation failed.
#[derive(Debug)]
pub enum PendingError {
    /// The filesystem refused; `what` names the step.
    Io {
        /// The step that failed.
        what: &'static str,
        /// The error.
        source: io::Error,
    },
    /// A record already exists under this ID with a different command: a request ID names
    /// one command for ever (C-6), so it is never overwritten.
    Conflict {
        /// The ID.
        request_id: RequestId,
    },
    /// A stored record cannot be read as one.
    BadRecord {
        /// The file or ID.
        name: String,
        /// Why.
        why: String,
    },
    /// The store already holds its maximum number of records.
    TooMany {
        /// The bound.
        max: usize,
    },
    /// The record would not fit in a record file.
    TooLarge {
        /// The encoded size in bytes.
        bytes: usize,
        /// The bound.
        max: usize,
    },
}

impl fmt::Display for PendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { what, source } => write!(f, "pending store: {what}: {source}"),
            Self::Conflict { request_id } => write!(
                f,
                "pending store: request {request_id} is already recorded with a different command"
            ),
            Self::BadRecord { name, why } => {
                write!(f, "pending store: {name} is not a valid record: {why}")
            }
            Self::TooMany { max } => write!(f, "pending store: more than {max} records"),
            Self::TooLarge { bytes, max } => {
                write!(f, "pending store: a record of {bytes} bytes; at most {max}")
            }
        }
    }
}

impl std::error::Error for PendingError {}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> PendingError {
    move |source| PendingError::Io { what, source }
}

/// The filesystem steps the store takes, by file name within the store directory. The real
/// implementation is [`FsDisk`]; the crash tests use a simulated disk.
pub trait Disk {
    /// What holds the store's exclusive lock until dropped.
    type Guard;
    /// Takes the exclusive lock, so concurrent processes never interleave.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn lock(&self) -> io::Result<Self::Guard>;
    /// The names of the files in the directory.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn list(&self) -> io::Result<Vec<String>>;
    /// The first `limit` bytes of the file, or `None` if it is absent.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn read(&self, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>>;
    /// Creates the file, which must not exist, with these bytes. Not yet synced.
    ///
    /// # Errors
    ///
    /// The filesystem's error; `AlreadyExists` if the file is there.
    fn create_new(&self, name: &str, bytes: &[u8]) -> io::Result<()>;
    /// Syncs the file's data.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn sync_file(&self, name: &str) -> io::Result<()>;
    /// Renames a file, replacing any file at the new name.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// Removes a file.
    ///
    /// # Errors
    ///
    /// The filesystem's error; `NotFound` if it is absent.
    fn remove(&self, name: &str) -> io::Result<()>;
    /// Syncs the directory, making renames and unlinks in it durable.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    fn sync_dir(&self) -> io::Result<()>;
}

/// The store on the real filesystem.
#[derive(Clone, Debug)]
pub struct FsDisk {
    dir: PathBuf,
}

impl FsDisk {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Disk for FsDisk {
    type Guard = File;

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.path(LOCK_NAME))?;
        file.lock()?;
        Ok(file)
    }

    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn read(&self, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let file = match File::open(self.path(name)) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut bytes = Vec::new();
        file.take(limit as u64).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    fn create_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path(name))?;
        file.write_all(bytes)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        File::open(self.path(name))?.sync_all()
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.path(from), self.path(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path(name))
    }

    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

/// A directory of pending records.
#[derive(Clone, Debug)]
pub struct PendingStore<D: Disk = FsDisk> {
    disk: D,
    max_records: usize,
    max_record_bytes: usize,
}

impl PendingStore {
    /// Opens the store at `dir`, creating it (and syncing its parent) if it is missing.
    ///
    /// # Errors
    ///
    /// [`PendingError::Io`] if the directory cannot be created or synced.
    pub fn open(dir: &Path) -> Result<Self, PendingError> {
        fs::create_dir_all(dir).map_err(io_err("create the store directory"))?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent)
                .and_then(|p| p.sync_all())
                .map_err(io_err("sync the store's parent directory"))?;
        }
        Ok(Self::with_disk(FsDisk {
            dir: dir.to_path_buf(),
        }))
    }

    /// The store's directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.disk.dir
    }
}

fn record_name(id: RequestId) -> String {
    format!("{id}{RECORD_SUFFIX}")
}

fn temp_name(id: RequestId) -> String {
    format!("{id}{TEMP_SUFFIX}")
}

impl<D: Disk> PendingStore<D> {
    /// A store over `disk` with the default bounds.
    #[must_use]
    pub const fn with_disk(disk: D) -> Self {
        Self {
            disk,
            max_records: MAX_RECORDS,
            max_record_bytes: MAX_RECORD_BYTES,
        }
    }

    /// The same store with smaller bounds, for boundary tests.
    #[must_use]
    pub const fn with_bounds(mut self, max_records: usize, max_record_bytes: usize) -> Self {
        self.max_records = max_records;
        self.max_record_bytes = max_record_bytes;
        self
    }

    /// Persists `record` durably (D-59). When it returns `Ok`, the record survives a crash
    /// or power loss and the caller may send. Persisting the identical record again is a
    /// success (a retry of the same command) that first makes the existing record durable.
    ///
    /// A leftover temporary file is recovered first, so a command that was persisted but
    /// never confirmed sent can hold its ID: a later, different command under it is a
    /// [`PendingError::Conflict`], which is the safe side of C-6.
    ///
    /// # Errors
    ///
    /// [`PendingError::Conflict`] if the ID is recorded with a different command;
    /// [`PendingError::TooLarge`] or [`PendingError::TooMany`] past the bounds, before
    /// anything is written; [`PendingError::Io`] otherwise.
    pub fn persist(&self, record: &Record) -> Result<(), PendingError> {
        let bytes = record.encode()?;
        if bytes.len() > self.max_record_bytes {
            return Err(PendingError::TooLarge {
                bytes: bytes.len(),
                max: self.max_record_bytes,
            });
        }
        let _lock = self.disk.lock().map_err(io_err("take the store lock"))?;
        self.recover_locked()?;
        let id = record.request_id;
        let (record_name, temp_name) = (record_name(id), temp_name(id));
        if let Some(existing) = self.read_record(&record_name)? {
            // recover_locked synced the directory, so the name is durable; sync the file
            // too, as an earlier attempt may have died before it did.
            return if &existing == record {
                self.disk
                    .sync_file(&record_name)
                    .map_err(io_err("sync the existing record"))
            } else {
                Err(PendingError::Conflict { request_id: id })
            };
        }
        if self.stems(RECORD_SUFFIX)?.len() >= self.max_records {
            return Err(PendingError::TooMany {
                max: self.max_records,
            });
        }
        self.disk
            .create_new(&temp_name, &bytes)
            .map_err(io_err("write the temporary record"))?;
        self.disk
            .sync_file(&temp_name)
            .map_err(io_err("sync the temporary record"))?;
        self.disk
            .rename(&temp_name, &record_name)
            .map_err(io_err("rename the record into place"))?;
        self.disk
            .sync_dir()
            .map_err(io_err("sync the store directory"))
    }

    /// Deletes the record for `id` once its terminal result is known, then syncs the
    /// directory. Deleting an absent record succeeds.
    ///
    /// # Errors
    ///
    /// [`PendingError::Io`] if the unlink or the sync fails.
    pub fn remove(&self, id: RequestId) -> Result<(), PendingError> {
        let _lock = self.disk.lock().map_err(io_err("take the store lock"))?;
        match self.disk.remove(&record_name(id)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_err("delete the record")(err)),
        }
        self.disk
            .sync_dir()
            .map_err(io_err("sync the store directory"))
    }

    /// The record for `id`, after recovering leftover temporary files. The record returned
    /// is durable: the directory is synced before it is read, so a caller may send it.
    ///
    /// # Errors
    ///
    /// [`PendingError::BadRecord`] if the stored file is unreadable as a record;
    /// [`PendingError::Io`] otherwise.
    pub fn load(&self, id: RequestId) -> Result<Option<Record>, PendingError> {
        let _lock = self.disk.lock().map_err(io_err("take the store lock"))?;
        self.recover_locked()?;
        self.read_record(&record_name(id))
    }

    /// Every unresolved record in ID order, after recovering leftover temporary files and
    /// syncing the directory. Files not named by a request ID are not the store's and are
    /// ignored.
    ///
    /// # Errors
    ///
    /// [`PendingError::TooMany`] past the bound; [`PendingError::BadRecord`] for an
    /// unreadable record (never skipped silently); [`PendingError::Io`] otherwise.
    pub fn list(&self) -> Result<Vec<Record>, PendingError> {
        let _lock = self.disk.lock().map_err(io_err("take the store lock"))?;
        self.recover_locked()?;
        let mut records = Vec::new();
        for stem in self.stems(RECORD_SUFFIX)? {
            if let Some(record) = self.read_record(&format!("{stem}{RECORD_SUFFIX}"))? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Finishes or discards every leftover temporary file (D-59); [`PendingStore::list`]
    /// and [`PendingStore::load`] do this themselves.
    ///
    /// # Errors
    ///
    /// [`PendingError::Io`] on a filesystem failure.
    pub fn recover(&self) -> Result<(), PendingError> {
        let _lock = self.disk.lock().map_err(io_err("take the store lock"))?;
        self.recover_locked()
    }

    /// [`PendingStore::recover`] with the lock held. A temporary file holding a complete
    /// record whose final name is absent is synced and renamed into place; one whose record
    /// is already final, is incomplete (unparseable) or names another ID belongs to a persist
    /// that never returned (nothing was sent) and is removed. A read error is never taken
    /// for an incomplete file. The directory is always synced afterwards, so every name
    /// listed or loaded next is durable.
    fn recover_locked(&self) -> Result<(), PendingError> {
        for stem in self.stems(TEMP_SUFFIX)? {
            let temp = format!("{stem}{TEMP_SUFFIX}");
            let name = format!("{stem}{RECORD_SUFFIX}");
            let final_exists = self
                .disk
                .read(&name, 0)
                .map_err(io_err("look for a record"))?
                .is_some();
            match self.read_record(&temp) {
                Ok(Some(_)) if !final_exists => {
                    self.disk
                        .sync_file(&temp)
                        .map_err(io_err("sync a leftover record"))?;
                    self.disk
                        .rename(&temp, &name)
                        .map_err(io_err("rename a leftover record into place"))?;
                }
                Ok(None) => {}
                Ok(Some(_)) | Err(PendingError::BadRecord { .. }) => {
                    self.disk
                        .remove(&temp)
                        .map_err(io_err("remove a leftover temporary file"))?;
                }
                Err(err) => return Err(err),
            }
        }
        self.disk
            .sync_dir()
            .map_err(io_err("sync the store directory"))
    }

    /// Reads the record in file `name`; `None` if absent. A record filed under another ID
    /// could never be removed by its own, so it is reported.
    fn read_record(&self, name: &str) -> Result<Option<Record>, PendingError> {
        let Some(bytes) = self
            .disk
            .read(name, self.max_record_bytes + 1)
            .map_err(io_err("read a record"))?
        else {
            return Ok(None);
        };
        if bytes.len() > self.max_record_bytes {
            return Err(PendingError::BadRecord {
                name: name.to_owned(),
                why: format!("more than {} bytes", self.max_record_bytes),
            });
        }
        let record = Record::decode(name, &bytes)?;
        let stem = name
            .strip_suffix(TEMP_SUFFIX)
            .or_else(|| name.strip_suffix(RECORD_SUFFIX))
            .unwrap_or(name);
        if record.request_id.to_string() != stem {
            return Err(PendingError::BadRecord {
                name: name.to_owned(),
                why: format!("it holds request {}", record.request_id),
            });
        }
        Ok(Some(record))
    }

    /// The sorted stems of the store's files ending in `suffix` whose stem is a request ID
    /// (`.json` does not match `.json.tmp`; the lock and foreign files never match).
    fn stems(&self, suffix: &str) -> Result<Vec<String>, PendingError> {
        let names = self
            .disk
            .list()
            .map_err(io_err("read the store directory"))?;
        let mut stems: Vec<String> = names
            .iter()
            .filter_map(|name| name.strip_suffix(suffix))
            .filter(|stem| RequestId::parse(stem).is_ok())
            .map(str::to_owned)
            .collect();
        stems.sort();
        Ok(stems)
    }
}

#[cfg(test)]
mod tests;
