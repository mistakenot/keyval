//! Tests of the pending store: round trips and bounds on the real filesystem, and the crash
//! matrix over a simulated disk that loses what was not synced (O-4, D-59).

#![cfg(test)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::rc::Rc;

use super::*;

const ID_A: &str = "d70ff69df7084a088e4a531837cc4e5a";
const ID_B: &str = "00000000000000000000000000000001";

fn id(text: &str) -> RequestId {
    RequestId::parse(text).unwrap()
}

fn record(text: &str, body: &str) -> Record {
    Record {
        request_id: id(text),
        method: Method::Put,
        target: "/v1/kv?key=greeting".to_owned(),
        body: body.to_owned(),
    }
}

/// A fresh store directory under the target directory, never /tmp or the source tree.
fn store(name: &str) -> PendingStore {
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .parent()
        .unwrap()
        .join("keyval-scratch")
        .join(format!("pending-{}-{name}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    PendingStore::open(&dir).unwrap()
}

// check: PENDING-STORE
#[test]
fn a_persisted_record_is_listed_and_loaded_verbatim() {
    let s = store("verbatim");
    let r = record(ID_A, "{\"value\":\"hello\"}");
    s.persist(&r).unwrap();
    assert_eq!(
        s.list().unwrap(),
        vec![r.clone()],
        "rule: list returns the exact command"
    );
    let again = PendingStore::open(s.dir()).unwrap();
    assert_eq!(
        again.load(id(ID_A)).unwrap(),
        Some(r),
        "rule: a new process finds the exact command"
    );
}

#[test]
fn records_list_in_id_order_and_foreign_files_are_ignored() {
    let s = store("order");
    s.persist(&record(ID_A, "a")).unwrap();
    s.persist(&record(ID_B, "b")).unwrap();
    fs::write(s.dir().join("notes.json"), b"not ours").unwrap();
    let ids: Vec<_> = s
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.request_id)
        .collect();
    assert_eq!(
        ids,
        vec![id(ID_B), id(ID_A)],
        "rule: sorted by ID, foreign file ignored"
    );
}

#[test]
fn a_corrupt_or_misfiled_record_is_reported_not_skipped() {
    let s = store("corrupt");
    s.persist(&record(ID_A, "a")).unwrap();
    s.persist(&record(ID_B, "b")).unwrap();
    fs::write(s.dir().join(format!("{ID_A}.json")), b"{not json").unwrap();
    let err = s.list().unwrap_err();
    assert!(
        matches!(err, PendingError::BadRecord { .. }),
        "rule: P10; got {err}"
    );
    // A record filed under another ID.
    fs::copy(
        s.dir().join(format!("{ID_B}.json")),
        s.dir().join(format!("{ID_A}.json")),
    )
    .unwrap();
    let err = s.load(id(ID_A)).unwrap_err();
    assert!(
        matches!(err, PendingError::BadRecord { .. }),
        "rule: filed under another ID; got {err}"
    );
}

#[test]
fn a_record_of_another_schema_or_with_unknown_fields_is_refused() {
    let s = store("schema");
    let name = format!("{ID_A}.json");
    for (what, text) in [
        (
            "another schema",
            format!(
                r#"{{"schema":"keyval.cli.pending/v2","request_id":"{ID_A}","method":"PUT","target":"/","body":""}}"#
            ),
        ),
        (
            "an unknown field",
            format!(
                r#"{{"schema":"{RECORD_SCHEMA}","request_id":"{ID_A}","method":"PUT","target":"/","body":"","attempts":3}}"#
            ),
        ),
        (
            "an unknown method",
            format!(
                r#"{{"schema":"{RECORD_SCHEMA}","request_id":"{ID_A}","method":"GET","target":"/","body":""}}"#
            ),
        ),
    ] {
        fs::write(s.dir().join(&name), text).unwrap();
        let err = s.load(id(ID_A)).unwrap_err();
        assert!(
            matches!(err, PendingError::BadRecord { .. }),
            "rule: {what} is refused; got {err}"
        );
    }
}

#[test]
fn a_different_command_under_a_recorded_id_is_refused() {
    let s = store("conflict");
    s.persist(&record(ID_A, "one")).unwrap();
    s.persist(&record(ID_A, "one")).unwrap();
    let err = s.persist(&record(ID_A, "two")).unwrap_err();
    assert!(
        matches!(err, PendingError::Conflict { .. }),
        "rule: no overwrite (C-6); got {err}"
    );
    assert_eq!(s.list().unwrap(), vec![record(ID_A, "one")]);
}

#[test]
fn two_processes_persisting_one_id_leave_exactly_one_command() {
    let s = store("race");
    let results = std::thread::scope(|scope| {
        // A separate handle each, as a separate process would have.
        let handles = ["one", "two"].map(|body| {
            let handle = PendingStore::open(s.dir()).unwrap();
            scope.spawn(move || handle.persist(&record(ID_A, body)))
        });
        handles.map(|h| h.join().unwrap())
    });
    let oks = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        oks, 1,
        "rule: exactly one command wins the ID (C-6); results={results:?}"
    );
    let kept = s.list().unwrap();
    assert_eq!(kept.len(), 1, "rule: one record; kept={kept:?}");
}

