//! `./dev sim run --seed N [--profile slice] [--config FILE]`: one simulated run (PLAN.md
//! §11.13, §12.2 W17). W42 extends it with campaigns, crash packs and the corpus runner.
//!
//! The run happens in a fresh process: the `keyval-sim` binary of the `sim` crate, built
//! and started through `cargo run` locally (its trace file must be on this machine). It
//! writes the canonical trace to `artifacts/sim/<profile>-seed-<N>.trace` and prints its
//! report, `keyval.dev.sim-run/v1`, which this module parses strictly and emits again:
//! with `--json` the same fields (in its own key order), else as text whose first line is
//! the run's label, a simulated slice (P6). The binary's exit status 2 (unusable arguments
//! or configuration) is a usage error here; its exit 3 (a trace or report it could not
//! write) and anything else are internal errors; a failed or incomplete run exits 1 (P10).
//! A trace left by an earlier run is removed before the launch, so a run that writes none
//! never leaves stale evidence behind.
//!
//! [`run_with`] takes the launcher, so the tests drive every exit status, stderr and stdout
//! through the same mapping (check DEV-SIM); [`run`] launches through cargo.
//!
//! `--scenario FILE` is part of the plan's surface, and scenario files arrive with W37: it
//! is refused as not implemented, never ignored.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cargo::Cargo;
use crate::output::{ErrorKind, Report};
use crate::process::{self, Captured};

/// Schema identifier of the run report (`schemas/dev/sim-run.v1.schema.json`).
pub const SCHEMA: &str = "keyval.dev.sim-run/v1";

/// Where the traces go, under the artifacts directory.
pub const TRACE_DIR: &str = "sim";

/// The building and the run together, killed after this long.
const LIMIT: Duration = Duration::from_mins(20);

/// The most bytes of the binary's stderr an error keeps (CD-9).
const STDERR_TAIL: usize = 2048;

/// A run's overall verdict.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every step happened and every check held.
    Pass,
    /// A step did not happen, a check did not hold, or a finding stopped the run.
    Fail,
    /// The run stopped before its end; its trace is marked incomplete (P10).
    Incomplete,
}

/// The run manifest (§10.2).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The scenario version.
    pub scenario_version: u64,
    /// The seed.
    pub seed: u64,
    /// The profile.
    pub profile: String,
    /// The pseudo-random generator.
    pub prng: String,
    /// The run's event budget.
    pub max_events: u64,
    /// The configuration's semantic fingerprint.
    pub config_fingerprint: String,
}

/// Step 1: the leader the cluster elected.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Elected {
    /// The leader.
    pub node: u64,
    /// Its term.
    pub term: u64,
    /// When every running node knew it.
    pub at_us: u64,
}

/// Step 2: the first PUT.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Put {
    /// The log index of its first execution.
    pub index: u64,
}

/// Step 4: the follower's crash.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Crashed {
    /// The follower.
    pub node: u64,
    /// The incarnation that crashed.
    pub incarnation: u64,
    /// The unsynced bytes its journal held.
    pub unsynced_bytes: u64,
    /// How many of them survived.
    pub kept_bytes: u64,
    /// Whether zeros or garbage followed the surviving prefix.
    pub damaged_tail: bool,
}

/// Step 5: the follower's restart.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Restarted {
    /// The follower.
    pub node: u64,
    /// Its new incarnation.
    pub incarnation: u64,
    /// The applied index it caught up to.
    pub applied: u64,
}

/// What the run showed, step by step.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    /// Step 1.
    pub elected: Option<Elected>,
    /// Step 2.
    pub put: Option<Put>,
    /// Step 3: the GET read the value back.
    pub read_back: bool,
    /// Step 4.
    pub crashed: Option<Crashed>,
    /// Step 5.
    pub restarted: Option<Restarted>,
    /// Step 6: every running node's mirror agreed with its disk.
    pub mirrors_audited: bool,
}

