//! `keyval-sim`: runs one simulated slice (PLAN.md §12.2 W17), the program `./dev sim run`
//! starts in a fresh process.
//!
//! ```text
//! keyval-sim run --seed N --profile slice|swarm --config FILE [--trace FILE]
//! keyval-sim run --scenario FILE --config FILE [--trace FILE]
//!     [--heal-budget-ticks N] [--build FILE --crashpacks DIR]
//! ```
//!
//! With `--profile slice` it runs the slice ([`sim::slice`]) under the configuration in
//! `--config`, writes the canonical trace to `--trace` if given, and prints the run's report
//! (`keyval.dev.sim-run/v1`) on stdout as one JSON object, labelled a simulated slice (P6).
//! With `--profile swarm` it generates the seed's scenario, and with `--scenario FILE` it reads
//! one (the seed is in the file, so `--seed` and `--profile` are refused there); either way
//! it drives the scenario through the run phases ([`sim::scenario_run`]) and prints
//! `keyval.dev.sim-run/v2`.
//!
//! A failed scenario run writes its crash pack ([`sim::crashpack`], PLAN.md §10.9) under
//! `--crashpacks`, with `--build` (the build identity JSON) as `build.json`, and names the
//! pack's directory in the report's `crash_pack`.
//!
//! Exit status: 0 the run passed; 1 it failed or is incomplete (P10); 2 the arguments or
//! the configuration were unusable, and nothing ran; 3 the run happened but its trace or
//! report could not be written (`./dev sim run` reports that as an internal error, never as
//! a usage error).

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sim::clients::Profile as Workload;
use sim::crashpack;
use sim::entropy::MasterSeed;
use sim::scenario::Scenario;
use sim::scenario_run;
use sim::scenario_run::Options;
use sim::scheduler::EventBudget;
use sim::slice::{self, Profile, Verdict};
use types::Config;

/// The profile name of a seed's generated scenario.
const SWARM: &str = "swarm";

const USAGE: &str = "usage: keyval-sim run --seed N --profile slice|swarm --config FILE \
[--trace FILE]\n       keyval-sim run --scenario FILE --config FILE [--trace FILE] \
[--heal-budget-ticks N] [--build FILE --crashpacks DIR]";

/// Why keyval-sim stopped without printing a report.
enum Stop {
    /// Unusable arguments or configuration: nothing ran (exit 2).
    Usage(String),
    /// The run happened, but its trace or report could not be written (exit 3).
    Io(String),
}

/// What is run.
enum Source {
    /// The slice of a seed.
    Slice(MasterSeed),
    /// The generated scenario of a seed.
    Swarm(MasterSeed),
    /// The scenario in a file.
    Scenario(PathBuf),
}

/// The parsed arguments.
struct Args {
    source: Source,
    config: PathBuf,
    trace: Option<PathBuf>,
    heal_budget_ticks: Option<u64>,
    build: Option<PathBuf>,
    crashpacks: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stderr = |text: &str| {
        // Nowhere left to report to if stderr is closed; the status still says it.
        let _ = writeln!(io::stderr().lock(), "keyval-sim: {text}");
    };
    match parse(&args)
        .map_err(Stop::Usage)
        .and_then(|args| run(&args))
    {
        Ok(verdict) => match verdict {
            Verdict::Pass => ExitCode::SUCCESS,
            Verdict::Fail | Verdict::Incomplete => ExitCode::from(1),
        },
        Err(Stop::Usage(why)) => {
            stderr(&format!("{why}\n{USAGE}"));
            ExitCode::from(2)
        }
        Err(Stop::Io(why)) => {
            stderr(&why);
            ExitCode::from(3)
        }
    }
}

/// Parses `run` and its flags, each exactly once; anything else is refused.
fn parse(args: &[String]) -> Result<Args, String> {
    let Some((command, flags)) = args.split_first() else {
        return Err("no command".to_owned());
    };
    if command != "run" {
        return Err(format!("unknown command {command:?}"));
    }
    let (mut seed, mut profile, mut config, mut trace, mut scenario) =
        (None, None, None, None, None);
    let (mut budget, mut build, mut crashpacks) = (None, None, None);
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        let Some(value) = rest.next() else {
            return Err(format!("{flag} needs a value"));
        };
        let slot = match flag.as_str() {
            "--seed" => &mut seed,
            "--profile" => &mut profile,
            "--config" => &mut config,
            "--trace" => &mut trace,
            "--scenario" => &mut scenario,
            "--heal-budget-ticks" => &mut budget,
            "--build" => &mut build,
            "--crashpacks" => &mut crashpacks,
            _ => return Err(format!("unknown flag {flag:?}")),
        };
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    let config = PathBuf::from(config.ok_or("--config is required")?);
    let source = match (scenario, seed, profile) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(
                "--scenario carries its own seed: give neither --seed nor --profile".to_owned(),
            );
        }
        (Some(path), None, None) => Source::Scenario(PathBuf::from(path)),
        (None, seed, profile) => {
            let seed = MasterSeed::new(
                seed.ok_or("--seed is required (or --scenario FILE)")?
                    .parse::<u64>()
                    .map_err(|err| format!("--seed: {err}"))?,
            );
            let profile = profile.ok_or("--profile is required")?;
            if profile == SWARM {
                Source::Swarm(seed)
            } else {
                Profile::parse(&profile).ok_or_else(|| {
                    format!("unknown profile {profile:?}: `slice` and `{SWARM}` exist")
                })?;
                Source::Slice(seed)
            }
        }
    };
    if crashpacks.is_some() && build.is_none() {
        return Err(
            "--crashpacks needs --build FILE: a pack names the build it came from".to_owned(),
        );
    }
    let heal_budget_ticks = budget
        .map(|text| {
            text.parse::<u64>()
                .map_err(|err| format!("--heal-budget-ticks: {err}"))
        })
        .transpose()?;
    if matches!(source, Source::Slice(_))
        && (crashpacks.is_some() || build.is_some() || heal_budget_ticks.is_some())
    {
        return Err("--crashpacks, --build and --heal-budget-ticks apply to swarm and scenario runs, not the slice".to_owned());
    }
    Ok(Args {
        source,
        config,
        trace: trace.map(PathBuf::from),
        heal_budget_ticks,
        build: build.map(PathBuf::from),
        crashpacks: crashpacks.map(PathBuf::from),
    })
}

