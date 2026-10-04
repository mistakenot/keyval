#![cfg(test)]
//! Check SIM-RUN (PLAN.md §12.2 W17): `./dev sim run --seed 42 --profile slice` gives
//! identical traces in two fresh processes, and its output labels itself a slice (P6).
//!
//! Each process is the `keyval-sim` binary that `./dev sim run` starts, at the fixed path
//! cargo builds it to, run through K-14's fresh-process harness
//! ([`sim::replay::fresh_processes`]), which compares the complete canonical bytes, never a
//! hash alone. The report is checked against its schema and against the trace it names:
//! the hash is recomputed here from FNV-1a's definition, not by `sim::trace`. A second seed
//! is the negative control: its trace must differ, or the comparison could not tell runs
//! apart.

mod sim_run {
    // check: SIM-RUN
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;
    use sim::entropy::MasterSeed;
    use sim::replay::{PROCESS_TIMEOUT, fresh_processes, trace_path};
    use sim::trace::{Comparison, compare};

    const SCHEMA: &str = include_str!("../../../schemas/dev/sim-run.v1.schema.json");

    /// The configuration `./dev sim run` passes.
    fn config() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/local.toml")
    }

    /// `<target>/tmp/sim-run/<case>`.
    fn out(case: &str) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("sim-run")
            .join(case)
    }

    /// `keyval-sim run --seed <seed> --profile slice --config <config> --trace <trace>`,
    /// with nothing from this process's environment.
    fn keyval_sim(seed: MasterSeed, trace: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_keyval-sim"));
        command
            .args([
                "run",
                "--seed",
                &seed.get().to_string(),
                "--profile",
                "slice",
            ])
            .arg("--config")
            .arg(config())
            .arg("--trace")
            .arg(trace)
            .env_clear();
        command
    }

    /// `keyval-sim` with `args` and nothing from this process's environment, run to its end.
    fn keyval_sim_output(args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_keyval-sim"))
            .args(args)
            .env_clear()
            .output()
            .unwrap()
    }

    /// Runs `seed` once in a fresh process: whether it exited 0, its report and its trace.
    fn run_once(seed: u64, case: &str) -> (bool, Value, String) {
        let dir = out(case);
        fs::create_dir_all(&dir).unwrap();
        let trace = dir.join(format!("seed-{seed}.trace"));
        if trace.exists() {
            fs::remove_file(&trace).unwrap();
        }
        let output = keyval_sim(MasterSeed::new(seed), &trace).output().unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "fixture: keyval-sim prints one JSON report ({err}); stdout {:?}, stderr {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (
            output.status.success(),
            report,
            fs::read_to_string(&trace).unwrap(),
        )
    }

    /// 64-bit FNV-1a over `bytes`, from its definition: offset basis 0xcbf29ce484222325,
    /// prime 0x100000001b3, xor each byte in, then multiply.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// The schema's errors for `report`.
    fn schema_errors(report: &Value) -> Vec<String> {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        validator
            .iter_errors(report)
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect()
    }

    #[test]
    fn seeds_42_and_43_give_byte_identical_traces_in_two_fresh_processes_each() {
        let seeds = [MasterSeed::new(42), MasterSeed::new(43)];
        let out = out("identity");
        let report = fresh_processes(&seeds, 2, &out, PROCESS_TIMEOUT, keyval_sim)
            .unwrap_or_else(|err| panic!("rule: every process runs its seed to the end: {err}"));
        let first = fs::read_to_string(trace_path(&out, seeds[0], 0)).unwrap();
        let events = first.lines().filter(|line| line.starts_with("E ")).count();
        assert!(
            report.divergences.is_empty() && events > 100,
            "rule: `./dev sim run --seed 42 --profile slice` gives byte-identical canonical \
             traces in two fresh processes, and so does seed 43 (§12.2 W17; K-14 at T0: 2 \
             seeds x 2 processes, §10.7; T-3, T-11), over seed 42's {events} events; \
             {report}"
        );
    }

    #[test]
    fn the_report_labels_a_passing_slice_and_names_its_trace_by_hash() {
        let (passed, report, trace) = run_once(42, "report");
        let errors = schema_errors(&report);
        let path = out("report").join("seed-42.trace");
        let observed = (
            passed,
            errors.is_empty(),
            report["ok"].as_bool(),
            report["verdict"].as_str(),
            report["label"]
                .as_str()
                .is_some_and(|label| label.starts_with("simulated slice:")),
            report["manifest"]["seed"].as_u64(),
            report["manifest"]["profile"].as_str(),
            report["trace"]["hash"].as_str().map(str::to_owned),
            report["trace"]["lines"].as_u64(),
            report["trace"]["complete"].as_bool(),
            report["trace"]["path"].as_str().map(PathBuf::from),
            trace.lines().next_back() == Some("X incomplete"),
        );
        let expected = (
            true,
            true,
            Some(true),
            Some("pass"),
            true,
            Some(42),
            Some("slice"),
            Some(format!("{:016x}", fnv1a(trace.as_bytes()))),
            Some(u64::try_from(trace.lines().count()).unwrap()),
            Some(true),
            Some(path),
            false,
        );
        assert_eq!(
            observed, expected,
            "rule: the run's report conforms to keyval.dev.sim-run/v1, labels the run a \
             simulated slice (P6), passes, and names the trace it wrote by its FNV-1a hash \
             and line count (§10.2); schema errors {errors:?}; report {report}"
        );
        let facts = &report["facts"];
        assert!(
            facts["elected"]["term"].as_u64() >= Some(1)
                && facts["put"]["index"].as_u64() >= Some(1)
                && facts["read_back"] == Value::Bool(true)
                && facts["crashed"]["unsynced_bytes"].as_u64() >= Some(1)
                && facts["restarted"]["incarnation"].as_u64() == Some(1)
                && facts["mirrors_audited"] == Value::Bool(true),
            "rule: the slice elected, wrote and read back, crashed a follower holding \
             unsynced bytes, restarted it caught up, and audited every mirror (§12.2 W17); \
             facts {facts}"
        );
    }

    #[test]
    fn another_seed_gives_another_trace_the_negative_control() {
        let (passed_42, _, trace_42) = run_once(42, "control-42");
        let (passed_43, _, trace_43) = run_once(43, "control-43");
        let differs = match compare(&trace_42, &trace_43) {
            Comparison::Identical => None,
            Comparison::Differs { line, .. } => Some(line),
        };
        assert!(
            passed_42 && passed_43 && differs.is_some(),
            "rule: seeds 42 and 43 both pass and their traces differ, so the identity \
             comparison can tell two runs apart (K-14); first difference at line {differs:?}"
        );
    }

    #[test]
    fn the_binary_refuses_what_it_cannot_run_and_prints_no_report() {
        let config = config();
        let config = config.to_str().unwrap();
        let cases: [(&[&str], &str); 6] = [
            (
                &[
                    "run",
                    "--seed",
                    "42",
                    "--profile",
                    "mixed",
                    "--config",
                    config,
                ],
                "unknown profile \"mixed\"",
            ),
            (
                &["run", "--profile", "slice", "--config", config],
                "--seed is required",
            ),
            (
                &[
                    "run",
                    "--seed",
                    "42",
                    "--seed",
                    "43",
                    "--profile",
                    "slice",
                    "--config",
                    config,
                ],
                "--seed is given twice",
            ),
            (
                &[
                    "run",
                    "--seed",
                    "x",
                    "--profile",
                    "slice",
                    "--config",
                    config,
                ],
                "--seed:",
            ),
            (
                &[
                    "run",
                    "--seed",
                    "42",
                    "--profile",
                    "slice",
                    "--scenario",
                    "f",
                ],
                "unknown flag \"--scenario\"",
            ),
            (&["campaign"], "unknown command \"campaign\""),
        ];
        for (args, why) in cases {
            let output = keyval_sim_output(args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                (
                    output.status.code(),
                    output.stdout.is_empty(),
                    stderr.contains(why)
                ),
                (Some(2), true, true),
                "rule: keyval-sim refuses {args:?} with exit 2, a reason ({why:?}) and no \
                 report; stderr {stderr:?}"
            );
        }
        let unwritable = keyval_sim_output(&[
            "run",
            "--seed",
            "42",
            "--profile",
            "slice",
            "--config",
            config,
            "--trace",
            "/nonexistent/keyval-sim/seed-42.trace",
        ]);
        let stderr = String::from_utf8_lossy(&unwritable.stderr);
        assert_eq!(
            (
                unwritable.status.code(),
                unwritable.stdout.is_empty(),
                stderr.contains("cannot write")
            ),
            (Some(3), true, true),
            "rule: a run whose trace cannot be written exits 3, never 2 (a usage error), and \
             prints no report; stderr {stderr:?}"
        );
    }
}