/// The canonical trace, by hash.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TraceSummary {
    /// 64-bit FNV-1a over the canonical bytes, 16 hex digits.
    pub hash: String,
    /// Lines.
    pub lines: u64,
    /// Events dispatched.
    pub events: u64,
    /// Whether the run reached its end with every required observation recorded.
    pub complete: bool,
    /// The file it was written to.
    pub path: Option<String>,
}

/// `keyval.dev.sim-run/v1`: the report `keyval-sim` prints, parsed strictly.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SimRunReport {
    /// [`SCHEMA`].
    pub schema: String,
    /// Whether the verdict is `pass`.
    pub ok: bool,
    /// What the run is, and is not (P6).
    pub label: String,
    /// The run manifest.
    pub manifest: Manifest,
    /// The verdict.
    pub verdict: Verdict,
    /// Each step that did not happen or check that did not hold.
    pub failures: Vec<String>,
    /// Why the run stopped early, if it did.
    pub error: Option<String>,
    /// What the run showed.
    pub facts: Facts,
    /// The canonical trace.
    pub trace: TraceSummary,
    /// The virtual time the run ended at.
    pub virtual_time_us: u64,
}

impl Report for SimRunReport {
    fn ok(&self) -> bool {
        self.ok && self.verdict == Verdict::Pass
    }

    fn render_human(&self) -> String {
        let mut text = format!(
            "{}\nseed {}, profile {}: {} ({} events, {} virtual µs)\ntrace {}: {} lines, {}",
            self.label,
            self.manifest.seed,
            self.manifest.profile,
            match self.verdict {
                Verdict::Pass => "pass",
                Verdict::Fail => "FAIL",
                Verdict::Incomplete => "INCOMPLETE (fails, P10)",
            },
            self.trace.events,
            self.virtual_time_us,
            self.trace.hash,
            self.trace.lines,
            if self.trace.complete {
                "complete"
            } else {
                "incomplete"
            },
        );
        if let Some(path) = &self.trace.path {
            let _ = write!(text, ", written to {path}");
        }
        let facts = &self.facts;
        if let Some(elected) = facts.elected {
            let _ = write!(text, "\nn{} elected in term {}", elected.node, elected.term);
        }
        if let Some(put) = facts.put {
            let _ = write!(text, "; PUT first executed at index {}", put.index);
            if facts.read_back {
                text.push_str(" and read back");
            }
        }
        if let Some(crashed) = facts.crashed {
            let _ = write!(
                text,
                "\nn{} crashed holding {} unsynced bytes (kept {}{})",
                crashed.node,
                crashed.unsynced_bytes,
                crashed.kept_bytes,
                if crashed.damaged_tail {
                    ", then a damaged tail"
                } else {
                    ""
                }
            );
        }
        if let Some(restarted) = facts.restarted {
            let _ = write!(
                text,
                "; restarted as incarnation {} and caught up to index {}",
                restarted.incarnation, restarted.applied
            );
        }
        if facts.mirrors_audited {
            text.push_str("\nevery node's log mirror agrees with its disk");
        }
        for failure in &self.failures {
            let _ = write!(text, "\nfailed: {failure}");
        }
        if let Some(error) = &self.error {
            let _ = write!(text, "\nstopped early: {error}");
        }
        text
    }
}

/// Why `./dev sim run` produced no report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The kind of error.
    pub kind: ErrorKind,
    /// What went wrong.
    pub message: String,
}

impl Refusal {
    const fn new(kind: ErrorKind, message: String) -> Self {
        Self { kind, message }
    }
}

/// What `./dev sim run` was asked.
#[derive(Clone, Debug)]
pub struct Request {
    /// The seed, unless a scenario file is given.
    pub seed: Option<u64>,
    /// The profile.
    pub profile: String,
    /// A scenario file (W37).
    pub scenario: Option<PathBuf>,
    /// The configuration file.
    pub config: PathBuf,
}

/// The configuration file `--config` names, relative to the workspace `root` unless it is
/// absolute; `config/local.toml` by default.
#[must_use]
pub fn config_path(root: &Path, config: Option<PathBuf>) -> PathBuf {
    config.map_or_else(
        || root.join("config/local.toml"),
        |config| root.join(config),
    )
}

