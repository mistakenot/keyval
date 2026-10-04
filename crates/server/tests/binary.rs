//! keyval-server's binary (PLAN.md §8.7, O-6; W18.1): exit codes, messages and the `--json`
//! output, checked against `schemas/server/`; and a real single node killed with `SIGKILL`
//! and restarted through PID-receipt helpers (W18.4, DUR-3). This target only runs the binary: it never
//! takes a data-directory lock itself, so no child it spawns can inherit one (br-o8co,
//! br-b7xl); the library tests, which do take locks, never spawn a process.
//!
//! The restart tests are process-crash evidence: `SIGKILL` keeps the OS page cache, so they
//! cannot detect a missing fsync (D-46); the sync-order check is W24's.
#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

#[path = "support/binary.rs"]
mod harness;

#[path = "support/node.rs"]
mod node;

// check: SERVER-RESTART

mod cli_tests {
    //! `keyval-server init` and `run` as an operator meets them: exit codes, messages, and the
    //! `--json` output (§8.7, O-6, D-52).

    use crate::harness::{Ran, assert_init_schema, init, keyval_server, run};
    use std::net::{SocketAddr, TcpStream};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::support::{
        INSTANCE, data_dir, free_port, genesis_frame, journal_bytes, one_voter, put_body, request,
        scratch, send,
    };

    #[test]
    fn init_prints_its_report_and_exits_0_then_refuses_with_exit_1_or_2() {
        let dir = scratch("cli-init");
        let (path, config) = one_voter(&dir, |text| text);
        let ok = init(&path, "1", INSTANCE, &["--json"]);
        let report = ok.json();
        assert_init_schema(&report);
        let again = init(&path, "1", INSTANCE, &["--json"]);
        let bad_instance = init(&path, "1", "xyz", &["--json"]);
        let no_config = init(&dir.join("absent.toml"), "1", INSTANCE, &["--json"]);
        let unknown = init(&path, "9", INSTANCE, &["--json"]);
        let human = init(&path, "1", INSTANCE, &[]);
        assert_eq!(
            (
                ok.code,
                report["data_dir"].as_str().map(str::to_owned),
                report["directory"].as_str(),
                report["instance_id"].as_str(),
                report["journal_bytes"].as_u64(),
                [&again, &bad_instance, &no_config, &unknown].map(Ran::error_code),
                (human.code, human.stderr.contains("initialised already")),
            ),
            (
                Some(0),
                Some(data_dir(&config, 1).display().to_string()),
                Some("created"),
                Some(INSTANCE),
                Some(u64::try_from(genesis_frame(&config, 1).len()).unwrap()),
                [
                    (Some(1), "already_initialised".to_owned()),
                    (Some(2), "usage".to_owned()),
                    (Some(2), "config".to_owned()),
                    (Some(2), "unknown_node".to_owned()),
                ],
                (Some(1), true),
            ),
            "rule: init prints keyval.server.init/v1 and exits 0, creating the data directory \
             beside the config file; a refusal because of the data directory exits 1, a usage \
             or configuration error 2, each as keyval.server.error/v1 under --json and a message \
             on stderr otherwise (O-6, §8.7, §7.7 step 0)"
        );
    }

    #[test]
    fn run_refuses_a_missing_or_uninitialised_data_directory_with_exit_1() {
        let dir = scratch("cli-run-missing");
        let (path, config) = one_voter(&dir, |text| text);
        let data = data_dir(&config, 1);
        let missing = run(&path, "1");
        let missing_envelope = missing.json();
        let missing_code = missing.error_code();
        std::fs::create_dir(&data).unwrap();
        let uninitialised = run(&path, "1").error_code();
        assert_eq!(
            (
                missing_code,
                missing_envelope["data_dir"].as_str().map(str::to_owned),
                uninitialised,
                journal_bytes(&data)
            ),
            (
                (Some(1), "missing_data_dir".to_owned()),
                Some(data.display().to_string()),
                (Some(1), "uninitialised".to_owned()),
                None
            ),
            "rule: run refuses a missing data directory and one without a journal, exit 1, and \
             creates neither the directory nor a journal: only init initialises (D-52)"
        );
    }

