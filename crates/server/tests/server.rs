//! keyval-server's library tests on real data directories: `init` and the start's preflight
//! (PLAN.md §7.7 step 0 and steps 1–4, D-52, DUR-7, DUR-8; W18.1), and the running node's
//! wiring (§7.12, O-3; W18.2). This target never spawns a process, so the data-directory
//! locks it takes can never leak into a child (br-o8co, br-b7xl); the binary and its
//! `--json` output are tested by `tests/binary.rs`, which never takes a lock itself.
#![cfg(test)]

// check: SERVER-INIT

#[path = "support/mod.rs"]
mod support;

#[path = "support/store.rs"]
mod store;

mod init_tests {
    //! `keyval-server init` (§7.7 step 0, DUR-7, DUR-8, D-52, D-63).

    use std::fs;

    use fs_storage::{DataDirCreated, FsStorage, create_data_dir};
    use server::init::{InitRefusal, Refused, Resumed, init, initialise};
    use types::{ByteCount, DataDir, InstanceId, StorageError};

    use crate::store::{Recording, recovered, three_voters};
    use crate::support::{
        data_dir, genesis_frame, instance, journal_bytes, node, one_voter, scratch,
    };

    #[test]
    fn init_writes_the_genesis_through_the_port_in_step_0_order() {
        let dir = scratch("init-order");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        assert_eq!(create_data_dir(&data), Ok(DataDirCreated::New));
        let mut storage = Recording::open(&data, None);
        let written = initialise(&mut storage, &config, node(1), instance()).unwrap();
        let frame = genesis_frame(&config, 1);
        assert_eq!(
            (
                storage.calls,
                written.resumed,
                written.journal_bytes.get(),
                journal_bytes(&data)
            ),
            (
                vec!["lock", "create_journal", "sync_dir", "append", "sync"],
                None,
                u64::try_from(frame.len()).unwrap(),
                Some(frame)
            ),
            "rule: init takes the lock, creates journal.log and syncs the directory before \
             writing into it, then appends the genesis frame and syncs it, and the journal is \
             exactly that frame (§7.7 step 0, DUR-7)"
        );
    }

    #[test]
    fn init_creates_the_data_directory_beside_the_config_and_binds_it() {
        let dir = scratch("init-binds");
        let (_, config) = one_voter(&dir, |text| text);
        let initialised = init(&config, node(1), instance()).unwrap();
        let data = dir.join("node-1");
        let recovered = recovered(&config, 1, &data);
        let genesis = recovered.genesis();
        assert_eq!(
            (
                initialised.data_dir.clone(),
                initialised.directory,
                data.join(fs_storage::LOCK).is_file(),
                genesis.instance_id(),
                genesis.node_id(),
                genesis.verify(&config, node(1)),
                genesis.clone(),
            ),
            (
                data,
                DataDirCreated::New,
                true,
                instance(),
                node(1),
                Ok(()),
                initialised.genesis,
            ),
            "rule: init creates the data directory, resolved against the config file's \
             directory, and binds it to the instance ID, the node, the voters and the semantic \
             configuration; a start recovers that genesis (§7.7 step 0, DUR-8)"
        );
    }

    #[test]
    fn an_interrupted_init_is_completed_over_an_empty_journal_or_an_incomplete_genesis() {
        let dir = scratch("init-resume");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        let frame = genesis_frame(&config, 1);
        // The crash came after the directory was created: no journal yet.
        assert_eq!(create_data_dir(&data), Ok(DataDirCreated::New));
        let no_journal = init(&config, node(1), instance()).unwrap();
        // The crash came before the genesis was appended: an empty journal.
        fs::write(data.join(fs_storage::JOURNAL), b"").unwrap();
        let empty = init(&config, node(1), instance()).unwrap();
        // The crash came during the genesis's append: part of the frame.
        fs::write(data.join(fs_storage::JOURNAL), &frame[..frame.len() / 2]).unwrap();
        let mut storage = Recording::open(&data, None);
        let partial = initialise(&mut storage, &config, node(1), instance()).unwrap();
        let half = ByteCount::new(u64::try_from(frame.len() / 2).unwrap());
        assert_eq!(
            (
                (no_journal.directory, no_journal.resumed),
                (empty.directory, empty.resumed),
                partial.resumed,
                storage.calls,
                journal_bytes(&data),
            ),
            (
                (DataDirCreated::Existing, None),
                (DataDirCreated::Existing, Some(Resumed::EmptyJournal)),
                Some(Resumed::IncompleteGenesis { bytes: half }),
                vec!["lock", "sync_dir", "truncate", "append", "sync"],
                Some(frame),
            ),
            "rule: init re-runs over what a crash during init leaves: the directory alone, an \
             empty journal, or part of the genesis frame, which it truncates away before \
             writing the genesis again; it syncs the directory every time, since a re-run \
             cannot know whether the first run's sync happened (§7.7 step 0, DUR-7)"
        );
    }

    #[test]
    fn init_never_reinitialises_a_journal_that_holds_more_than_an_interrupted_init_leaves() {
        let dir = scratch("init-never");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        init(&config, node(1), instance()).unwrap();
        let before = journal_bytes(&data);
        let other = InstanceId::from_bytes([7; 16]);
        let again = init(&config, node(1), instance()).unwrap_err();
        let another = init(&config, node(1), other).unwrap_err();
        // The same directory under another voter set: a genesis for another configuration.
        let (_, three) = three_voters(&dir);
        let foreign = init(&three, node(1), instance()).unwrap_err();
        let after = journal_bytes(&data);
        // Content where a genesis belongs, longer than any genesis frame.
        let junk = scratch("init-junk");
        let (_, junk_config) = one_voter(&junk, |text| text);
        let junk_data = data_dir(&junk_config, 1);
        assert_eq!(create_data_dir(&junk_data), Ok(DataDirCreated::New));
        fs::write(junk_data.join(fs_storage::JOURNAL), vec![0x5a; 4096]).unwrap();
        let not_init = init(&junk_config, node(1), instance()).unwrap_err();
        let refused_codes: Vec<&str> = [&again, &another, &foreign, &not_init]
            .iter()
            .map(|refusal| match refusal {
                InitRefusal::AlreadyInitialised { instance_id, .. }
                    if *instance_id == instance() =>
                {
                    "already_initialised"
                }
                InitRefusal::NotInitialisable { .. } => "not_initialisable",
                InitRefusal::UnknownNode { .. }
                | InitRefusal::DataDir(_)
                | InitRefusal::Locked { .. }
                | InitRefusal::AlreadyInitialised { .. }
                | InitRefusal::Storage { .. } => "other",
            })
            .collect();
        assert_eq!(
            (
                refused_codes,
                after == before,
                journal_bytes(&junk_data).map(|bytes| bytes.len())
            ),
            (
                vec![
                    "already_initialised",
                    "already_initialised",
                    "not_initialisable",
                    "not_initialisable"
                ],
                true,
                Some(4096)
            ),
            "rule: init refuses a directory whose journal holds a valid genesis, whatever \
             instance is asked for, and one holding a genesis for another configuration or \
             content where a genesis belongs; each is left exactly as it was (§7.7 step 0, \
             DUR-6, D-63); refusals: {again:?}, {another:?}, {foreign:?}, {not_init:?}"
        );
    }

