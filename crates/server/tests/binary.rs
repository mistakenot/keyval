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
    use crate::support::{INSTANCE, data_dir, genesis_frame, journal_bytes, one_voter, scratch};

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
    fn run_on_an_initialised_directory_says_it_does_not_serve_yet() {
        let dir = scratch("cli-run-ready");
        let (path, config) = one_voter(&dir, |text| text);
        let initialised = init(&path, "1", INSTANCE, &[]);
        let ran = run(&path, "1");
        let envelope = ran.json();
        assert_eq!(
            (
                initialised.code,
                ran.error_code(),
                envelope["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("W18.2")),
                journal_bytes(&data_dir(&config, 1)) == Some(genesis_frame(&config, 1)),
            ),
            (Some(0), (Some(1), "not_serving".to_owned()), true, true),
            "rule: an initialised directory passes run's preflight, and until W18.2 wires the \
             node run says it does not serve rather than pretend to (P6), changing nothing"
        );
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
