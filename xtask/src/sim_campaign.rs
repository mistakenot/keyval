//! `./dev sim run --scenario FILE | --profile swarm`, `./dev sim campaign` and
//! `./dev sim shrink` (PLAN.md §10.9, §12.2 W42.1).
//!
//! A scenario run is the `keyval-sim` binary's `run --scenario FILE` (or `run --seed N
//! --profile swarm`) in a fresh process; its report, `keyval.dev.sim-run/v2`, is parsed
//! strictly and re-emitted ([`ScenarioReport`]). A campaign runs a seed range of the swarm
//! profile in up to [`MAX_JOBS`] parallel processes and reports one line per seed
//! (`keyval.dev.sim-campaign/v1`): it is ok only when every seed passed, and a seed whose
//! process could not report is recorded as `error`, never skipped (P10). `shrink` is the
//! entry point W44's engine fills in; until then it is refused as not implemented, never
//! pretended.
//!
//! Both take the launcher as a parameter, so the tests drive every exit status through the
//! same mapping (check DEV-SIM-CAMPAIGN).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::output::{ErrorKind, Report};
use crate::process::Captured;
use crate::sim::{Refusal, TRACE_DIR, TraceSummary, Verdict, arguments_for, own_message, tail};

/// Schema identifier of a scenario run's report (`schemas/dev/sim-run.v2.schema.json`).
pub const RUN_SCHEMA: &str = "keyval.dev.sim-run/v2";

/// Schema identifier of a campaign's report (`schemas/dev/sim-campaign.v1.schema.json`).
pub const CAMPAIGN_SCHEMA: &str = "keyval.dev.sim-campaign/v1";

/// The profile whose scenarios a seed generates.
pub const SWARM: &str = "swarm";

/// The most parallel `keyval-sim` processes a campaign starts (CD-9).
pub const MAX_JOBS: usize = 64;

/// The most seeds one campaign runs (CD-9); a larger sweep is several campaigns.
pub const MAX_SEEDS: u64 = 10_000;

/// The checkers a run executed and the ones it could not.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Checkers {
    /// The checkers that judged the run.
    pub ran: Vec<String>,
    /// The checkers a complete run needs that did not run.
    pub not_run: Vec<String>,
}

/// The scenario run's manifest (§10.2).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioManifest {
    /// The scenario file format.
    pub scenario_schema: String,
    /// `swarm` for a seed's scenario, `scenario` for a file's.
    pub profile: String,
    /// The pseudo-random generator.
    pub prng: String,
    /// The configuration's semantic fingerprint.
    pub config_fingerprint: String,
    /// The scenario's own manifest record: seed, swarm, workload, sizes.
    pub scenario: Value,
}

/// `keyval.dev.sim-run/v2`: the report `keyval-sim` prints for a scenario run.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioReport {
    /// [`RUN_SCHEMA`].
    pub schema: String,
    /// Whether the verdict is `pass`.
    pub ok: bool,
    /// What the run is, and is not (P6).
    pub label: String,
    /// The run manifest.
    pub manifest: ScenarioManifest,
    /// The verdict.
    pub verdict: Verdict,
    /// What failed, when the verdict is a failure.
    pub failures: Vec<String>,
    /// Why the run is incomplete, when it is.
    pub incomplete: Vec<String>,
    /// How the heal ended.
    pub heal: Value,
    /// Which checkers judged the run.
    pub checkers: Checkers,
    /// The canonical trace.
    pub trace: TraceSummary,
    /// The virtual time the run ended at.
    pub virtual_time_us: u64,
}

impl ScenarioReport {
    /// The scenario's seed, if its manifest record carries one.
    #[must_use]
    pub fn seed(&self) -> Option<u64> {
        self.manifest.scenario.get("seed").and_then(Value::as_u64)
    }
}

impl Report for ScenarioReport {
    fn ok(&self) -> bool {
        self.ok && self.verdict == Verdict::Pass
    }

    fn render_human(&self) -> String {
        let mut text = format!(
            "{}\nseed {}, profile {}: {} ({} events, {} virtual µs)\ntrace {}: {} lines, {}",
            self.label,
            self.seed()
                .map_or_else(|| "?".to_owned(), |seed| seed.to_string()),
            self.manifest.profile,
            verdict_text(self.verdict),
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
        let _ = write!(
            text,
            "\ncheckers run: {}; not run: {}",
            self.checkers.ran.len(),
            self.checkers.not_run.len()
        );
        for failure in &self.failures {
            let _ = write!(text, "\nfailed: {failure}");
        }
        for reason in &self.incomplete {
            let _ = write!(text, "\nincomplete: {reason}");
        }
        text
    }
}

const fn verdict_text(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "FAIL",
        Verdict::Incomplete => "INCOMPLETE (fails, P10)",
    }
}