    #[test]
    fn init_refuses_a_locked_directory_and_an_unknown_node_creating_nothing() {
        let dir = scratch("init-locked");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        assert_eq!(create_data_dir(&data), Ok(DataDirCreated::New));
        let mut holder = FsStorage::open(&data).unwrap();
        holder.lock().unwrap();
        let locked = init(&config, node(1), instance());
        let unknown = init(&config, node(9), instance());
        assert_eq!(
            (
                locked,
                journal_bytes(&data),
                unknown,
                dir.join("node-9").exists()
            ),
            (
                Err(InitRefusal::Locked {
                    data_dir: data.clone()
                }),
                None,
                Err(InitRefusal::UnknownNode { node: node(9) }),
                false
            ),
            "rule: while another process holds the data directory's lock init writes nothing \
             (DUR-8); a node that is not a voter is refused before anything is created"
        );
    }

    #[test]
    fn a_failed_storage_call_stops_init_at_that_step() {
        let dir = scratch("init-fails");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        assert_eq!(create_data_dir(&data), Ok(DataDirCreated::New));
        let mut failing_sync_dir = Recording::open(&data, Some("sync_dir"));
        let first = initialise(&mut failing_sync_dir, &config, node(1), instance());
        let after_first = journal_bytes(&data);
        let first_calls = failing_sync_dir.calls.clone();
        drop(failing_sync_dir);
        let mut failing_sync = Recording::open(&data, Some("sync"));
        let second = initialise(&mut failing_sync, &config, node(1), instance());
        assert_eq!(
            (
                (first, first_calls),
                after_first,
                second,
                failing_sync.calls
            ),
            (
                (
                    Err(Refused::Storage("sync_dir", StorageError::Failed)),
                    vec!["lock", "create_journal", "sync_dir"]
                ),
                Some(Vec::new()),
                Err(Refused::Storage("sync", StorageError::Failed)),
                vec!["lock", "sync_dir", "append", "sync"]
            ),
            "rule: when a sync fails init stops there and reports no genesis: nothing is \
             appended before the directory sync succeeds, and an unsynced genesis is never \
             reported as written (DUR-7, §7.7 step 0)"
        );
    }
}

mod run_tests {
    //! `keyval-server run`'s preflight (§7.7 steps 1–4, D-52, DUR-6, DUR-8).

    use std::fs;

    use fs_storage::{DataDirCreated, FsStorage, create_data_dir};
    use journal::{FrameKind, GenesisMismatch, encode};
    use server::init::init;
    use server::run::{Prepared, RunRefusal, Uninitialised, prepare};
    use types::{DataDir, StorageError};

    use crate::store::three_voters;
    use crate::support::{
        data_dir, genesis_frame, instance, journal_bytes, node, one_voter, scratch,
    };

    /// The refusal of `prepare`, or `None` if it passed.
    fn refusal(prepared: Result<Prepared, RunRefusal>) -> Option<RunRefusal> {
        prepared.err()
    }

    #[test]
    fn run_refuses_a_missing_data_directory_and_never_creates_it() {
        let dir = scratch("run-missing");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        let refused = refusal(prepare(&config, node(1)));
        assert_eq!(
            (refused, data.exists()),
            (
                Some(RunRefusal::MissingDataDir {
                    data_dir: data.clone()
                }),
                false
            ),
            "rule: a start refuses a missing data directory and never creates it: a node that \
             lost its data must not rejoin as a blank voter (D-52, §7.7 step 0); dir={data:?}"
        );
    }

    #[test]
    fn run_refuses_a_directory_without_a_valid_genesis_and_changes_nothing() {
        let dir = scratch("run-uninit");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        let frame = genesis_frame(&config, 1);
        assert_eq!(create_data_dir(&data), Ok(DataDirCreated::New));
        let no_journal = (refusal(prepare(&config, node(1))), journal_bytes(&data));
        fs::write(data.join(fs_storage::JOURNAL), b"").unwrap();
        let empty = (refusal(prepare(&config, node(1))), journal_bytes(&data));
        let half = frame[..frame.len() / 2].to_vec();
        fs::write(data.join(fs_storage::JOURNAL), &half).unwrap();
        let partial = (refusal(prepare(&config, node(1))), journal_bytes(&data));
        let uninitialised = |why| {
            Some(RunRefusal::Uninitialised {
                data_dir: data.clone(),
                why,
            })
        };
        assert_eq!(
            (no_journal, empty, partial),
            (
                (uninitialised(Uninitialised::NoJournal), None),
                (uninitialised(Uninitialised::EmptyJournal), Some(Vec::new())),
                (uninitialised(Uninitialised::IncompleteGenesis), Some(half)),
            ),
            "rule: a start refuses a directory with no journal, an empty journal, or an \
             incomplete genesis, and creates or rewrites nothing: only init initialises (D-52, \
             §7.7 step 0)"
        );
    }

