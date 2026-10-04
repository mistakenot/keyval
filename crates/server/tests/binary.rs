//! keyval-server's binary (PLAN.md §8.7, O-6; W18.1): exit codes, messages and the `--json`
//! output, checked against `schemas/server/`. This target only runs the binary: it never
//! takes a data-directory lock itself, so no child it spawns can inherit one (br-o8co,
//! br-b7xl); the library tests, which do take locks, never spawn a process.
#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

#[path = "support/binary.rs"]
mod harness;

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
