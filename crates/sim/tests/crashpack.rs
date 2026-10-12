#![cfg(test)]
//! Check CRASH-PACK (PLAN.md §10.9, §12.2 W42.2; T-4, T-8, O-6): a failed scenario run
//! leaves a crash pack, and the pack's own `REPLAY.md` command reproduces the failure.
//!
//! Each case runs the `keyval-sim` binary that `./dev sim run` starts. The failure is forced
//! the way the heal tests force it: a heal budget of one tick (seed 13 cannot finish healing
//! in one, §9.4). The replay is read back out of the pack, not rebuilt here, so a pack whose
//! command did not reproduce its run would fail.

mod crashpack {
    // check: CRASH-PACK
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use serde_json::Value;
    use sim::crashpack::FILES;

    const MANIFEST_SCHEMA: &str = include_str!("../../../schemas/dev/crashpack.v1.schema.json");

    /// The seed whose heal cannot finish in one tick.
    const SEED: &str = "13";

    fn config() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/local.toml")
    }

    /// `<target>/tmp/crashpack/<case>`, emptied of an earlier run's files.
    fn out(case: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("crashpack")
            .join(case);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `keyval-sim run <args> --config <config>` with nothing from this environment.
    fn keyval_sim(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_keyval-sim"))
            .arg("run")
            .args(args)
            .arg("--config")
            .arg(config())
            .env_clear()
            .output()
            .unwrap()
    }

    fn report(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "fixture: keyval-sim prints one JSON report ({err}); stdout {:?}, stderr {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    /// The failing run of seed 13 with a one-tick heal, writing its pack under `dir`.
    fn forced_failure(dir: &Path) -> (Output, Value) {
        let build = dir.join("build.json");
        fs::write(
            &build,
            r#"{"build_id":"sha256:test-build","note":"fixture"}"#,
        )
        .unwrap();
        let output = keyval_sim(&[
            "--seed",
            SEED,
            "--profile",
            "swarm",
            "--heal-budget-ticks",
            "1",
            "--build",
            build.to_str().unwrap(),
            "--crashpacks",
            dir.join("packs").to_str().unwrap(),
            "--trace",
            dir.join("run.trace").to_str().unwrap(),
        ]);
        let report = report(&output);
        (output, report)
    }

    fn schema_errors(manifest: &Value) -> Vec<String> {
        let schema: Value = serde_json::from_str(MANIFEST_SCHEMA).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        validator
            .iter_errors(manifest)
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect()
    }

    #[test]
    fn a_forced_failure_leaves_a_complete_pack() {
        let dir = out("complete");
        let (output, report) = forced_failure(&dir);
        let pack = PathBuf::from(report["crash_pack"].as_str().unwrap_or_default());
        let missing: Vec<&str> = FILES
            .iter()
            .copied()
            .filter(|name| !pack.join(name).is_file())
            .collect();
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(pack.join("manifest.json")).unwrap_or_default(),
        )
        .unwrap_or(Value::Null);
        let errors = schema_errors(&manifest);
        let trace = fs::read_to_string(pack.join("trace.txt")).unwrap_or_default();
        let observed = (
            output.status.code(),
            report["verdict"].as_str(),
            missing,
            errors.is_empty(),
            manifest["seed"].as_u64(),
            manifest["build_id"].as_str(),
            manifest["trace_hash"] == report["trace"]["hash"],
            trace == fs::read_to_string(dir.join("run.trace")).unwrap(),
            pack.file_name().and_then(|name| name.to_str()).map(|name| {
                name == format!(
                    "{SEED}-{}",
                    &report["trace"]["hash"].as_str().unwrap_or_default()[..8]
                )
            }),
        );
        assert_eq!(
            observed,
            (
                Some(1),
                Some("fail"),
                Vec::new(),
                true,
                Some(13),
                Some("sha256:test-build"),
                true,
                true,
                Some(true)
            ),
            "rule: a failed run exits 1 and leaves a pack named <seed>-<short hash> holding \
             every §10.9 file, a manifest that conforms to keyval.dev.crashpack/v1 and the \
             full canonical trace (T-8); schema errors {errors:?}; pack {pack:?}; report {report}"
        );
        let history: Value =
            serde_json::from_str(&fs::read_to_string(pack.join("history.json")).unwrap()).unwrap();
        let god_view: Value =
            serde_json::from_str(&fs::read_to_string(pack.join("godview.json")).unwrap()).unwrap();
        assert!(
            history["schema"] == "keyval.observation/v1"
                && god_view["nodes"].as_array().map(Vec::len) == Some(5),
            "rule: the pack holds the operation history (keyval.observation/v1) and every \
             node's state at the failure (§10.9); history {history}; god view {god_view}"
        );
    }

    #[test]
    fn the_replay_command_in_the_pack_reproduces_the_failure() {
        let dir = out("replay");
        let (_, original) = forced_failure(&dir);
        let pack = PathBuf::from(original["crash_pack"].as_str().unwrap());
        let replay_md = fs::read_to_string(pack.join("REPLAY.md")).unwrap();
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(pack.join("manifest.json")).unwrap()).unwrap();
        let command = manifest["replay"].as_str().unwrap();
        // `./dev sim run <flags>` is `keyval-sim run <flags>` with the scenario file, the
        // heal budget and the configuration `./dev` adds.
        let flags: Vec<&str> = command
            .strip_prefix("./dev sim run ")
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        let replayed_trace = dir.join("replayed.trace");
        let mut args = flags.clone();
        args.extend(["--trace", replayed_trace.to_str().unwrap()]);
        let output = keyval_sim(&args);
        let replayed = report(&output);
        let observed = (
            replay_md.contains(command),
            output.status.code(),
            replayed["verdict"].as_str(),
            replayed["failures"].clone(),
            replayed["trace"]["hash"].clone(),
            fs::read_to_string(&replayed_trace).unwrap_or_default()
                == fs::read_to_string(pack.join("trace.txt")).unwrap(),
        );
        assert_eq!(
            observed,
            (
                true,
                Some(1),
                Some("fail"),
                original["failures"].clone(),
                original["trace"]["hash"].clone(),
                true
            ),
            "rule: REPLAY.md names the replay command, and running it ends in the same \
             verdict, the same failure and a byte-identical canonical trace (T-4, T-8); \
             command {command:?}; replayed {replayed}"
        );
    }

    #[test]
    fn a_run_that_did_not_fail_leaves_no_pack_and_a_pack_needs_a_build() {
        let dir = out("none");
        let build = dir.join("build.json");
        fs::write(&build, r#"{"build_id":"sha256:test-build"}"#).unwrap();
        let packs = dir.join("packs");
        let output = keyval_sim(&[
            "--seed",
            "7",
            "--profile",
            "swarm",
            "--build",
            build.to_str().unwrap(),
            "--crashpacks",
            packs.to_str().unwrap(),
        ]);
        let report = report(&output);
        assert_eq!(
            (
                report["verdict"].as_str(),
                report["crash_pack"].is_null(),
                packs.exists()
            ),
            (Some("incomplete"), true, false),
            "rule: only a failed run writes a pack (an unjudged run is incomplete, P10); \
             report {report}"
        );
        let refused = keyval_sim(&[
            "--seed",
            SEED,
            "--profile",
            "swarm",
            "--crashpacks",
            packs.to_str().unwrap(),
        ]);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(
            (
                refused.status.code(),
                refused.stdout.is_empty(),
                stderr.contains("--build")
            ),
            (Some(2), true, true),
            "rule: --crashpacks without --build is a usage error, since a pack names its \
             build; stderr {stderr:?}"
        );
    }
}