    #[test]
    fn run_refuses_a_directory_bound_to_another_configuration() {
        let dir = scratch("run-mismatch");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        init(&config, node(1), instance()).unwrap();
        let before = journal_bytes(&data);
        let (_, three) = three_voters(&dir);
        let other_voters = refusal(prepare(&three, node(1)));
        let (_, smaller_keys) = one_voter(&dir, |text| {
            text.replace("max_key_bytes = 256", "max_key_bytes = 128")
        });
        let other_limits = refusal(prepare(&smaller_keys, node(1)));
        let mismatch = |refused: &Option<RunRefusal>| match refused {
            Some(RunRefusal::GenesisMismatch { mismatch, .. }) => Some(match mismatch {
                GenesisMismatch::Node { .. } => "node",
                GenesisMismatch::Voters { .. } => "voters",
                GenesisMismatch::Fingerprint { .. } => "fingerprint",
            }),
            Some(_) | None => None,
        };
        assert_eq!(
            (
                mismatch(&other_voters),
                mismatch(&other_limits),
                journal_bytes(&data) == before
            ),
            (Some("voters"), Some("fingerprint"), true),
            "rule: a start under another voter set or other semantic limits than the genesis \
             binds is refused, and the directory is not modified (DUR-8, §7.7 step 2); \
             refusals: {other_voters:?}, {other_limits:?}"
        );
    }

    #[test]
    fn run_refuses_a_locked_directory_and_a_damaged_journal() {
        let dir = scratch("run-locked");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        init(&config, node(1), instance()).unwrap();
        let mut holder = FsStorage::open(&data).unwrap();
        holder.lock().unwrap();
        let locked = refusal(prepare(&config, node(1)));
        drop(holder);
        // A checksum-valid genesis frame whose record does not decode: content, not a tear.
        let damaged = scratch("run-damaged");
        let (_, damaged_config) = one_voter(&damaged, |text| text);
        let damaged_data = data_dir(&damaged_config, 1);
        assert_eq!(create_data_dir(&damaged_data), Ok(DataDirCreated::New));
        let bogus = encode(FrameKind::Genesis, 1, b"not a genesis record").unwrap();
        fs::write(damaged_data.join(fs_storage::JOURNAL), &bogus).unwrap();
        let refused = refusal(prepare(&damaged_config, node(1)));
        let is_damaged = match &refused {
            Some(RunRefusal::Damaged { error, .. }) => !error.permits_init(),
            Some(_) | None => false,
        };
        assert_eq!(
            (
                locked,
                is_damaged,
                journal_bytes(&damaged_data) == Some(bogus)
            ),
            (Some(RunRefusal::Locked { data_dir: data }), true, true),
            "rule: a start refuses a directory another process holds (DUR-8) and a journal it \
             cannot recover, modifying nothing (DUR-6, §7.7 step 3); damaged: {refused:?}"
        );
    }

    #[test]
    fn run_passes_the_preflight_holding_the_lock_on_an_initialised_directory() {
        let dir = scratch("run-ready");
        let (_, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        init(&config, node(1), instance()).unwrap();
        let prepared = prepare(&config, node(1)).unwrap();
        let mut second = FsStorage::open(&data).unwrap();
        let while_held = second.lock();
        let preflight = (
            prepared.recovered.genesis().instance_id(),
            prepared.recovered.truncate_to(),
            prepared.data_dir.clone(),
        );
        drop(prepared);
        let after_release = second.lock();
        assert_eq!(
            (
                preflight,
                while_held,
                after_release,
                journal_bytes(&data) == Some(genesis_frame(&config, 1))
            ),
            (
                (instance(), None, data),
                Err(StorageError::Locked),
                Ok(()),
                true
            ),
            "rule: an initialised directory passes the preflight: the process holds its lock, \
             so no second process can use it (DUR-8), and has recovered its genesis (§7.7 steps \
             1-4), changing nothing"
        );
    }
}

mod runtime_tests {
    //! The production node (§7.12, O-3; W18.2): the serial controller, the hand-off, the
    //! I/O thread, the clock, boot entropy and the log, on a real data directory, driven
    //! only through `server::runtime`'s API.

    // check: SERVER-RUNTIME

    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use serde_json::Value as Json;
    use server::init::init;
    use server::run::prepare;
    use server::runtime::{Busy, Node, Options, Outputs, Snapshot, start};
    use types::{
        AttemptId, ClientOp, Config, Key, Lifecycle, Limit, Mutation, NodeId, RequestId, Response,
        Role, Value,
    };

    use crate::support::{instance, node, one_voter, scratch};

    /// A one-voter config with a 5 ms tick, so an election takes a fraction of a second.
    fn fast(dir: &Path) -> Config {
        one_voter(dir, |text| text.replace("tick_ms = 50", "tick_ms = 5")).1
    }

    /// Outputs that hand every answer to the returned receiver.
    fn collecting() -> (Outputs, Receiver<(AttemptId, Response)>) {
        let (answers, received) = mpsc::channel();
        (
            Outputs {
                respond: Box::new(move |attempt, response| {
                    let _ = answers.send((attempt, response));
                }),
                send: Box::new(|_, _| {}),
            },
            received,
        )
    }

    fn started(config: &Config, log: &Path, outputs: Outputs) -> Node {
        let prepared = prepare(config, node(1)).unwrap();
        start(
            prepared,
            config,
            node(1),
            outputs,
            &Options {
                log_file: log.to_path_buf(),
            },
        )
        .unwrap()
    }

    /// Waits until `done` holds of the node's published snapshot, at most 10 s.
    fn wait_for(running: &Node, what: &str, done: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = running.snapshot();
            if done(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "fixture: {what} within 10 s; last snapshot {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn leads(snapshot: &Snapshot) -> bool {
        snapshot.lifecycle == Lifecycle::Ready && snapshot.role == Role::Leader
    }

    /// The answer to `attempt`, skipping others, within 10 s.
    fn answer(answers: &Receiver<(AttemptId, Response)>, attempt: u64) -> Response {
        loop {
            let (id, response) = answers
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|err| panic!("fixture: attempt {attempt} answered: {err}"));
            if id.get() == attempt {
                return response;
            }
        }
    }

    fn put(config: &Config, n: u8, k: &str, v: &str) -> ClientOp {
        let limits = config.key_value_limits();
        ClientOp::Write {
            request_id: RequestId::from_bytes([n; 16]),
            mutation: Mutation::Put {
                key: Key::parse(k.to_owned(), limits).unwrap(),
                value: Value::parse(v.to_owned(), limits).unwrap(),
            },
        }
    }

    fn get(config: &Config, k: &str) -> ClientOp {
        ClientOp::Get {
            key: Key::parse(k.to_owned(), config.key_value_limits()).unwrap(),
        }
    }

    /// The value a GET found, if it found one.
    fn found(response: &Response) -> Option<String> {
        match response {
            Response::Found { value, .. } => Some(value.as_str().to_owned()),
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_) => None,
        }
    }