/// Parses `keyval-sim`'s stdout: exactly one `keyval.dev.sim-run/v2` report.
///
/// # Errors
///
/// What is wrong with it.
pub fn parse(stdout: &str) -> Result<ScenarioReport, String> {
    let report: ScenarioReport = serde_json::from_str(stdout.trim_end())
        .map_err(|err| format!("keyval-sim's report does not parse: {err}"))?;
    if report.schema != RUN_SCHEMA {
        return Err(format!(
            "keyval-sim's report is {:?}, not {RUN_SCHEMA}",
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

/// What a scenario run starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The generated scenario of a seed.
    Seed(u64),
    /// A scenario file.
    File(PathBuf),
}

/// Runs `source` with `launch`, which starts `cargo` with the arguments it is given and waits
/// for it, keeping the trace under `artifacts`.
///
/// # Errors
///
/// A [`Refusal`]: keyval-sim's usage error (its exit 2); a launch, build or I/O failure, a
/// signal or another exit status; or a report that does not parse.
pub fn run_with(
    source: &Source,
    config: &Path,
    artifacts: &Path,
    launch: impl FnOnce(&[&str]) -> Result<Captured, String>,
) -> Result<ScenarioReport, Refusal> {
    let dir = artifacts.join(TRACE_DIR);
    fs::create_dir_all(&dir).map_err(|err| {
        Refusal::new(
            ErrorKind::Internal,
            format!("cannot create {}: {err}", dir.display()),
        )
    })?;
    let trace = dir.join(trace_name(source));
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
    let from = match source {
        Source::Seed(seed) => [
            "--seed".to_owned(),
            seed.to_string(),
            "--profile".to_owned(),
            SWARM.to_owned(),
        ],
        Source::File(file) => [
            "--scenario".to_owned(),
            utf8(file)?,
            String::new(),
            String::new(),
        ],
    };
    let from: Vec<String> = from.into_iter().filter(|arg| !arg.is_empty()).collect();
    let args = arguments_for(&from, config, &trace)?;
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

fn utf8(path: &Path) -> Result<String, Refusal> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Refusal::new(
            ErrorKind::Usage,
            format!("{} is not a UTF-8 path", path.display()),
        )
    })
}

/// The trace file's name under `artifacts/sim/`.
fn trace_name(source: &Source) -> String {
    match source {
        Source::Seed(seed) => format!("{SWARM}-seed-{seed}.trace"),
        Source::File(file) => format!(
            "scenario-{}.trace",
            file.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("file")
        ),
    }
}

/// What `./dev sim campaign` was asked.
#[derive(Clone, Debug)]
pub struct CampaignRequest {
    /// The first seed.
    pub from: u64,
    /// The last seed, inclusive.
    pub to: u64,
    /// The profile; only `swarm` runs a campaign.
    pub profile: String,
    /// The configuration file.
    pub config: PathBuf,
    /// The most parallel processes.
    pub jobs: usize,
}

/// Parses `A..B` (inclusive) as a seed range.
///
/// # Errors
///
/// What is wrong with it, as a usage error message.
pub fn parse_range(text: &str) -> Result<(u64, u64), String> {
    let (from, to) = text
        .split_once("..")
        .ok_or_else(|| format!("--seeds {text:?}: write it as A..B (inclusive)"))?;
    let number = |part: &str| {
        part.parse::<u64>()
            .map_err(|err| format!("--seeds {text:?}: {part:?}: {err}"))
    };
    let (from, to) = (number(from)?, number(to)?);
    if to < from {
        return Err(format!("--seeds {text:?}: the range ends before it starts"));
    }
    if to - from >= MAX_SEEDS {
        return Err(format!(
            "--seeds {text:?}: a campaign runs at most {MAX_SEEDS} seeds; split the sweep"
        ));
    }
    Ok((from, to))
}

/// One seed's line in a campaign.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeedLine {
    /// The seed.
    pub seed: u64,
    /// `pass`, `fail`, `incomplete`, or `error` when no report came back.
    pub verdict: String,
    /// The trace hash, when a report came back.
    pub trace_hash: Option<String>,
    /// The trace file, when one was written.
    pub trace_path: Option<String>,
    /// The first reason the seed is not a pass (a failure, an incomplete reason or the
    /// error).
    pub reason: Option<String>,
}