/// Runs `request` through `keyval-sim` in a fresh process built by `cargo`, from the
/// workspace at `root`, keeping its trace under `artifacts`.
///
/// # Errors
///
/// As [`run_with`].
pub fn run(
    root: &Path,
    cargo: &Cargo,
    request: &Request,
    artifacts: &Path,
) -> Result<SimRunReport, Refusal> {
    run_with(request, artifacts, |args| {
        let mut cmd = cargo.command(root, args);
        process::run(&mut cmd, LIMIT).map_err(|err| format!("{}: {err}", cargo.describe(args)))
    })
}

/// Runs `request` with `launch`, which starts `cargo` with the arguments it is given and
/// waits for it, keeping the trace under `artifacts`.
///
/// # Errors
///
/// A [`Refusal`]: a scenario file (not implemented before W37) or no seed; keyval-sim's
/// usage error (its exit 2); a launch, build or I/O failure, a signal or another exit
/// status; or a report that does not parse.
pub fn run_with(
    request: &Request,
    artifacts: &Path,
    launch: impl FnOnce(&[&str]) -> Result<Captured, String>,
) -> Result<SimRunReport, Refusal> {
    if let Some(scenario) = &request.scenario {
        return Err(Refusal::new(
            ErrorKind::NotImplemented,
            format!(
                "scenario files arrive with W37; {} was not run (a missing feature never \
                 counts as a pass, P10)",
                scenario.display()
            ),
        ));
    }
    let Some(seed) = request.seed else {
        return Err(Refusal::new(
            ErrorKind::Usage,
            "give --seed N (or, from W37, --scenario FILE)".to_owned(),
        ));
    };
    let dir = artifacts.join(TRACE_DIR);
    fs::create_dir_all(&dir).map_err(|err| {
        Refusal::new(
            ErrorKind::Internal,
            format!("cannot create {}: {err}", dir.display()),
        )
    })?;
    let trace = dir.join(format!("{}-seed-{seed}.trace", request.profile));
    match fs::remove_file(&trace) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(Refusal::new(
                ErrorKind::Internal,
                format!("cannot remove the earlier {}: {err}", trace.display()),
            ));
        }
    }
    let args = arguments(seed, &request.profile, &request.config, &trace)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = launch(&args).map_err(|why| Refusal::new(ErrorKind::Internal, why))?;
    match captured.status.code() {
        Some(0 | 1) => parse(&captured.stdout).map_err(|why| {
            Refusal::new(
                ErrorKind::Internal,
                format!("{why}; stderr: {}", tail(&captured.stderr)),
            )
        }),
        Some(2) => Err(Refusal::new(
            ErrorKind::Usage,
            own_message(&captured.stderr),
        )),
        _ => Err(Refusal::new(
            ErrorKind::Internal,
            format!(
                "keyval-sim's build or run ended with {}: {}",
                captured.status,
                tail(&captured.stderr)
            ),
        )),
    }
}