    #[test]
    fn run_serves_an_initialised_directory_over_http_until_killed() {
        let dir = scratch("cli-run-serves");
        let port = free_port();
        let (path, config) = one_voter(&dir, |text| {
            text.replace("tick_ms = 50", "tick_ms = 5")
                .replace("127.0.0.1:7101", &format!("127.0.0.1:{port}"))
        });
        let initialised = init(&path, "1", INSTANCE, &[]);
        let log = dir.join("node.log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_keyval-server"))
            .args([
                "run",
                "--config",
                path.to_str().unwrap(),
                "--node",
                "1",
                "--log-file",
            ])
            .arg(&log)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(20);
        let ready = loop {
            if let Ok(stream) = TcpStream::connect(address) {
                drop(stream);
                let ready = send(address, &request("GET", "/v1/health/ready", &[], ""));
                let leads = send(address, &request("GET", "/v1/cluster", &[], ""));
                if ready.status == 200 && leads.body["role"] == "leader" {
                    break ready;
                }
            }
            assert!(Instant::now() < deadline, "fixture: run serves within 20 s");
            std::thread::sleep(Duration::from_millis(20));
        };
        let written = send(
            address,
            &request(
                "PUT",
                "/v1/kv?key=k",
                &[
                    ("Idempotency-Key", INSTANCE),
                    ("Content-Type", "application/json"),
                ],
                &put_body("v"),
            ),
        );
        let read = send(address, &request("GET", "/v1/kv?key=k", &[], ""));
        let missing = send(address, &request("GET", "/v1/kv?key=other", &[], ""));
        // The log is written by its own thread: wait for the election's record before the kill.
        let elected = serde_json::json!({"role": "leader", "leader": 1});
        let deadline = Instant::now() + Duration::from_secs(10);
        let events = loop {
            let events = logged_events(&log);
            if events
                .iter()
                .any(|(event, fields)| event == "role_changed" && *fields == elected)
                || Instant::now() >= deadline
            {
                break events;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        let logged = (
            events.iter().any(|(event, _)| event == "boot"),
            events
                .iter()
                .any(|(event, fields)| event == "role_changed" && *fields == elected),
        );
        assert_eq!(
            (
                initialised.code,
                ready.header("cache-control"),
                written.status,
                (read.status, read.body["value"].clone()),
                missing.code(),
                logged,
                journal_bytes(&data_dir(&config, 1)).map_or(0, |bytes| bytes.len())
                    > genesis_frame(&config, 1).len(),
            ),
            (
                Some(0),
                vec!["no-store"],
                200,
                (200, serde_json::json!("v")),
                Some("not_found"),
                (true, true),
                true,
            ),
            "rule: run starts the node from its initialised directory and serves the client \
             API on its client address (§7.12): ready, a PUT and a GET over HTTP, with the \
             boot and its own election (a role_changed record, O-3) in its --log-file and the \
             write in its journal; logged events {events:?}"
        );
    }

    /// Each JSON line of the log at `path`: its event and its role and leader fields.
    fn logged_events(path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .map(|record| {
                (
                    record["event"].as_str().unwrap_or_default().to_owned(),
                    serde_json::json!({"role": record["role"], "leader": record["leader"]}),
                )
            })
            .collect()
    }

    #[test]
    fn run_reports_usage_and_configuration_errors_with_exit_2() {
        let dir = scratch("cli-run-usage");
        let (path, _) = one_voter(&dir, |text| text);
        let unknown = run(&path, "9");
        let no_config = run(&dir.join("absent.toml"), "1");
        let no_node = keyval_server(&["run", "--config", path.to_str().unwrap(), "--json"]);
        let commands: Vec<String> = [&unknown, &no_config, &no_node]
            .iter()
            .map(|ran| ran.json()["command"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            (
                [&unknown, &no_config, &no_node].map(Ran::error_code),
                commands
            ),
            (
                [
                    (Some(2), "unknown_node".to_owned()),
                    (Some(2), "config".to_owned()),
                    (Some(2), "usage".to_owned()),
                ],
                vec!["run".to_owned(); 3]
            ),
            "rule: an unknown node, an unreadable config and a missing argument are usage or \
             configuration errors of run: exit 2, as keyval.server.error/v1 under --json (O-6)"
        );
    }
}

mod restart_tests {
    //! Start, write over HTTP, `SIGKILL` the PID its receipt names, restart, read back.

    use std::fs;
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::harness::init;
    use crate::node::{Node, Refusal, Spec, killed, try_send};
    use crate::support::{INSTANCE, Reply, free_port, one_voter, put_body, request, scratch, send};

    /// An initialised one-voter node, started with its receipt in a fresh scratch dir.
    fn started(name: &str) -> Node {
        let dir = scratch(name);
        let port = free_port();
        let (path, _) = one_voter(&dir, |text| {
            text.replace("tick_ms = 50", "tick_ms = 5")
                .replace("127.0.0.1:7101", &format!("127.0.0.1:{port}"))
        });
        let initialised = init(&path, "1", INSTANCE, &[]);
        assert_eq!(
            initialised.code,
            Some(0),
            "fixture: init succeeds; stderr: {}",
            initialised.stderr
        );
        Node::start(Spec {
            config: path,
            node: 1,
            address: SocketAddr::from(([127, 0, 0, 1], port)),
            log: dir.join("node.log"),
            dir,
        })
    }

    /// The `n`th request ID of writer `writer`: 32 lowercase hex characters.
    fn request_id(writer: u64, n: u64) -> String {
        format!("{writer:08x}{n:024x}")
    }

    fn put_request(id: &str, key: &str, value: &str) -> Vec<u8> {
        request(
            "PUT",
            &format!("/v1/kv?key={key}"),
            &[
                ("Idempotency-Key", id),
                ("Content-Type", "application/json"),
            ],
            &put_body(value),
        )
    }

    fn put(address: SocketAddr, id: &str, key: &str, value: &str) -> Reply {
        send(address, &put_request(id, key, value))
    }

    fn get(address: SocketAddr, key: &str) -> Reply {
        send(
            address,
            &request("GET", &format!("/v1/kv?key={key}"), &[], ""),
        )
    }

    /// Every record of the node's log, in order.
    fn log_records(node: &Node) -> Vec<Value> {
        fs::read_to_string(&node.spec.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The incarnation of each boot; the last boot's lifecycle changes, up to ready; the index
    /// its startup replay applied (0 if none); and the highest index it has applied since.
    fn last_boot(records: &[Value]) -> (Vec<Value>, Vec<String>, u64, u64) {
        let boots: Vec<Value> = records
            .iter()
            .filter(|record| record["event"] == "boot")
            .map(|record| record["incarnation"].clone())
            .collect();
        let last = records
            .iter()
            .rposition(|record| record["event"] == "boot")
            .unwrap_or(records.len());
        let since = &records[last..];
        let mut lifecycle = Vec::new();
        let (mut replayed, mut applied) = (0, 0);
        for record in since {
            match record["event"].as_str() {
                Some("lifecycle_changed")
                    if !lifecycle.contains(&"recovering→ready".to_owned()) =>
                {
                    lifecycle.push(format!("{}→{}", record["from"], record["to"]).replace('"', ""));
                }
                Some("applied") => {
                    let index = record["index"].as_u64().unwrap_or(0);
                    if !lifecycle.contains(&"recovering→ready".to_owned()) {
                        replayed = index;
                    }
                    applied = applied.max(index);
                }
                _ => {}
            }
        }
        (boots, lifecycle, replayed, applied)
    }

    #[test]
    fn every_acknowledged_write_survives_a_sigkill_recovered_from_the_journal() {
        let mut node = started("restart-acked");
        node.wait_leading();
        let address = node.spec.address;
        let mut written: Vec<(String, String, String, Value)> = Vec::new();
        for n in 0..10 {
            let (key, value, id) = (format!("k{n}"), format!("v{n}"), request_id(1, n));
            let reply = put(address, &id, &key, &value);
            assert_eq!(
                reply.status, 200,
                "fixture: PUT {key} is acknowledged; {reply:?}"
            );
            written.push((key, id, value, reply.body["version"].clone()));
        }
        let overwrite = put(address, &request_id(1, 10), "k0", "v0-again");
        let last_version = overwrite.body["version"].as_u64().unwrap();
        written[0] = (
            "k0".to_owned(),
            request_id(1, 10),
            "v0-again".to_owned(),
            json!(last_version),
        );

        let receipt: Value =
            serde_json::from_str(&fs::read_to_string(node.receipt()).unwrap()).unwrap();
        let first_pid = node.pid();
        let status = node.kill().unwrap();
        let gone = TcpStream::connect(address).is_err() && !node.receipt().exists();
        node.restart();
        node.wait_leading();
        let cluster = node.wait_applied(last_version);
        let live = send(address, &request("GET", "/v1/health/live", &[], ""));
        // Before any new write: everything applied since the boot came from the journal.
        let (boots, lifecycle, replayed, applied) = last_boot(&log_records(&node));

        let reads: Vec<(String, Value, Value)> = written
            .iter()
            .map(|(key, ..)| {
                let reply = get(address, key);
                (
                    key.clone(),
                    reply.body["value"].clone(),
                    reply.body["version"].clone(),
                )
            })
            .collect();
        let expected: Vec<(String, Value, Value)> = written
            .iter()
            .map(|(key, _, value, version)| (key.clone(), json!(value), version.clone()))
            .collect();
        let (_, id3, value3, version3) = &written[3];
        let retried = put(address, id3, "k3", value3);
        let fresh = put(address, &request_id(1, 11), "k-new", "after");
        assert_eq!(
            (
                (receipt["pid"].clone(), killed(status), gone),
                reads,
                (
                    retried.status,
                    retried.body["replayed"].clone(),
                    retried.body["version"].clone()
                ),
                fresh.body["version"]
                    .as_u64()
                    .is_some_and(|version| version > last_version),
                (
                    live.status,
                    live.body["lifecycle"].clone(),
                    cluster.body["commit_index"]
                        .as_u64()
                        .is_some_and(|commit| commit >= last_version)
                ),
                (boots.len(), boots.first() != boots.last()),
                (lifecycle, replayed <= last_version, applied >= last_version),
            ),
            (
                (json!(first_pid), true, true),
                expected,
                (200, json!(true), version3.clone()),
                true,
                (200, json!("ready"), true),
                (2, true),
                (
                    vec![
                        "starting→recovering".to_owned(),
                        "recovering→ready".to_owned(),
                    ],
                    true,
                    true,
                ),
            ),
            "rule: a node SIGKILLed after acknowledging writes restarts from its data directory \
             without operator action and serves every acknowledged write with its version \
             (DUR-1, DUR-3), its state machine rebuilt from the recovered journal alone (the \
             new incarnation logs starting→recovering→ready and, before any new write, applies \
             up to the last acknowledged write: its startup replay up to the persisted commit, \
             §7.7 step 5, and the rest once its new term commits, step 7; O-3), ready in health \
             (O-1); a retry replays the original result (F-6); the kill signalled the \
             receipt's PID only (rule 18). Startup replay applied {replayed}, then {applied}; \
             stderr: {}",
            node.stderr()
        );
    }

    /// One write a burst attempted: its key, ID and value, and the answer, if one came.
    struct Attempt {
        key: String,
        id: String,
        value: String,
        answer: Result<Reply, String>,
    }

    /// Writer `writer`'s part of a burst: PUTs of fresh keys, one at a time, counting each
    /// acknowledgement in `acked`, until one gets no answer or any other status (its node was
    /// killed). Bounded at 2,000 writes.
    fn burst_writer(address: SocketAddr, writer: u64, acked: &AtomicUsize) -> Vec<Attempt> {
        let mut attempts = Vec::new();
        for n in 0..2_000 {
            let (key, value) = (format!("w{writer}-{n}"), format!("v{writer}-{n}"));
            let id = request_id(writer + 1, n);
            let answer = try_send(address, &put_request(&id, &key, &value));
            let stop = answer.as_ref().map_or(true, |reply| reply.status != 200);
            if !stop {
                acked.fetch_add(1, Ordering::SeqCst);
            }
            attempts.push(Attempt {
                key,
                id,
                value,
                answer,
            });
            if stop {
                break;
            }
        }
        attempts
    }

    #[test]
    fn a_sigkill_mid_burst_loses_no_acknowledged_write_and_executes_none_twice() {
        const WRITERS: u64 = 4;
        const ACKED_BEFORE_KILL: usize = 40;
        let mut node = started("restart-burst");
        node.wait_leading();
        let address = node.spec.address;
        let acked = Arc::new(AtomicUsize::new(0));
        // Every writer starts before the kill, and is joined only after it.
        let mut writers = Vec::new();
        for writer in 0..WRITERS {
            let acked = Arc::clone(&acked);
            writers.push(std::thread::spawn(move || {
                burst_writer(address, writer, &acked)
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while acked.load(Ordering::SeqCst) < ACKED_BEFORE_KILL {
            assert!(
                Instant::now() < deadline,
                "fixture: {ACKED_BEFORE_KILL} writes acknowledged within 20 s; stderr: {}",
                node.stderr()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let status = node.kill().unwrap();
        let mut attempts: Vec<Attempt> = Vec::new();
        for writer in writers {
            attempts.extend(writer.join().unwrap());
        }
        node.restart();
        node.wait_leading();
        // A restarted node may briefly serve an older prefix (C-4, C-7): read once it has
        // applied up to the last acknowledged write.
        let last_acknowledged = attempts
            .iter()
            .filter_map(|attempt| attempt.answer.as_ref().ok())
            .filter(|reply| reply.status == 200)
            .filter_map(|reply| reply.body["version"].as_u64())
            .max()
            .unwrap_or(0);
        node.wait_applied(last_acknowledged);

        let mut lost = Vec::new();
        let mut refused = Vec::new();
        let (mut acknowledged, mut replayed, mut executed) = (0, 0, 0);
        for attempt in &attempts {
            match &attempt.answer {
                Ok(reply) if reply.status == 200 => {
                    acknowledged += 1;
                    let read = get(address, &attempt.key);
                    if (&read.body["value"], &read.body["version"])
                        != (&json!(attempt.value), &reply.body["version"])
                    {
                        lost.push((attempt.key.clone(), reply.body.clone(), read.body));
                    }
                }
                Ok(reply) => refused.push((attempt.key.clone(), reply.status, reply.body.clone())),
                Err(_) => {
                    // Unanswered: it may or may not have executed. A retry with the same ID
                    // gets its original result or its first execution, never a second one.
                    let retry = put(address, &attempt.id, &attempt.key, &attempt.value);
                    let read = get(address, &attempt.key);
                    match (retry.status, retry.body["replayed"].as_bool()) {
                        (200, Some(true)) => replayed += 1,
                        (200, Some(false)) => executed += 1,
                        _ => refused.push((attempt.key.clone(), retry.status, retry.body)),
                    }
                    if read.body["value"] != json!(attempt.value) {
                        lost.push((attempt.key.clone(), Value::Null, read.body));
                    }
                }
            }
        }
        let unanswered = attempts
            .iter()
            .filter(|attempt| attempt.answer.is_err())
            .count();
        assert_eq!(
            (
                killed(status),
                acknowledged >= ACKED_BEFORE_KILL,
                lost,
                refused,
                (unanswered, replayed + executed),
            ),
            (
                true,
                true,
                vec![],
                vec![],
                (usize::try_from(WRITERS).unwrap(), unanswered)
            ),
            "rule: a SIGKILL during a burst of writes loses no acknowledged write (DUR-1, \
             DUR-3), and each write left unanswered resolves on retry with the same request \
             ID to its original result or a single first execution (F-6, A-5); \
             {acknowledged} acknowledged, {unanswered} unanswered ({replayed} replayed, \
             {executed} first executions); stderr: {}",
            node.stderr()
        );
    }

    #[test]
    fn kill_signals_only_the_pid_its_receipt_names() {
        let mut node = started("restart-receipt");
        node.wait_leading();
        let address = node.spec.address;
        let receipt = node.receipt();
        let original = fs::read_to_string(&receipt).unwrap();
        let mut forged: Value = serde_json::from_str(&original).unwrap();
        forged["pid"] = json!(u64::from(node.pid()) + 1);
        fs::write(&receipt, forged.to_string()).unwrap();
        let other = node.kill();
        let alive_after_other = send(address, &request("GET", "/v1/health/live", &[], "")).status;
        fs::remove_file(&receipt).unwrap();
        let missing = node.kill().map_err(|refusal| match refusal {
            Refusal::NoReceipt(_) => "no receipt",
            Refusal::NotRunning => "not running",
            Refusal::OtherPid { .. } => "other pid",
        });
        let alive_after_missing = send(address, &request("GET", "/v1/health/live", &[], "")).status;
        fs::write(&receipt, &original).unwrap();
        let pid = node.pid();
        let status = node.kill().unwrap();
        let again = node.kill();
        assert_eq!(
            (
                other,
                alive_after_other,
                missing.err(),
                alive_after_missing,
                killed(status),
                again
            ),
            (
                Err(Refusal::OtherPid {
                    receipt: u64::from(pid) + 1,
                    child: pid,
                }),
                200,
                Some("no receipt"),
                200,
                true,
                Err(Refusal::NotRunning),
            ),
            "rule: the kill helper signals only the PID its receipt names, and only while that \
             PID is the child it started and has not reaped; a forged or missing receipt \
             signals nothing (rule 18, O-2)"
        );
    }
}