    /// Whether a write was executed now, not replayed.
    fn executed(response: &Response) -> bool {
        match response {
            Response::Written { replayed, .. } => !replayed,
            Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_) => false,
        }
    }

    fn log_records(path: &Path) -> Vec<Json> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_started_node_elects_itself_on_its_clock_answers_a_put_and_a_get_and_logs_it() {
        let dir = scratch("runtime-serve");
        let config = fast(&dir);
        init(&config, node(1), instance()).unwrap();
        let log = dir.join("node.log");
        let (outputs, answers) = collecting();
        let running = started(&config, &log, outputs);
        let led = wait_for(&running, "the node leads", leads);
        running
            .client(AttemptId::new(1), put(&config, 1, "k", "v"))
            .unwrap();
        let written = answer(&answers, 1);
        running
            .client(AttemptId::new(2), get(&config, "k"))
            .unwrap();
        let read = answer(&answers, 2);
        let seed = running.seed();
        let stopped = running.shutdown().unwrap();
        let records = log_records(&log);
        let has = |event: &str, field: &str, value: &Json| {
            records
                .iter()
                .any(|record| record["event"] == *event && record[field] == *value)
        };
        assert_eq!(
            (
                led.ticks > 0,
                executed(&written),
                found(&read),
                stopped.applied >= led.applied,
                has("boot", "entropy_seed", &Json::from(seed)),
                has("lifecycle_changed", "to", &Json::from("ready")),
                has("applied", "node", &Json::from(1)),
                has("stopped", "lifecycle", &Json::from("ready")),
            ),
            (
                true,
                true,
                Some("v".to_owned()),
                true,
                true,
                true,
                true,
                true
            ),
            "rule: the node's clock drives the core, which elects itself; a PUT is answered \
             once applied and a GET reads it; the log holds the boot seed, the lifecycle \
             change to ready, the observations and the stop, one JSON object per line (§7.12, \
             O-3, F-1, F-2); log: {records:?}"
        );
    }

    #[test]
    fn a_write_survives_a_restart_of_the_node_from_its_journal() {
        let dir = scratch("runtime-restart");
        let config = fast(&dir);
        init(&config, node(1), instance()).unwrap();
        let log = dir.join("node.log");
        let (outputs, answers) = collecting();
        let first = started(&config, &log, outputs);
        wait_for(&first, "the node leads", leads);
        first
            .client(AttemptId::new(1), put(&config, 1, "k", "kept"))
            .unwrap();
        let written = answer(&answers, 1);
        let first_seed = first.seed();
        let before = first.shutdown().unwrap();
        let (outputs, answers) = collecting();
        let second = started(&config, &log, outputs);
        // The last hard state on disk may still carry the commit index from before the
        // write: the new boot replays to it, then commits the rest again in its own term.
        wait_for(
            &second,
            "the restarted node applies what the first had",
            |s| leads(s) && s.applied >= before.applied,
        );
        second.client(AttemptId::new(2), get(&config, "k")).unwrap();
        let read = answer(&answers, 2);
        let second_seed = second.seed();
        second.shutdown().unwrap();
        assert_eq!(
            (executed(&written), found(&read), first_seed == second_seed),
            (true, Some("kept".to_owned()), false),
            "rule: a write answered by the node was synced through the I/O thread, so a new \
             boot recovers it from the journal without operator action, applies it again and \
             serves it; each boot draws a fresh seed (§7.12, DUR-3, §7.2)"
        );
    }

    #[test]
    fn the_clock_turns_monotonic_time_into_ticks_for_the_core() {
        let dir = scratch("runtime-clock");
        let config = fast(&dir);
        init(&config, node(1), instance()).unwrap();
        let (outputs, _answers) = collecting();
        let running = started(&config, &dir.join("node.log"), outputs);
        let before = wait_for(&running, "the clock starts", |s| s.ticks > 0);
        let started_at = Instant::now();
        let after = wait_for(&running, "100 more ticks", |s| {
            s.ticks >= before.ticks + 100
        });
        let elapsed_ms = started_at.elapsed().as_millis();
        running.shutdown().unwrap();
        let tick_ms = u128::from(config.tick_ms());
        assert!(
            u128::from(after.ticks - before.ticks) <= elapsed_ms / tick_ms + 2,
            "rule: the core processes the ticks the monotonic clock says elapsed, never more \
             (§7.10, §7.12): {} ticks in {elapsed_ms} ms at {tick_ms} ms per tick",
            after.ticks - before.ticks
        );
    }

    #[test]
    fn a_full_hand_off_answers_busy_and_drops_peer_messages_counting_both() {
        let dir = scratch("runtime-full");
        let config = fast(&dir);
        init(&config, node(1), instance()).unwrap();
        let bound = config.limit(Limit::ControllerQueueInputs);
        // The controller answers attempt 1, then blocks until released: it still holds that
        // attempt, so the one inbound bound leaves the hand-off room for bound - 1 more.
        let (entered, blocked) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let (answers_tx, answers) = mpsc::channel();
        let outputs = Outputs {
            respond: Box::new(move |attempt: AttemptId, response| {
                if attempt.get() == 1 {
                    let _ = entered.send(());
                    let _ = released.lock().unwrap().recv();
                }
                let _ = answers_tx.send((attempt, response));
            }),
            send: Box::new(|_, _| {}),
        };
        let running = started(&config, &dir.join("node.log"), outputs);
        wait_for(&running, "the node leads", leads);
        running
            .client(AttemptId::new(1), get(&config, "k"))
            .unwrap();
        blocked.recv_timeout(Duration::from_secs(10)).unwrap();
        let accepted = (2..=bound)
            .filter(|n| {
                running
                    .client(AttemptId::new(*n), get(&config, "k"))
                    .is_ok()
            })
            .count();
        let refused = running.client(AttemptId::new(1 + bound), get(&config, "k"));
        let peer = running.peer(NodeId::new(2).unwrap(), vec![0; 8]);
        release.send(()).unwrap();
        let last = answer(&answers, bound);
        let counted = wait_for(&running, "the counts are published", |s| {
            s.clients_refused == 1 && s.peers_dropped == 1
        });
        // Once the controller has judged what it held, the bound frees: a client retrying
        // busy gets in again and is answered.
        let deadline = Instant::now() + Duration::from_secs(10);
        let freed = loop {
            match running.client(AttemptId::new(2 + bound), get(&config, "k")) {
                Ok(()) => break true,
                Err(Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(Busy) => break false,
            }
        };
        let answered = freed && matches!(answer(&answers, 2 + bound), Response::NotFound { .. });
        running.shutdown().unwrap();
        assert_eq!(
            (
                accepted,
                refused,
                peer,
                matches!(last, Response::NotFound { .. }),
                (counted.clients_refused, counted.peers_dropped),
                answered
            ),
            (
                usize::try_from(bound - 1).unwrap(),
                Err(Busy),
                false,
                true,
                (1, 1),
                true
            ),
            "rule: client attempts and peer messages pass one inbound bound of \
             controller_queue_inputs ({bound}) end to end, counting the attempt the busy \
             controller holds, so the hand-off takes {bound} - 1 more (br-rl0u); past that a \
             client attempt is refused busy (not submitted) and a peer message dropped, both \
             counted once, and every attempt handed over is still answered; once the \
             controller has judged what it held, the bound frees again (§7.11, §7.12)"
        );
    }
}

