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
        // The controller answers attempt 1, then blocks until released: the hand-off fills.
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
        let accepted = (2..2 + bound)
            .filter(|n| {
                running
                    .client(AttemptId::new(*n), get(&config, "k"))
                    .is_ok()
            })
            .count();
        let refused = running.client(AttemptId::new(2 + bound), get(&config, "k"));
        let peer = running.peer(NodeId::new(2).unwrap(), vec![0; 8]);
        release.send(()).unwrap();
        let last = answer(&answers, 1 + bound);
        let counted = wait_for(&running, "the counts are published", |s| {
            s.clients_refused == 1 && s.peers_dropped == 1
        });
        running.shutdown().unwrap();
        assert_eq!(
            (
                accepted,
                refused,
                peer,
                matches!(last, Response::NotFound { .. }),
                (counted.clients_refused, counted.peers_dropped)
            ),
            (
                usize::try_from(bound).unwrap(),
                Err(Busy),
                false,
                true,
                (1, 1)
            ),
            "rule: while the controller is busy the hand-off takes controller_queue_inputs \
             ({bound}) client attempts and peer messages; past that a client attempt is \
             refused busy (not submitted) and a peer message dropped, both counted, and every \
             attempt handed over is still answered (§7.11, §7.12)"
        );
    }
}