/// The verdict counts of a campaign.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    /// Seeds that passed.
    pub pass: u64,
    /// Seeds that failed.
    pub fail: u64,
    /// Seeds whose run was incomplete.
    pub incomplete: u64,
    /// Seeds whose process produced no report.
    pub error: u64,
}

/// `keyval.dev.sim-campaign/v1`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignReport {
    /// [`CAMPAIGN_SCHEMA`].
    pub schema: String,
    /// Whether every seed passed.
    pub ok: bool,
    /// What the runs are, and are not (P6).
    pub label: String,
    /// The profile.
    pub profile: String,
    /// The first seed.
    pub from: u64,
    /// The last seed, inclusive.
    pub to: u64,
    /// The parallel processes used.
    pub jobs: u64,
    /// The verdict counts.
    pub counts: Counts,
    /// One line per seed, in seed order.
    pub seeds: Vec<SeedLine>,
}

impl Report for CampaignReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut text = format!(
            "{}\ncampaign {}..{} ({} seeds, {} jobs): {} pass, {} fail, {} incomplete, {} error",
            self.label,
            self.from,
            self.to,
            self.seeds.len(),
            self.jobs,
            self.counts.pass,
            self.counts.fail,
            self.counts.incomplete,
            self.counts.error,
        );
        for line in self.seeds.iter().filter(|line| line.verdict != "pass") {
            let _ = write!(
                text,
                "\nseed {}: {}{}\n  re-run: ./dev sim run --seed {} --profile {}",
                line.seed,
                line.verdict,
                line.reason
                    .as_ref()
                    .map_or_else(String::new, |why| format!(" — {why}")),
                line.seed,
                self.profile,
            );
        }
        text
    }
}

/// What a campaign says its runs are.
pub const CAMPAIGN_LABEL: &str = "simulated campaign: each seed is a simulated scenario run \
                                  (five production node cores over simulated disk, network, \
                                  clock and entropy); test evidence, not proof, and not a real \
                                  cluster (P6)";

/// Runs the seeds of `request` in up to `request.jobs` parallel processes, each started by
/// `launch` (which is given the `cargo` arguments).
///
/// # Errors
///
/// A [`Refusal`] when the request itself is unusable: a profile other than `swarm`, no
/// jobs, or too many. A seed whose process fails is a line of the report, not a refusal.
pub fn campaign_with(
    request: &CampaignRequest,
    artifacts: &Path,
    launch: &(impl Fn(&[&str]) -> Result<Captured, String> + Sync),
) -> Result<CampaignReport, Refusal> {
    if request.profile != SWARM {
        return Err(Refusal::new(
            ErrorKind::Usage,
            format!(
                "campaigns run the `{SWARM}` profile (each seed a generated scenario); {:?} is \
                 not one (run `slice` with `./dev sim run`)",
                request.profile
            ),
        ));
    }
    if request.jobs == 0 || request.jobs > MAX_JOBS {
        return Err(Refusal::new(
            ErrorKind::Usage,
            format!("--jobs {} is outside 1..={MAX_JOBS}", request.jobs),
        ));
    }
    if request.to < request.from || request.to - request.from >= MAX_SEEDS {
        return Err(Refusal::new(
            ErrorKind::Usage,
            format!(
                "seeds {}..{} are not 1..={MAX_SEEDS} seeds",
                request.from, request.to
            ),
        ));
    }
    let seeds: Vec<u64> = (request.from..=request.to).collect();
    let next = AtomicUsize::new(0);
    let lines: Mutex<Vec<SeedLine>> = Mutex::new(Vec::with_capacity(seeds.len()));
    let workers = request.jobs.min(seeds.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::SeqCst);
                    let Some(&seed) = seeds.get(at) else { break };
                    let line = seed_line(
                        seed,
                        run_with(&Source::Seed(seed), &request.config, artifacts, launch),
                    );
                    if let Ok(mut all) = lines.lock() {
                        all.push(line);
                    }
                }
            });
        }
    });
    let mut seeds = lines.into_inner().unwrap_or_default();
    seeds.sort_by_key(|line| line.seed);
    let mut counts = Counts::default();
    for line in &seeds {
        match line.verdict.as_str() {
            "pass" => counts.pass += 1,
            "fail" => counts.fail += 1,
            "incomplete" => counts.incomplete += 1,
            _ => counts.error += 1,
        }
    }
    let expected = request.to - request.from + 1;
    Ok(CampaignReport {
        schema: CAMPAIGN_SCHEMA.to_owned(),
        ok: counts.pass == expected && seeds.len() as u64 == expected,
        label: CAMPAIGN_LABEL.to_owned(),
        profile: request.profile.clone(),
        from: request.from,
        to: request.to,
        jobs: workers as u64,
        counts,
        seeds,
    })
}