/// Runs what `args` names, writes its trace and prints its report; returns its verdict.
fn run(args: &Args) -> Result<Verdict, Stop> {
    let text = fs::read_to_string(&args.config)
        .map_err(|err| Stop::Usage(format!("cannot read {}: {err}", args.config.display())))?;
    let base = args.config.parent().unwrap_or_else(|| Path::new("."));
    let config = Config::parse(&text, base)
        .map_err(|err| Stop::Usage(format!("{}: {err:?}", args.config.display())))?;
    let path = match &args.trace {
        Some(path) => Some(
            path.to_str()
                .ok_or_else(|| Stop::Usage(format!("{} is not UTF-8", path.display())))?,
        ),
        None => None,
    };
    let (trace, report, verdict) = match &args.source {
        Source::Slice(seed) => {
            let run = slice::run(&config, *seed, Profile::Slice, EventBudget::DEFAULT);
            (run.trace.clone(), run.report(path), run.verdict)
        }
        Source::Swarm(seed) => {
            let scenario = Scenario::generate(*seed, config.key_value_limits(), Workload::PutGet);
            scenario_report(&config, &scenario, SWARM, path, args)?
        }
        Source::Scenario(file) => {
            let text = fs::read_to_string(file)
                .map_err(|err| Stop::Usage(format!("cannot read {}: {err}", file.display())))?;
            let scenario = Scenario::parse(&text, config.key_value_limits())
                .map_err(|err| Stop::Usage(format!("{}: {err}", file.display())))?;
            scenario_report(&config, &scenario, "scenario", path, args)?
        }
    };
    if let Some(path) = path {
        fs::write(path, &trace).map_err(|err| Stop::Io(format!("cannot write {path}: {err}")))?;
    }
    let report = serde_json::to_string(&report)
        .map_err(|err| Stop::Io(format!("cannot encode the report: {err}")))?;
    writeln!(io::stdout().lock(), "{report}").map_err(|err| Stop::Io(format!("stdout: {err}")))?;
    Ok(verdict)
}

/// Runs `scenario` and returns its trace, report and verdict. A failed run also writes its
/// crash pack (PLAN.md §10.9) under `--crashpacks`, and the report names it.
fn scenario_report(
    config: &Config,
    scenario: &Scenario,
    profile: &'static str,
    path: Option<&str>,
    args: &Args,
) -> Result<(String, serde_json::Value, Verdict), Stop> {
    let build = match &args.build {
        Some(file) => {
            let text = fs::read_to_string(file)
                .map_err(|err| Stop::Usage(format!("cannot read {}: {err}", file.display())))?;
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|err| Stop::Usage(format!("{}: {err}", file.display())))?
        }
        None => serde_json::Value::Null,
    };
    let mut options = Options::default();
    if let Some(ticks) = args.heal_budget_ticks {
        options.heal_budget_ticks = ticks;
    }
    if let Some(id) = build.get("build_id").and_then(serde_json::Value::as_str) {
        id.clone_into(&mut options.build_id);
    }
    let run = scenario_run::run_as(config, scenario, profile, options);
    let mut report = run.report(path);
    if let Some(root) = &args.crashpacks {
        let root_text = root
            .to_str()
            .ok_or_else(|| Stop::Usage(format!("{} is not UTF-8", root.display())))?;
        if let Some(pack) = crashpack::pack(&run, scenario, &build, root_text) {
            let dir = root.join(&pack.name);
            fs::create_dir_all(&dir)
                .map_err(|err| Stop::Io(format!("cannot create {}: {err}", dir.display())))?;
            for (name, contents) in &pack.files {
                let file = dir.join(name);
                fs::write(&file, contents)
                    .map_err(|err| Stop::Io(format!("cannot write {}: {err}", file.display())))?;
            }
            report["crash_pack"] = serde_json::Value::String(dir.display().to_string());
        }
    }
    Ok((run.trace.clone(), report, run.verdict))
}