/// The `cargo` arguments that build `keyval-sim` and run `seed` with `profile`.
fn arguments(
    seed: u64,
    profile: &str,
    config: &Path,
    trace: &Path,
) -> Result<Vec<String>, Refusal> {
    let utf8 = |path: &Path| {
        path.to_str().map(str::to_owned).ok_or_else(|| {
            Refusal::new(
                ErrorKind::Usage,
                format!("{} is not a UTF-8 path", path.display()),
            )
        })
    };
    Ok([
        "run",
        "--locked",
        "--quiet",
        "-p",
        "sim",
        "--bin",
        "keyval-sim",
        "--",
        "run",
        "--seed",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([
        seed.to_string(),
        "--profile".to_owned(),
        profile.to_owned(),
        "--config".to_owned(),
        utf8(config)?,
        "--trace".to_owned(),
        utf8(trace)?,
    ])
    .collect())
}

/// Parses `keyval-sim`'s stdout: exactly one `keyval.dev.sim-run/v1` report.
///
/// # Errors
///
/// What is wrong with it.
pub fn parse(stdout: &str) -> Result<SimRunReport, String> {
    let report: SimRunReport = serde_json::from_str(stdout.trim_end())
        .map_err(|err| format!("keyval-sim's report does not parse: {err}"))?;
    if report.schema != SCHEMA {
        return Err(format!(
            "keyval-sim's report is {:?}, not {SCHEMA}",
            report.schema
        ));
    }
    if report.ok != (report.verdict == Verdict::Pass) {
        return Err(format!(
            "keyval-sim's report says ok={} with verdict {:?}",
            report.ok, report.verdict
        ));
    }
    Ok(report)
}

/// `keyval-sim`'s own message on `stderr`: from its last line that starts `keyval-sim:`
/// to the end, without the compiler output cargo replays before it; else the tail.
fn own_message(stderr: &str) -> String {
    let start = stderr
        .rmatch_indices("keyval-sim: ")
        .map(|(at, _)| at)
        .find(|at| *at == 0 || stderr.as_bytes().get(at - 1) == Some(&b'\n'));
    start.map_or_else(
        || tail(stderr),
        |at| tail(stderr.get(at..).unwrap_or_default()),
    )
}

/// The last [`STDERR_TAIL`] bytes of `stderr`, at a character boundary.
fn tail(stderr: &str) -> String {
    let trimmed = stderr.trim_end();
    let mut start = trimmed.len().saturating_sub(STDERR_TAIL);
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed.get(start..).unwrap_or_default().to_owned()
}

#[cfg(test)]
pub mod tests {
    // check: DEV-SIM
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::*;
    use crate::test_support::scratch_dir;

    /// A real report: seed 42's, as `keyval-sim` printed it.
    pub const PASSING: &str = r#"{"error":null,"facts":{"crashed":{"damaged_tail":true,"incarnation":0,"kept_bytes":0,"node":1,"unsynced_bytes":94},"elected":{"at_us":1110000,"node":2,"term":1},"mirrors_audited":true,"put":{"index":2},"read_back":true,"restarted":{"applied":3,"incarnation":1,"node":1}},"failures":[],"label":"simulated slice: five production node cores over a simulated disk, network, clock and entropy in one process; not a real server or cluster (P6)","manifest":{"config_fingerprint":"48205fd1b435b33601373568bee9688dd2d780f57d77f83b2a4c2e4c2f9f99dd","max_events":1000000,"prng":"splitmix64","profile":"slice","scenario_version":1,"seed":42},"ok":true,"schema":"keyval.dev.sim-run/v1","trace":{"complete":true,"events":268,"hash":"34ec631c053276db","lines":629,"path":"/artifacts/sim/slice-seed-42.trace"},"verdict":"pass","virtual_time_us":1306550}"#;

    #[test]
    fn a_report_parses_strictly_and_renders_its_label_first() {
        let report = parse(PASSING).unwrap();
        let text = report.render_human();
        assert_eq!(
            (
                report.ok(),
                text.lines().next(),
                text.contains("seed 42, profile slice: pass"),
                text.contains("trace 34ec631c053276db: 629 lines, complete"),
                text.contains("restarted as incarnation 1"),
            ),
            (true, Some(report.label.as_str()), true, true, true),
            "rule: `./dev sim run` prints the run's label first (P6), then its verdict, \
             trace hash and steps; text={text}"
        );
    }

    #[test]
    fn a_report_that_differs_from_its_schema_is_refused() {
        let cases = [
            (
                PASSING.replace("\"virtual_time_us\"", "\"extra\":1,\"virtual_time_us\""),
                "an unknown field",
            ),
            (
                PASSING.replace("sim-run/v1", "sim-run/v2"),
                "another schema",
            ),
            (
                PASSING.replace("\"ok\":true", "\"ok\":false"),
                "ok disagreeing with the verdict",
            ),
            (
                PASSING.replace("\"verdict\":\"pass\"", "\"verdict\":\"passed\""),
                "an unknown verdict",
            ),
            (format!("{PASSING}\n{PASSING}"), "two reports"),
        ];
        for (stdout, case) in cases {
            assert!(
                parse(&stdout).is_err(),
                "rule: keyval-sim's report is parsed strictly (O-6): {case} is refused"
            );
        }
    }

    #[test]
    fn a_failed_or_incomplete_run_never_passes() {
        let failed = parse(
            &PASSING
                .replace("\"ok\":true", "\"ok\":false")
                .replace("\"verdict\":\"pass\"", "\"verdict\":\"incomplete\"")
                .replace("\"error\":null", "\"error\":\"budget\""),
        )
        .unwrap();
        assert!(
            !failed.ok() && failed.render_human().contains("INCOMPLETE (fails, P10)"),
            "rule: an incomplete run fails, and says so (P10); {failed:?}"
        );
    }

    #[test]
    fn the_command_builds_and_runs_keyval_sim_with_the_seed_profile_config_and_trace() {
        let args = arguments(
            42,
            "slice",
            Path::new("/r/config/local.toml"),
            Path::new("/a/sim/slice-seed-42.trace"),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "run",
                "--locked",
                "--quiet",
                "-p",
                "sim",
                "--bin",
                "keyval-sim",
                "--",
                "run",
                "--seed",
                "42",
                "--profile",
                "slice",
                "--config",
                "/r/config/local.toml",
                "--trace",
                "/a/sim/slice-seed-42.trace"
            ],
            "rule: `./dev sim run` runs the keyval-sim binary of the sim crate, locked, with \
             the seed, profile, configuration and trace path it was given"
        );
    }

    #[test]
    fn a_usage_error_is_keyval_sims_own_message_without_cargos_output() {
        let stderr = "warning: hiding a lifetime\n  --> vendor/raft/src/x.rs:1:1\n\
                      keyval-sim: unknown profile \"mixed\"\nusage: keyval-sim run --seed N\n";
        assert_eq!(
            (own_message(stderr), own_message("no message of its own\n")),
            (
                "keyval-sim: unknown profile \"mixed\"\nusage: keyval-sim run --seed N".to_owned(),
                "no message of its own".to_owned()
            ),
            "rule: a usage error reports why keyval-sim refused, not the compiler warnings \
             cargo replays before it"
        );
    }

    /// A request for seed 42's slice under `config`.
    fn request(config: &str) -> Request {
        Request {
            seed: Some(42),
            profile: "slice".to_owned(),
            scenario: None,
            config: PathBuf::from(config),
        }
    }

    /// What a launch returns.
    type Launched = Result<Captured, String>;

    /// A launch that ran: `status` as `wait(2)` encodes it, and both streams.
    fn captured(status: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            status: ExitStatus::from_raw(status),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    /// Exit status `code`, as `wait(2)` encodes it.
    const fn exited(code: i32) -> i32 {
        code << 8
    }

    #[test]
    fn a_passing_run_is_launched_with_its_trace_path_and_parsed() {
        let artifacts = scratch_dir("sim-relay-pass");
        let trace = artifacts.join(TRACE_DIR).join("slice-seed-42.trace");
        fs::create_dir_all(artifacts.join(TRACE_DIR)).unwrap();
        fs::write(&trace, "a trace an earlier run left").unwrap();
        let mut seen = Vec::new();
        let mut stale = true;
        let report = run_with(&request("/r/config/local.toml"), &artifacts, |args| {
            seen = args.iter().map(|arg| (*arg).to_owned()).collect();
            stale = trace.exists();
            Ok(captured(exited(0), PASSING, "warning: replayed by cargo\n"))
        })
        .unwrap();
        assert!(
            !stale
                && report.ok()
                && artifacts.join(TRACE_DIR).is_dir()
                && seen
                    .windows(2)
                    .any(|pair| pair == ["--config", "/r/config/local.toml"])
                && seen
                    .windows(2)
                    .any(|pair| pair[0] == "--trace" && Path::new(&pair[1]) == trace),
            "rule: `./dev sim run` removes an earlier run's trace, starts keyval-sim with the \
             configuration and a trace path under artifacts/sim/, and a passing run's report is \
             parsed and ok; stale={stale} args={seen:?}"
        );
    }

    #[test]
    fn each_exit_status_maps_to_a_report_or_a_refusal_of_its_own_kind() {
        let failed = PASSING
            .replace("\"ok\":true", "\"ok\":false")
            .replace("\"verdict\":\"pass\"", "\"verdict\":\"fail\"")
            .replace("\"failures\":[]", "\"failures\":[\"the GET read nothing\"]");
        let usage = "warning: hiding a lifetime\nkeyval-sim: unknown profile \"mixed\"\nusage: x\n";
        let cases: [(&str, Launched, Result<bool, ErrorKind>); 8] = [
            (
                "exit 0, a passing report",
                Ok(captured(exited(0), PASSING, "")),
                Ok(true),
            ),
            (
                "exit 1, a failed report",
                Ok(captured(exited(1), &failed, "")),
                Ok(false),
            ),
            (
                "exit 2, a usage error",
                Ok(captured(exited(2), "", usage)),
                Err(ErrorKind::Usage),
            ),
            (
                "exit 3, a trace it could not write",
                Ok(captured(
                    exited(3),
                    "",
                    "keyval-sim: cannot write /a/t: denied\n",
                )),
                Err(ErrorKind::Internal),
            ),
            (
                "exit 101, a build failure",
                Ok(captured(
                    exited(101),
                    "",
                    "error[E0308]: mismatched types\n",
                )),
                Err(ErrorKind::Internal),
            ),
            (
                "killed by a signal",
                Ok(captured(9, "", "")),
                Err(ErrorKind::Internal),
            ),
            (
                "exit 0, no report",
                Ok(captured(exited(0), "not json", "")),
                Err(ErrorKind::Internal),
            ),
            (
                "a launch that cannot start",
                Err("cargo: not found".to_owned()),
                Err(ErrorKind::Internal),
            ),
        ];
        for (case, launched, expected) in cases {
            let artifacts = scratch_dir("sim-relay-status");
            let outcome = run_with(&request("/r/c.toml"), &artifacts, |_| launched)
                .map(|report| report.ok())
                .map_err(|refusal| refusal.kind);
            assert_eq!(
                outcome, expected,
                "rule: `./dev sim run` maps keyval-sim's exit 0 and 1 to its report (ok only \
                 when it passed), its exit 2 to a usage error and anything else to an internal \
                 error, never a pass (P10); case: {case}"
            );
        }
        let refusal = run_with(
            &request("/r/c.toml"),
            &scratch_dir("sim-relay-usage"),
            |_| Ok(captured(exited(2), "", usage)),
        )
        .unwrap_err();
        assert_eq!(
            refusal.message, "keyval-sim: unknown profile \"mixed\"\nusage: x",
            "rule: the usage error is keyval-sim's own message"
        );
    }

    #[test]
    fn the_configuration_is_relative_to_the_workspace_root_by_default_local() {
        let root = Path::new("/w");
        assert_eq!(
            (
                config_path(root, None),
                config_path(root, Some(PathBuf::from("config/other.toml"))),
                config_path(root, Some(PathBuf::from("/etc/keyval.toml"))),
            ),
            (
                PathBuf::from("/w/config/local.toml"),
                PathBuf::from("/w/config/other.toml"),
                PathBuf::from("/etc/keyval.toml"),
            ),
            "rule: `--config` defaults to config/local.toml and is relative to the workspace \
             root unless absolute (§11.13)"
        );
    }

    #[test]
    fn a_scenario_file_is_refused_as_not_implemented() {
        let request = Request {
            seed: None,
            profile: "slice".to_owned(),
            scenario: Some(PathBuf::from("scenario.toml")),
            config: PathBuf::from("config/local.toml"),
        };
        let refused = run_with(&request, Path::new("/nonexistent"), |_| {
            panic!("rule: nothing is launched for a scenario file")
        });
        assert_eq!(
            refused.map_err(|refusal| refusal.kind),
            Err(ErrorKind::NotImplemented),
            "rule: `--scenario` is refused before anything runs, never ignored (P10, W37)"
        );
    }
}