fn seed_line(seed: u64, outcome: Result<ScenarioReport, Refusal>) -> SeedLine {
    match outcome {
        Ok(report) => SeedLine {
            seed,
            verdict: match report.verdict {
                Verdict::Pass => "pass",
                Verdict::Fail => "fail",
                Verdict::Incomplete => "incomplete",
            }
            .to_owned(),
            trace_hash: Some(report.trace.hash.clone()),
            trace_path: report.trace.path.clone(),
            reason: report
                .failures
                .first()
                .or_else(|| report.incomplete.first())
                .cloned(),
        },
        Err(refusal) => SeedLine {
            seed,
            verdict: "error".to_owned(),
            trace_hash: None,
            trace_path: None,
            reason: Some(refusal.message),
        },
    }
}

/// `./dev sim shrink <crash-pack>`: W44's engine reduces a failing run to a minimal one; until
/// it lands the command exists and says so.
#[must_use]
pub fn shrink(pack: &Path) -> Refusal {
    Refusal::new(
        ErrorKind::NotImplemented,
        format!(
            "the shrinker arrives with W44; {} was not shrunk (a missing feature never counts \
             as a pass, P10)",
            pack.display()
        ),
    )
}

#[cfg(test)]
pub mod tests {
    // check: DEV-SIM-CAMPAIGN
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::*;
    use crate::test_support::scratch_dir;

    /// A scenario run's report, as `keyval-sim` prints it, for seed `seed` with `verdict`.
    pub fn report_text(seed: u64, verdict: &str) -> String {
        let ok = verdict == "pass";
        let failures = if verdict == "fail" {
            r#"["the heal used its whole budget"]"#
        } else {
            "[]"
        };
        let incomplete = if verdict == "incomplete" {
            r#"["no checker has judged this run"]"#
        } else {
            "[]"
        };
        format!(
            r#"{{"checkers":{{"not_run":["K-1","K-2"],"ran":[]}},"failures":{failures},"heal":{{"outcome":"converged","ticks":222}},"incomplete":{incomplete},"label":"simulated scenario run","manifest":{{"config_fingerprint":"48205fd1b435b33601373568bee9688dd2d780f57d77f83b2a4c2e4c2f9f99dd","prng":"splitmix64","profile":"swarm","scenario":{{"seed":{seed}}},"scenario_schema":"keyval.scenario/v1"}},"ok":{ok},"schema":"keyval.dev.sim-run/v2","trace":{{"complete":true,"events":9,"hash":"00000000000000{seed:02x}","lines":4,"path":"/a/sim/swarm-seed-{seed}.trace"}},"verdict":"{verdict}","virtual_time_us":5}}"#
        )
    }

    /// What a launch returns.
    type Launched = Result<Captured, String>;