#[test]
fn record_count_and_size_are_bounded_on_write() {
    let s = store("bounds").with_bounds(2, 400);
    let first = record(ID_A, "a");
    s.persist(&first).unwrap();
    s.persist(&record(ID_B, "b")).unwrap();
    let third = "00000000000000000000000000000002";
    let err = s.persist(&record(third, "c")).unwrap_err();
    assert!(
        matches!(err, PendingError::TooMany { max: 2 }),
        "rule: N+1 is refused; got {err}"
    );
    s.persist(&first).unwrap();
    s.remove(id(ID_B)).unwrap();
    s.persist(&record(third, "c")).unwrap();

    let size = record(ID_A, "").encode().unwrap().len();
    let exact = PendingStore::open(store("size").dir())
        .unwrap()
        .with_bounds(10, size + 10);
    exact.persist(&record(ID_A, &"x".repeat(10))).unwrap();
    let err = exact.persist(&record(ID_B, &"x".repeat(11))).unwrap_err();
    assert!(
        matches!(err, PendingError::TooLarge { .. }),
        "rule: N+1 bytes is refused; got {err}"
    );
    assert!(
        !exact.dir().join(format!("{ID_B}.json.tmp")).exists(),
        "rule: nothing is written for a refused record"
    );
}

/// A disk that keeps what was written in memory and loses what was not synced when the
/// power fails: the data of a file never synced, and (in the strict model) a directory
/// entry never synced. `crash_at` fails the n-th mutating step and every step after it.
#[derive(Clone, Default)]
struct SimDisk(Rc<RefCell<Sim>>);

#[derive(Default)]
struct Sim {
    next_inode: u64,
    /// Inode -> (data, whether the data was synced).
    inodes: BTreeMap<u64, (Vec<u8>, bool)>,
    entries: BTreeMap<String, u64>,
    durable_entries: BTreeMap<String, u64>,
    steps: Vec<&'static str>,
    crash_at: Option<usize>,
    crashed: bool,
    /// Bug injection: a sync that does nothing.
    skip_file_sync: bool,
    skip_dir_sync: bool,
}

impl SimDisk {
    fn step(&self, what: &'static str) -> io::Result<()> {
        let mut sim = self.0.borrow_mut();
        if sim.crashed || sim.crash_at == Some(sim.steps.len()) {
            sim.crashed = true;
            return Err(io::Error::other("the process was killed here"));
        }
        sim.steps.push(what);
        Ok(())
    }

    fn steps(&self) -> Vec<&'static str> {
        self.0.borrow().steps.clone()
    }

    fn crash_at(&self, n: usize) {
        let mut sim = self.0.borrow_mut();
        sim.crash_at = Some(n);
        sim.crashed = false;
        sim.steps.clear();
    }

    /// The disk after a power loss and reboot. `lucky` keeps every directory entry the
    /// kernel had written back; otherwise only synced entries survive. Unsynced file data
    /// is always lost.
    fn after_power_loss(&self, lucky: bool) -> Self {
        let sim = self.0.borrow();
        let entries = if lucky {
            &sim.entries
        } else {
            &sim.durable_entries
        };
        let inodes: BTreeMap<u64, (Vec<u8>, bool)> = sim
            .inodes
            .iter()
            .map(|(n, (data, synced))| {
                (*n, (if *synced { data.clone() } else { Vec::new() }, true))
            })
            .collect();
        Self(Rc::new(RefCell::new(Sim {
            next_inode: sim.next_inode,
            inodes,
            entries: entries.clone(),
            durable_entries: entries.clone(),
            ..Sim::default()
        })))
    }
}

impl Disk for SimDisk {
    type Guard = ();

    fn lock(&self) -> io::Result<()> {
        Ok(())
    }