mod serve_tests {
    //! The client API over HTTP (§8, §7.12 "Mapping", O-1; W18.3): a real node behind the
    //! std-only HTTP/1.1 server on loopback, driven with raw requests.

    // check: SERVER-HTTP

    use std::io::{self, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::{Value as Json, json};
    use server::init::init;
    use server::run::prepare;
    use server::runtime::{Node, Options, start};
    use server::serve::{
        ACCEPT_BACKOFF_MAX, ACCEPT_BACKOFF_MIN, Accept, HALT_EXIT_STATUS, Responders, Serving,
        accept_backoff, exit_on_halt, serve, serve_on,
    };
    use types::Config;

    use crate::support::{
        INSTANCE, Reply, free_port, instance, node, one_voter, put_body, read_reply, request,
        scratch, send,
    };

    /// A one-voter config with a 5 ms tick whose client address is a free loopback port.
    fn config(dir: &Path) -> (Config, SocketAddr) {
        let port = free_port();
        let config = one_voter(dir, |text| {
            text.replace("tick_ms = 50", "tick_ms = 5")
                .replace("127.0.0.1:7101", &format!("127.0.0.1:{port}"))
        })
        .1;
        (config, SocketAddr::from(([127, 0, 0, 1], port)))
    }

    /// An initialised node, started and served, once it leads.
    fn served(dir: &Path) -> (Config, Serving) {
        let (config, address) = config(dir);
        init(&config, node(1), instance()).unwrap();
        let prepared = prepare(&config, node(1)).unwrap();
        let responders = Responders::new();
        let running = start(
            prepared,
            &config,
            node(1),
            responders.outputs(),
            &Options {
                log_file: dir.join("node.log"),
            },
        )
        .unwrap();
        let listener = TcpListener::bind(address).unwrap();
        let serving = serve(listener, Arc::new(running), responders, &config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let cluster = send(address, &request("GET", "/v1/cluster", &[], ""));
            if cluster.body["role"] == "leader" && cluster.body["applied_index"].as_u64() > Some(0)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture: the node leads within 10 s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        (config, serving)
    }

    /// Stops serving and the node.
    fn stop(serving: Serving) {
        let running = serving.stop(Duration::from_secs(5)).unwrap();
        if let Ok(running) = Arc::try_unwrap(running) {
            running.shutdown().unwrap();
        }
    }

    fn put(address: SocketAddr, target: &str, id: &str, body: &str) -> Reply {
        send(
            address,
            &request(
                "PUT",
                target,
                &[
                    ("Idempotency-Key", id),
                    ("Content-Type", "application/json"),
                ],
                body,
            ),
        )
    }

    fn get(address: SocketAddr, target: &str) -> Reply {
        send(address, &request("GET", target, &[], ""))
    }

    const OTHER_ID: &str = "ffeeddccbbaa99887766554433221100";

    #[test]
    fn a_put_and_a_get_round_trip_over_http_in_the_spec_s_shapes() {
        let dir = scratch("serve-kv");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let first = put(address, "/v1/kv?key=a%2Fb%20c", INSTANCE, &put_body("v1"));
        let index = first.body["index"].clone();
        let replay = put(address, "/v1/kv?key=a%2Fb%20c", INSTANCE, &put_body("v1"));
        let misuse = put(address, "/v1/kv?key=a%2Fb%20c", INSTANCE, &put_body("v2"));
        let read = get(address, "/v1/kv?key=a%2Fb%20c");
        let absent = get(address, "/v1/kv?key=nothing");
        let applied = read.body["served_by"]["applied_index"].clone();
        let replies = [&first, &replay, &misuse, &read, &absent];
        let no_store = replies
            .iter()
            .all(|reply| reply.header("cache-control") == vec!["no-store"]);
        stop(serving);
        assert_eq!(
            (
                (first.status, first.body.clone()),
                (replay.status, replay.body["replayed"].clone()),
                (
                    misuse.status,
                    misuse.code(),
                    misuse.body["error"]["details"].clone()
                ),
                (read.status, read.body.clone()),
                (absent.status, absent.code()),
                no_store,
            ),
            (
                (
                    200,
                    json!({"request_id": INSTANCE, "index": index, "version": index,
                    "replayed": false,
                    "served_by": {"node_id": 1, "applied_index": first.body["served_by"]["applied_index"]}})
                ),
                (200, json!(true)),
                (
                    409,
                    Some("request_id_conflict"),
                    json!({"request_id": INSTANCE, "original_index": index})
                ),
                (
                    200,
                    json!({"key": "a/b c", "value": "v1", "version": index,
                    "served_by": {"node_id": 1, "applied_index": applied}})
                ),
                (404, Some("not_found")),
                true,
            ),
            "rule: a PUT over HTTP is answered once applied with request_id, index, version == \
             index and served_by; a retry replays it and another command under its ID is \
             request_id_conflict; a GET reads the decoded key; every response carries \
             Cache-Control: no-store (F-1, F-2, F-6, §8.1, §8.3)"
        );
    }

    fn id(n: u32) -> String {
        format!("{n:032x}")
    }

    fn post_json(address: SocketAddr, target: &str, id: &str, body: &str) -> Reply {
        send(
            address,
            &request(
                "POST",
                target,
                &[
                    ("Idempotency-Key", id),
                    ("Content-Type", "application/json"),
                ],
                body,
            ),
        )
    }

    fn delete(address: SocketAddr, target: &str, id: &str) -> Reply {
        send(
            address,
            &request("DELETE", target, &[("Idempotency-Key", id)], ""),
        )
    }

    fn no_store(replies: &[&Reply]) -> bool {
        replies
            .iter()
            .all(|reply| reply.header("cache-control") == vec!["no-store"])
    }

    #[test]
    fn a_scan_pages_through_the_keys_over_http_in_the_spec_s_shapes() {
        let dir = scratch("serve-scan");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let put_a = put(address, "/v1/kv?key=a", &id(1), &put_body("1"));
        let put_b = put(address, "/v1/kv?key=b", &id(2), &put_body("2"));
        let page = get(address, "/v1/scan");
        let first = get(address, "/v1/scan?limit=1");
        let second = get(address, "/v1/scan?limit=1&after=a");
        let empty = get(address, "/v1/scan?start=m&end=m");
        let inverted = get(address, "/v1/scan?start=b&end=a");
        let stored = no_store(&[&page, &first, &second, &empty, &inverted]);
        stop(serving);
        let keys = |reply: &Reply| {
            reply.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["key"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let shape = |reply: &Reply| {
            (
                reply.status,
                keys(reply),
                reply.body["has_more"].clone(),
                reply.body["next_after"].clone(),
            )
        };
        assert_eq!(
            (
                (put_a.status, put_b.status),
                shape(&page),
                shape(&first),
                shape(&second),
                shape(&empty),
                (inverted.status, inverted.code()),
                page.body["items"][1].clone(),
                stored,
            ),
            (
                (200, 200),
                (
                    200,
                    vec!["a".to_owned(), "b".to_owned()],
                    json!(false),
                    Json::Null
                ),
                (200, vec!["a".to_owned()], json!(true), json!("a")),
                (200, vec!["b".to_owned()], json!(false), Json::Null),
                (200, Vec::new(), json!(false), Json::Null),
                (400, Some("invalid_request")),
                json!({"key": "b", "value": "2", "version": put_b.body["version"]}),
                true,
            ),
            "rule: SCAN lists key, value and version in key order; a page cut by limit says \
             has_more with next_after the last key and `after` continues past it; equal \
             bounds are an empty page and start > end is invalid_request; every response is \
             no-store (F-4, §8.5)"
        );
    }

    #[test]
    fn a_cas_sets_or_refuses_terminally_over_http_in_the_spec_s_shapes() {
        let dir = scratch("serve-cas");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let put_a = put(address, "/v1/kv?key=a", &id(1), &put_body("1"));
        let version_a = put_a.body["version"].as_u64().unwrap();
        let matching =
            format!(r#"{{"expected":{{"kind":"version","version":{version_a}}},"value":"3"}}"#);
        let stale =
            format!(r#"{{"expected":{{"kind":"version","version":{version_a}}},"value":"4"}}"#);
        let cas_ok = post_json(address, "/v1/cas?key=a", &id(3), &matching);
        let cas_stale = post_json(address, "/v1/cas?key=a", &id(4), &stale);
        let cas_stale_again = post_json(address, "/v1/cas?key=a", &id(4), &stale);
        let create_only = post_json(
            address,
            "/v1/cas?key=c",
            &id(5),
            r#"{"expected":{"kind":"absent"},"value":"new"}"#,
        );
        let too_big = post_json(
            address,
            "/v1/cas?key=a",
            &id(6),
            r#"{"expected":{"kind":"version","version":9007199254740992},"value":"x"}"#,
        );
        let stored = no_store(&[&cas_ok, &cas_stale, &create_only, &too_big]);
        stop(serving);
        let cas_index = cas_ok.body["index"].clone();
        let stale_index = cas_stale.body["error"]["details"]["index"].clone();
        assert_eq!(
            (
                (
                    cas_ok.status,
                    cas_ok.body["version"].clone(),
                    cas_ok.body["replayed"].clone()
                ),
                (
                    cas_stale.status,
                    cas_stale.code(),
                    cas_stale.body["error"]["details"].clone()
                ),
                (
                    cas_stale_again.status,
                    cas_stale_again.body["error"]["details"]["replayed"].clone()
                ),
                (
                    create_only.status,
                    create_only.body["version"] == create_only.body["index"]
                ),
                (
                    too_big.status,
                    too_big.code(),
                    too_big.body["error"]["details"].clone()
                ),
                stored,
            ),
            (
                (200, cas_index.clone(), json!(false)),
                (
                    409,
                    Some("version_mismatch"),
                    json!({"current_version": cas_index, "request_id": id(4),
                        "index": stale_index, "replayed": false})
                ),
                (409, json!(true)),
                (200, true),
                (400, Some("invalid_request"), json!({"request_id": id(6)})),
                true,
            ),
            "rule: a CAS that matches sets with version == index; one that does not is the \
             terminal 409 version_mismatch naming the current version and the original index, \
             replayed on a retry; an absent expectation creates; an expected version of 2^53 \
             is invalid_request (F-5, F-6, §8.3)"
        );
    }

    #[test]
    fn a_delete_and_the_status_lookup_round_trip_over_http_in_the_spec_s_shapes() {
        let dir = scratch("serve-delete-status");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let put_a = put(address, "/v1/kv?key=a", &id(1), &put_body("1"));
        let version_a = put_a.body["version"].as_u64().unwrap();
        let stale = format!(
            r#"{{"expected":{{"kind":"version","version":{}}},"value":"4"}}"#,
            version_a + 1
        );
        let cas_stale = post_json(address, "/v1/cas?key=a", &id(4), &stale);
        let removed = delete(address, "/v1/kv?key=a", &id(7));
        let removed_again = delete(address, "/v1/kv?key=a", &id(8));
        let removed_replay = delete(address, "/v1/kv?key=a", &id(7));
        let reused = delete(address, "/v1/kv?key=b", &id(7));
        let gone = get(address, "/v1/kv?key=a");
        let status_stale = get(address, &format!("/v1/requests/{}", id(4)));
        let status_put = get(address, &format!("/v1/requests/{}", id(1)));
        let status_removed = get(address, &format!("/v1/requests/{}", id(8)));
        let status_unknown = get(address, &format!("/v1/requests/{}", id(99)));
        let stored = no_store(&[&removed, &status_stale, &status_unknown]);
        stop(serving);
        assert_eq!(
            (
                (
                    removed.status,
                    removed.body["existed"].clone(),
                    removed.body["replayed"].clone()
                ),
                (removed_again.status, removed_again.body["existed"].clone()),
                (
                    removed_replay.status,
                    removed_replay.body["replayed"].clone()
                ),
                (reused.status, reused.code()),
                (gone.status, gone.code()),
                stored,
            ),
            (
                (200, json!(true), json!(false)),
                (200, json!(false)),
                (200, json!(true)),
                (409, Some("request_id_conflict")),
                (404, Some("not_found")),
                true,
            ),
            "rule: DELETE reports existed, replays on a retry, deleting an absent key succeeds \
             with existed false, and a reused ID for another command is request_id_conflict \
             (F-3, F-6, §8.3)"
        );
        assert_eq!(
            (
                (status_stale.status, status_stale.body["state"].clone()),
                status_stale.body["result"]["error"]["code"].clone(),
                status_stale.body["result"]["error"]["details"].clone(),
                (
                    status_put.status,
                    status_put.body["result"]["version"].clone()
                ),
                status_removed.body["result"]["existed"].clone(),
                (status_unknown.status, status_unknown.body.clone()),
            ),
            (
                (200, json!("known")),
                json!("version_mismatch"),
                json!({"current_version": version_a, "request_id": id(4),
                    "index": cas_stale.body["error"]["details"]["index"], "replayed": true}),
                (200, put_a.body["version"].clone()),
                json!(false),
                (
                    200,
                    json!({"state": "unknown", "served_by": status_unknown.body["served_by"]})
                ),
            ),
            "rule: the status lookup is 200 with the original terminal result and replayed true \
             whatever it was, and `unknown` for an ID this node never applied (F-9, §8.3)"
        );
    }

    #[test]
    fn health_and_cluster_report_the_running_node() {
        let dir = scratch("serve-health");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let live = get(address, "/v1/health/live");
        let ready = get(address, "/v1/health/ready");
        let cluster = get(address, "/v1/cluster");
        stop(serving);
        let peers = cluster.body["peers"].clone();
        assert_eq!(
            (
                (live.status, live.body),
                (ready.status, ready.body),
                cluster.status,
                cluster.body["role"].clone(),
                cluster.body["leader_id"].clone(),
                peers[0]["client_address"].clone(),
                peers[0]["match_index"].as_u64() >= cluster.body["commit_index"].as_u64(),
                cluster.body["durable_last_index"].as_u64()
                    >= cluster.body["commit_index"].as_u64(),
            ),
            (
                (
                    200,
                    json!({"status": "ok", "node_id": 1, "lifecycle": "ready"})
                ),
                (
                    200,
                    json!({"status": "ok", "node_id": 1, "lifecycle": "ready"})
                ),
                200,
                json!("leader"),
                json!(1),
                json!(format!("http://{address}")),
                true,
                true,
            ),
            "rule: health live and ready report the ready node, and /v1/cluster its role, \
             term, leader and indices with every voter's client URL and match index (O-1)"
        );
    }

    #[test]
    fn an_oversized_body_is_answered_413_and_the_client_can_read_it() {
        use std::io::{Read, Write};
        let dir = scratch("serve-too-large");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let body = vec![b'x'; 3 * 1024 * 1024];
        let head = format!(
            "PUT /v1/kv?key=big HTTP/1.1\r\nHost: x\r\nIdempotency-Key: {INSTANCE}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut outcomes = Vec::new();
        for attempt in 0..20 {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let sent = stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(&body));
            let mut reply = Vec::new();
            let read = stream.read_to_end(&mut reply);
            let text = String::from_utf8_lossy(&reply).into_owned();
            if !(read.is_ok() && text.starts_with("HTTP/1.1 413") && text.contains("too_large")) {
                outcomes.push(format!(
                    "attempt {attempt}: send={sent:?} read={read:?} reply={:?}",
                    &text[..text.len().min(80)]
                ));
            }
        }
        stop(serving);
        assert!(
            outcomes.is_empty(),
            "rule: a body over 512 KiB is refused with a readable 413 too_large, never a reset \
             connection (§7.11, §8.4); failures={outcomes:#?}"
        );
    }

    #[test]
    fn a_closing_connection_is_not_held_open_by_a_client_that_trickles_bytes() {
        use std::io::{Read, Write};
        let dir = scratch("serve-trickle");
        let (_, serving) = served(&dir);
        let mut stream = std::net::TcpStream::connect(serving.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"GARBAGE\r\n\r\n").unwrap();
        let start = std::time::Instant::now();
        let mut reply = Vec::new();
        // Keep trickling a byte at a time while reading the reply to its end.
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..16 {
                std::thread::sleep(Duration::from_millis(500));
                if writer.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let read = stream.read_to_end(&mut reply);
        let waited = start.elapsed();
        trickle.join().unwrap();
        stop(serving);
        assert!(
            read.is_ok() && String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 400"),
            "rule: a malformed request is answered 400 and read to its end; read={read:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "rule: the close drain is bounded in total time (CD-9); waited={waited:?}"
        );
    }

    #[test]
    fn malformed_requests_are_refused_over_the_wire_and_a_connection_is_kept_alive() {
        let dir = scratch("serve-strict");
        let (_, serving) = served(&dir);
        let address = serving.address();
        let unknown_field = put(address, "/v1/kv?key=a", INSTANCE, r#"{"value":"v","x":1}"#);
        let plus = put(address, "/v1/kv?key=a+b", INSTANCE, &put_body("v"));
        let no_id = send(
            address,
            &request(
                "PUT",
                "/v1/kv?key=a",
                &[("Content-Type", "application/json")],
                &put_body("v"),
            ),
        );
        let delete = send(address, &request("POST", "/v1/kv?key=a", &[], ""));
        let bare_lf = send(address, b"GET /v1/kv?key=a HTTP/1.1\nHost: x\n\n");
        // Two requests on one connection, the first kept alive.
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(b"GET /v1/health/live HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let first = read_reply(&mut reader);
        stream
            .write_all(&request(
                "PUT",
                "/v1/kv?key=kept",
                &[
                    ("Idempotency-Key", OTHER_ID),
                    ("Content-Type", "application/json"),
                ],
                &put_body("alive"),
            ))
            .unwrap();
        let second = read_reply(&mut reader);
        // The second request asked to close: the server closes after answering it.
        let mut rest = Vec::new();
        let closed = std::io::Read::read_to_end(&mut reader, &mut rest).is_ok() && rest.is_empty();
        drop(reader);
        drop(stream);
        stop(serving);
        let shape = |reply: &Reply| {
            (
                reply.status,
                reply.code().map(str::to_owned),
                reply.body["error"]["details"]["request_id"].clone(),
                reply.header("cache-control") == vec!["no-store"],
            )
        };
        assert_eq!(
            (
                [&unknown_field, &plus, &no_id, &delete, &bare_lf].map(shape),
                delete.header("allow"),
                bare_lf.header("connection"),
                (
                    first.status,
                    second.status,
                    second.header("connection"),
                    closed
                ),
            ),
            (
                [
                    (
                        400,
                        Some("invalid_request".to_owned()),
                        json!(INSTANCE),
                        true
                    ),
                    (
                        400,
                        Some("invalid_request".to_owned()),
                        json!(INSTANCE),
                        true
                    ),
                    (400, Some("invalid_request".to_owned()), Json::Null, true),
                    (405, Some("invalid_request".to_owned()), Json::Null, true),
                    (400, Some("invalid_request".to_owned()), Json::Null, true),
                ],
                vec!["DELETE, GET, PUT"],
                vec!["close"],
                (200, 200, vec!["close"], true),
            ),
            "rule: over the wire, an unknown field and an unescaped '+' are 400 with the \
             request ID, a write without an Idempotency-Key is 400 without one, a POST to \
             /v1/kv is 405 naming DELETE, GET and PUT, malformed HTTP is 400 and closes the connection, every refusal is \
             no-store, and a kept-alive connection serves the next request (§8.1, §8.4)"
        );
    }

    /// An initialised, started node that is not yet served.
    fn started_node(dir: &Path) -> (Config, Arc<Node>, Arc<Responders>) {
        let (config, _) = config(dir);
        init(&config, node(1), instance()).unwrap();
        let prepared = prepare(&config, node(1)).unwrap();
        let responders = Responders::new();
        let running = start(
            prepared,
            &config,
            node(1),
            responders.outputs(),
            &Options {
                log_file: dir.join("node.log"),
            },
        )
        .unwrap();
        (config, Arc::new(running), responders)
    }

    /// A connection source whose `accept()` fails every time, counting its calls.
    struct Failing(Arc<AtomicU64>);

    impl Accept for Failing {
        fn accept(&self) -> io::Result<TcpStream> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("fixture: persistent accept error"))
        }
    }

    /// A connection source whose `accept()` panics.
    struct Panicking;

    impl Accept for Panicking {
        fn accept(&self) -> io::Result<TcpStream> {
            panic!("fixture: accept panics");
        }
    }

    #[test]
    fn accept_backoff_doubles_to_its_cap() {
        let pauses: Vec<Duration> = [1, 2, 3, 7, 8, 100, u32::MAX]
            .into_iter()
            .map(accept_backoff)
            .collect();
        assert_eq!(
            pauses,
            vec![
                ACCEPT_BACKOFF_MIN,
                ACCEPT_BACKOFF_MIN * 2,
                ACCEPT_BACKOFF_MIN * 4,
                ACCEPT_BACKOFF_MIN * 64,
                ACCEPT_BACKOFF_MAX,
                ACCEPT_BACKOFF_MAX,
                ACCEPT_BACKOFF_MAX,
            ],
            "rule: the pause after n failures in a row doubles from the minimum and never \
             exceeds the cap (br-4ova, CD-9); failures 1,2,3,7,8,100,max"
        );
        assert_eq!(
            accept_backoff(0),
            ACCEPT_BACKOFF_MIN,
            "rule: even a zero count (never used) yields the minimum pause"
        );
    }

    #[test]
    fn persistent_accept_error_backs_off_instead_of_spinning() {
        let dir = scratch("accept-error-backoff");
        let (config, running, responders) = started_node(&dir);
        let calls = Arc::new(AtomicU64::new(0));
        let address = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let serving = serve_on(
            Failing(Arc::clone(&calls)),
            address,
            running,
            responders,
            &config,
        )
        .unwrap();
        // Wait for the 8th failure: the loop is then in a pause of 640 ms or more (1 s cap).
        let deadline = Instant::now() + Duration::from_secs(10);
        while serving.accept_errors() < 8 {
            assert!(
                Instant::now() < deadline,
                "fixture: eight accept errors within 10 s; counted={}",
                serving.accept_errors()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let (counted, made) = (serving.accept_errors(), calls.load(Ordering::SeqCst));
        let stopped = Instant::now();
        let running = serving.stop(Duration::from_secs(5)).unwrap();
        let stop_took = stopped.elapsed();
        if let Ok(running) = Arc::try_unwrap(running) {
            running.shutdown().unwrap();
        }
        // Eight failures take about 1.3 s of pauses (10+20+...+640 ms); a spin makes millions.
        assert!(
            (8..=10).contains(&made) && counted <= made && made <= counted + 1,
            "rule: a persistent accept() error is retried after growing pauses and counted, \
             not spun on (br-4ova); calls={made} counted={counted}"
        );
        assert!(
            stop_took < Duration::from_millis(300),
            "rule: stop interrupts a backing-off accept loop within a slice, not the 640 ms+ \
             pause it is in (br-4ova); took {stop_took:?}"
        );
    }

    #[test]
    fn accept_panic_halts_the_node_and_the_process_exits_with_status_1() {
        let dir = scratch("accept-panic-halt");
        let (config, running, responders) = started_node(&dir);
        let address = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let serving = serve_on(
            Panicking,
            address,
            Arc::clone(&running),
            responders,
            &config,
        )
        .unwrap();
        let mut report = Vec::new();
        let status = exit_on_halt(&running, node(1), &mut report);
        let report = String::from_utf8(report).unwrap();
        drop(serving);
        if let Ok(running) = Arc::try_unwrap(running) {
            // The halted node's own shutdown may report the halt: only the exit status counts.
            let _ = running.shutdown();
        }
        assert!(
            status == HALT_EXIT_STATUS
                && HALT_EXIT_STATUS == 1
                && report.contains("keyval-accept")
                && report.contains("fixture: accept panics"),
            "rule: a panic on the accept thread halts the node and the process exits with \
             status 1, naming the thread (br-4ova, br-7tjt); status={status} report={report:?}"
        );
    }
}