    fn captured(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    /// The seed a launch is for, from its `--seed N` arguments.
    fn seed_of(args: &[&str]) -> u64 {
        let at = args.iter().position(|arg| *arg == "--seed").unwrap();
        args[at + 1].parse().unwrap()
    }

    #[test]
    fn a_report_parses_strictly_and_a_disagreeing_one_is_refused() {
        let text = report_text(7, "pass");
        let report = parse(&text).unwrap();
        assert_eq!(
            (report.ok(), report.seed()),
            (true, Some(7)),
            "rule: a v2 report parses and names its seed; text={text}"
        );
        let cases = [
            (
                text.replace("\"virtual_time_us\"", "\"x\":1,\"virtual_time_us\""),
                "an unknown field",
            ),
            (text.replace("sim-run/v2", "sim-run/v1"), "another schema"),
            (
                text.replace("\"ok\":true", "\"ok\":false"),
                "ok disagreeing with the verdict",
            ),
            (format!("{text}\n{text}"), "two reports"),
        ];
        for (stdout, case) in cases {
            assert!(
                parse(&stdout).is_err(),
                "rule: keyval-sim's report is parsed strictly (O-6): {case} is refused"
            );
        }
    }

    #[test]
    fn a_scenario_file_run_passes_the_file_and_a_seed_run_passes_the_swarm_profile() {
        let artifacts = scratch_dir("sim-scenario-args");
        let mut seen = Vec::new();
        let by_file = run_with(
            &Source::File(PathBuf::from("/s/found.json")),
            Path::new("/r/c.toml"),
            &artifacts,
            |args| {
                seen = args.iter().map(|arg| (*arg).to_owned()).collect();
                Ok(captured(1, &report_text(7, "incomplete"), ""))
            },
        )
        .unwrap();
        assert!(
            !by_file.ok()
                && seen
                    .windows(2)
                    .any(|pair| pair == ["--scenario", "/s/found.json"])
                && !seen.iter().any(|arg| arg == "--profile" || arg == "--seed")
                && seen
                    .windows(2)
                    .any(|pair| pair == ["--config", "/r/c.toml"])
                && seen
                    .windows(2)
                    .any(|pair| pair[0] == "--trace" && pair[1].ends_with("scenario-found.trace")),
            "rule: --scenario FILE reaches keyval-sim as `--scenario FILE` alone (the seed is in \
             the file), with the configuration and a trace path; args={seen:?}"
        );
        let mut seen = Vec::new();
        run_with(
            &Source::Seed(9),
            Path::new("/r/c.toml"),
            &artifacts,
            |args| {
                seen = args.iter().map(|arg| (*arg).to_owned()).collect();
                Ok(captured(1, &report_text(9, "incomplete"), ""))
            },
        )
        .unwrap();
        assert!(
            seen.windows(2).any(|pair| pair == ["--seed", "9"])
                && seen.windows(2).any(|pair| pair == ["--profile", "swarm"]),
            "rule: a seed run is `--seed N --profile swarm`; args={seen:?}"
        );
    }

    #[test]
    fn each_exit_status_maps_to_a_report_or_a_refusal_of_its_own_kind() {
        let cases: [(&str, Launched, Result<bool, ErrorKind>); 5] = [
            (
                "exit 1, an incomplete report",
                Ok(captured(1, &report_text(1, "incomplete"), "")),
                Ok(false),
            ),
            (
                "exit 0, a pass",
                Ok(captured(0, &report_text(1, "pass"), "")),
                Ok(true),
            ),
            (
                "exit 2",
                Ok(captured(2, "", "keyval-sim: bad flag\n")),
                Err(ErrorKind::Usage),
            ),
            (
                "exit 3",
                Ok(captured(3, "", "keyval-sim: cannot write\n")),
                Err(ErrorKind::Internal),
            ),
            (
                "no launch",
                Err("cargo: not found".to_owned()),
                Err(ErrorKind::Internal),
            ),
        ];
        for (case, launched, expected) in cases {
            let outcome = run_with(
                &Source::Seed(1),
                Path::new("/r/c.toml"),
                &scratch_dir("sim-scenario-status"),
                |_| launched,
            )
            .map(|report| report.ok())
            .map_err(|refusal| refusal.kind);
            assert_eq!(
                outcome, expected,
                "rule: exit 0 and 1 are reports, 2 is usage, anything else internal, never a pass \
                 (P10); case: {case}"
            );
        }
    }

    #[test]
    fn seed_ranges_parse_inclusive_and_bounded() {
        assert_eq!(parse_range("3..9"), Ok((3, 9)), "rule: A..B is inclusive");
        assert_eq!(parse_range("5..5"), Ok((5, 5)), "rule: one seed is a range");
        for bad in ["3", "9..3", "a..b", "1..", "..4", "0..10000"] {
            assert!(
                parse_range(bad).is_err(),
                "rule: {bad:?} is not a bounded A..B range (CD-9)"
            );
        }
        assert_eq!(
            parse_range("0..9999"),
            Ok((0, 9999)),
            "rule: exactly {MAX_SEEDS} seeds is allowed"
        );
    }

    fn request(from: u64, to: u64, jobs: usize) -> CampaignRequest {
        CampaignRequest {
            from,
            to,
            profile: SWARM.to_owned(),
            config: PathBuf::from("/r/c.toml"),
            jobs,
        }
    }

    #[test]
    fn a_campaign_reports_every_seed_in_order_and_is_ok_only_if_all_pass() {
        let artifacts = scratch_dir("sim-campaign-all");
        let launched = Mutex::new(Vec::new());
        let report = campaign_with(&request(10, 17, 3), &artifacts, &|args: &[&str]| {
            let seed = seed_of(args);
            launched.lock().unwrap().push(seed);
            let verdict = match seed {
                12 => "fail",
                14 => "incomplete",
                _ => "pass",
            };
            Ok(captured(
                i32::from(verdict != "pass"),
                &report_text(seed, verdict),
                "",
            ))
        })
        .unwrap();
        let mut ran = launched.into_inner().unwrap();
        ran.sort_unstable();
        assert_eq!(
            (
                ran,
                report
                    .seeds
                    .iter()
                    .map(|line| line.seed)
                    .collect::<Vec<_>>(),
                report.counts,
                report.ok,
                report.jobs,
            ),
            (
                (10..=17).collect::<Vec<_>>(),
                (10..=17).collect::<Vec<_>>(),
                Counts {
                    pass: 6,
                    fail: 1,
                    incomplete: 1,
                    error: 0
                },
                false,
                3,
            ),
            "rule: a campaign runs every seed exactly once, lists them in seed order, counts \
             verdicts, and one non-pass makes it not ok; report={report:?}"
        );
        let text = report.render_human();
        assert!(
            text.contains("seed 12: fail — the heal used its whole budget")
                && text.contains("re-run: ./dev sim run --seed 14 --profile swarm"),
            "rule: each non-passing seed is listed with its reason and its re-run command; \
             text={text}"
        );
    }

    #[test]
    fn a_seed_whose_process_cannot_report_is_an_error_line_and_the_campaign_fails() {
        let report = campaign_with(
            &request(1, 3, 2),
            &scratch_dir("sim-campaign-error"),
            &|args: &[&str]| {
                let seed = seed_of(args);
                if seed == 2 {
                    Ok(captured(101, "", "error[E0308]: mismatched types\n"))
                } else {
                    Ok(captured(0, &report_text(seed, "pass"), ""))
                }
            },
        )
        .unwrap();
        assert_eq!(
            (
                report.ok,
                report.counts.error,
                report.seeds[1].verdict.as_str(),
                report.seeds[1].trace_hash.clone()
            ),
            (false, 1, "error", None),
            "rule: a seed with no report is an `error` line, never skipped or passed (P10); \
             report={report:?}"
        );
    }

    #[test]
    fn a_campaign_never_runs_more_than_its_jobs_at_once() {
        use std::sync::atomic::AtomicUsize;
        let (live, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let report = campaign_with(
            &request(0, 11, 4),
            &scratch_dir("sim-campaign-jobs"),
            &|args: &[&str]| {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(15));
                live.fetch_sub(1, Ordering::SeqCst);
                let seed = seed_of(args);
                Ok(captured(0, &report_text(seed, "pass"), ""))
            },
        )
        .unwrap();
        let peak = peak.load(Ordering::SeqCst);
        assert!(
            report.ok && (2..=4).contains(&peak),
            "rule: --jobs bounds the parallel processes (CD-9) and a campaign does run in \
             parallel; peak={peak} report={report:?}"
        );
    }

    #[test]
    fn an_unusable_campaign_request_is_refused_before_anything_runs() {
        let artifacts = scratch_dir("sim-campaign-refused");
        let never = |_: &[&str]| -> Result<Captured, String> {
            panic!("rule: nothing is launched for an unusable request")
        };
        let mut slice = request(1, 2, 2);
        slice.profile = "slice".to_owned();
        let cases = [
            (slice, "the slice profile"),
            (request(1, 2, 0), "zero jobs"),
            (request(1, 2, MAX_JOBS + 1), "too many jobs"),
            (request(5, 4, 2), "an empty range"),
            (request(0, MAX_SEEDS, 2), "too many seeds"),
        ];
        for (request, case) in cases {
            assert_eq!(
                campaign_with(&request, &artifacts, &never).map_err(|refusal| refusal.kind),
                Err(ErrorKind::Usage),
                "rule: {case} is a usage error"
            );
        }
    }

    #[test]
    fn shrink_is_refused_as_not_implemented_until_w44() {
        let refusal = shrink(Path::new("artifacts/crashpacks/7-abc"));
        assert_eq!(
            (refusal.kind, refusal.message.contains("W44")),
            (ErrorKind::NotImplemented, true),
            "rule: shrink exists as a command and says its engine is W44's, never a pass (P10)"
        );
    }
}