    fn list(&self) -> io::Result<Vec<String>> {
        Ok(self.0.borrow().entries.keys().cloned().collect())
    }

    fn read(&self, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let sim = self.0.borrow();
        Ok(sim.entries.get(name).map(|n| {
            let data = &sim.inodes[n].0;
            data[..data.len().min(limit)].to_vec()
        }))
    }

    fn create_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.step("create_new")?;
        let mut sim = self.0.borrow_mut();
        if sim.entries.contains_key(name) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let inode = sim.next_inode;
        sim.next_inode += 1;
        sim.inodes.insert(inode, (bytes.to_vec(), false));
        sim.entries.insert(name.to_owned(), inode);
        Ok(())
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.step("sync_file")?;
        let mut sim = self.0.borrow_mut();
        if sim.skip_file_sync {
            return Ok(());
        }
        let inode = *sim.entries.get(name).ok_or(io::ErrorKind::NotFound)?;
        sim.inodes.get_mut(&inode).ok_or(io::ErrorKind::NotFound)?.1 = true;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.step("rename")?;
        let mut sim = self.0.borrow_mut();
        let inode = sim.entries.remove(from).ok_or(io::ErrorKind::NotFound)?;
        sim.entries.insert(to.to_owned(), inode);
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.step("remove")?;
        let mut sim = self.0.borrow_mut();
        sim.entries
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.step("sync_dir")?;
        let mut sim = self.0.borrow_mut();
        if !sim.skip_dir_sync {
            sim.durable_entries = sim.entries.clone();
        }
        Ok(())
    }
}

/// The steps of a clean persist on an empty store: recovery's directory sync, then D-59.
const PERSIST_STEPS: [&str; 5] = ["sync_dir", "create_new", "sync_file", "rename", "sync_dir"];

fn sim_store(disk: &SimDisk) -> PendingStore<SimDisk> {
    PendingStore::with_disk(disk.clone())
}

#[test]
fn a_persist_takes_the_d59_steps_in_order() {
    let disk = SimDisk::default();
    sim_store(&disk).persist(&record(ID_A, "x")).unwrap();
    assert_eq!(
        disk.steps(),
        PERSIST_STEPS,
        "rule: D-59 order, temp -> fsync temp -> rename -> fsync directory"
    );
}

/// Persists on a disk that is killed at step `n`, loses power, and returns the rebooted
/// store's listing together with whether the persist returned `Ok` (the caller may send).
fn kill_persist_at(n: usize, lucky: bool, tweak: impl Fn(&mut Sim)) -> (bool, Vec<Record>) {
    let disk = SimDisk::default();
    tweak(&mut disk.0.borrow_mut());
    disk.crash_at(n);
    let returned = sim_store(&disk).persist(&record(ID_A, "x")).is_ok();
    let rebooted = sim_store(&disk.after_power_loss(lucky));
    let listed = rebooted.list().unwrap_or_else(|err| {
        panic!(
            "rule: a rebooted store lists without error; killed at step {n} (lucky={lucky}): {err}"
        )
    });
    (returned, listed)
}

#[test]
fn a_kill_at_every_step_of_a_persist_never_loses_a_returned_record() {
    for lucky in [false, true] {
        for n in 0..=PERSIST_STEPS.len() {
            let (returned, listed) = kill_persist_at(n, lucky, |_| {});
            if returned {
                assert_eq!(
                    listed,
                    vec![record(ID_A, "x")],
                    "rule: once persist returned, the record survives (O-4); killed at step {n}, lucky={lucky}"
                );
            } else {
                assert!(
                    listed.is_empty() || listed == vec![record(ID_A, "x")],
                    "rule: a killed persist leaves nothing or the whole record; killed at step {n}, lucky={lucky}: {listed:?}"
                );
            }
        }
    }
}

#[test]
fn the_crash_matrix_catches_a_persist_that_skips_a_sync() {
    // Bug injection: the same matrix, over a disk whose syncs do nothing, must fail. If it
    // did not, the matrix would prove nothing about the order.
    let loses_a_returned_record = |tweak: fn(&mut Sim)| {
        [false, true].into_iter().any(|lucky| {
            let disk = SimDisk::default();
            tweak(&mut disk.0.borrow_mut());
            let returned = sim_store(&disk).persist(&record(ID_A, "x")).is_ok();
            let rebooted = sim_store(&disk.after_power_loss(lucky));
            returned && rebooted.list().ok() != Some(vec![record(ID_A, "x")])
        })
    };
    assert!(
        loses_a_returned_record(|sim| sim.skip_file_sync = true),
        "rule: without the file sync a returned record is lost"
    );
    assert!(
        loses_a_returned_record(|sim| sim.skip_dir_sync = true),
        "rule: without the directory sync a returned record is lost"
    );
}

