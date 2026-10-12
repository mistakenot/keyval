//! Crash packs (PLAN.md §10.9, T-8): everything a failed run leaves so that it can be
//! reproduced and diagnosed.
//!
//! [`pack`] turns a failed [`ScenarioRun`] into a [`Pack`]: a directory name and the files
//! that go in it. It does no I/O (the program that runs the scenario writes the files), so
//! the same run always yields the same pack.
//!
//! The files:
//! - `manifest.json`, schema [`SCHEMA`]: seed, profile, verdict, the finding that failed the
//!   run, the build ID, configuration fingerprint and trace hash, and the heal's record;
//! - `scenario.json`: the scenario, runnable on its own (`--scenario`);
//! - `build.json`: the build identity the caller supplied (toolchain, lockfile, patch and
//!   source identity), verbatim;
//! - `history.json`: the operation history, `keyval.observation/v1`;
//! - `trace.txt`: the full canonical trace;
//! - `faults.json`: the faults the run injected, in order;
//! - `godview.json`: every node's state when the run ended;
//! - `REPLAY.md`: the exact replay command and a summary.
//!
//! **What the pack does not yet name:** no history checker is wired to the simulator, so
//! `failing_checker` is `null` and `violating_operations` is empty; the finding is the run's
//! own (a heal that used its whole budget). Both fill in when checkers are wired. The shrunk
//! scenario joins the pack when the shrinker (W44) exists.

use serde_json::{Value as Json, json};

use crate::scenario::{SCHEMA as SCENARIO_SCHEMA, Scenario};
use crate::scenario_run::ScenarioRun;
use crate::slice::Verdict;

/// The identifier of a pack's manifest.
pub const SCHEMA: &str = "keyval.dev.crashpack/v1";

/// The files every pack holds, in the order the manifest lists them.
pub const FILES: [&str; 8] = [
    "manifest.json",
    "scenario.json",
    "build.json",
    "history.json",
    "trace.txt",
    "faults.json",
    "godview.json",
    "REPLAY.md",
];

/// A pack to write: the directory's name and its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    /// `<seed>-<short trace hash>`, the directory under `artifacts/crashpacks/`.
    pub name: String,
    /// Each file's name (one of [`FILES`]) and contents.
    pub files: Vec<(&'static str, String)>,
}

/// The pack of `run` (a run of `scenario`), or `None` if the run did not fail.
///
/// `build` is the build identity JSON, kept as `build.json`. The pack goes in
/// `<root>/<name>`, which is where the replay command reads `scenario.json`.
#[must_use]
pub fn pack(run: &ScenarioRun, scenario: &Scenario, build: &Json, root: &str) -> Option<Pack> {
    let evidence = run.evidence.as_ref()?;
    if run.verdict != Verdict::Fail {
        return None;
    }
    let seed = scenario.seed().get();
    let hash = format!("{:016x}", run.hash().get());
    let name = format!("{seed}-{}", &hash[..8]);
    let replay = replay_command(&format!("{root}/{name}"), run.options.heal_budget_ticks);
    let manifest = json!({
        "schema": SCHEMA,
        "seed": seed,
        "profile": run.profile,
        "scenario_schema": SCENARIO_SCHEMA,
        "verdict": run.verdict.as_str(),
        "finding": run.failures,
        "failing_checker": Json::Null,
        "violating_operations": Vec::<Json>::new(),
        "build_id": run.options.build_id,
        "config_fingerprint": run.config_fingerprint,
        "heal_budget_ticks": run.options.heal_budget_ticks,
        "heal": run.heal,
        "trace_hash": hash,
        "events": run.events,
        "virtual_time_us": run.end_us,
        "replay": replay,
        "files": FILES,
    });
    let files = vec![
        ("manifest.json", pretty(&manifest)),
        ("scenario.json", scenario.to_text()),
        ("build.json", pretty(build)),
        ("history.json", pretty(&evidence.history)),
        ("trace.txt", run.trace.clone()),
        ("faults.json", pretty(&evidence.fault_timeline)),
        ("godview.json", pretty(&evidence.god_view)),
        ("REPLAY.md", replay_md(run, seed, &hash, &replay)),
    ];
    Some(Pack { name, files })
}

/// The command that reproduces the run from the pack's own scenario file.
#[must_use]
pub fn replay_command(replay_dir: &str, heal_budget_ticks: u64) -> String {
    format!(
        "./dev sim run --scenario {replay_dir}/scenario.json --heal-budget-ticks {heal_budget_ticks}"
    )
}

fn pretty(value: &Json) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

fn replay_md(run: &ScenarioRun, seed: u64, hash: &str, replay: &str) -> String {
    let finding = run.failures.join("; ");
    format!(
        "# Replay\n\nRun this from the repository root, on the build named in `build.json`:\n\n\
         ```\n{replay}\n```\n\n\
         Seed {seed}, profile `{profile}`: the run failed with: {finding}. The replay reads \
         `scenario.json` (which carries the seed) and the same heal budget, so it must end in \
         the same verdict with canonical trace hash `{hash}` (T-4, T-10). A different hash \
         means a different build (compare `build.json`) or a changed scenario.\n\n\
         `history.json` is the operation history, `faults.json` the injected faults, \
         `godview.json` each node's state at the end and `trace.txt` the full canonical \
         trace. No history checker is wired to the simulator yet, so the manifest names no \
         failing checker.\n",
        profile = run.profile,
    )
}