#[test]
fn a_retry_after_a_kill_before_the_directory_sync_makes_the_record_durable() {
    let disk = SimDisk::default();
    disk.crash_at(4);
    assert!(sim_store(&disk).persist(&record(ID_A, "x")).is_err());
    // Rebooted, the rename was written back but never synced.
    let lucky = disk.after_power_loss(true);
    let retry = sim_store(&lucky);
    retry.persist(&record(ID_A, "x")).unwrap();
    let strict = sim_store(&lucky.after_power_loss(false));
    assert_eq!(
        strict.list().unwrap(),
        vec![record(ID_A, "x")],
        "rule: an idempotent persist syncs before it returns Ok"
    );
}

#[test]
fn a_kill_between_response_and_delete_keeps_the_record_for_resume() {
    for lucky in [false, true] {
        for n in 0..=2 {
            let disk = SimDisk::default();
            sim_store(&disk).persist(&record(ID_A, "x")).unwrap();
            disk.crash_at(n);
            let removed = sim_store(&disk).remove(id(ID_A)).is_ok();
            let listed = sim_store(&disk.after_power_loss(lucky)).list().unwrap();
            if removed {
                assert!(
                    listed.is_empty(),
                    "rule: a returned delete is durable; n={n} lucky={lucky}"
                );
            } else {
                assert!(
                    listed.is_empty() || listed == vec![record(ID_A, "x")],
                    "rule: a killed delete leaves the whole record or nothing; n={n} lucky={lucky}: {listed:?}"
                );
            }
            if n == 0 {
                assert_eq!(
                    listed,
                    vec![record(ID_A, "x")],
                    "rule: killed before the delete, resume still has the record (O-4); lucky={lucky}"
                );
            }
        }
    }
}

#[test]
fn recovery_finishes_a_complete_leftover_and_discards_a_partial_one() {
    // Kill after the temp is synced (step 3 of 5): lucky, the temp survives complete.
    let (returned, listed) = kill_persist_at(3, true, |_| {});
    assert!(!returned);
    assert_eq!(
        listed,
        vec![record(ID_A, "x")],
        "rule: pending recovers a complete leftover temp"
    );
    // Kill after the temp is written but before it is synced: lucky, it survives empty.
    let (returned, listed) = kill_persist_at(2, true, |_| {});
    assert!(!returned);
    assert!(
        listed.is_empty(),
        "rule: a partial temp is discarded, nothing was sent"
    );
}

#[test]
fn recovery_does_not_take_a_read_error_for_an_incomplete_file() {
    // A temp that cannot be read must stop recovery, not be deleted as incomplete.
    struct Unreadable(SimDisk);
    impl Disk for Unreadable {
        type Guard = ();
        fn lock(&self) -> io::Result<()> {
            Ok(())
        }
        fn list(&self) -> io::Result<Vec<String>> {
            self.0.list()
        }
        fn read(&self, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
            if std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "tmp")
            {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                self.0.read(name, limit)
            }
        }
        fn create_new(&self, n: &str, b: &[u8]) -> io::Result<()> {
            self.0.create_new(n, b)
        }
        fn sync_file(&self, n: &str) -> io::Result<()> {
            self.0.sync_file(n)
        }
        fn rename(&self, f: &str, t: &str) -> io::Result<()> {
            self.0.rename(f, t)
        }
        fn remove(&self, n: &str) -> io::Result<()> {
            self.0.remove(n)
        }
        fn sync_dir(&self) -> io::Result<()> {
            self.0.sync_dir()
        }
    }
    let disk = SimDisk::default();
    disk.crash_at(3);
    assert!(sim_store(&disk).persist(&record(ID_A, "x")).is_err());
    let lucky = disk.after_power_loss(true);
    let err = PendingStore::with_disk(Unreadable(lucky.clone()))
        .list()
        .unwrap_err();
    assert!(
        matches!(err, PendingError::Io { .. }),
        "rule: a read error is an error; got {err}"
    );
    assert!(
        lucky
            .0
            .borrow()
            .entries
            .contains_key(&format!("{ID_A}.json.tmp")),
        "rule: the unreadable temp is kept"
    );
}
