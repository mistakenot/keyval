//! A scenario run and its report (PLAN.md §10.2, §10.9; W42.1): what `keyval-sim run
//! --profile swarm` and `keyval-sim run --scenario FILE` execute.
//!
//! [`run`] drives a [`Scenario`] through the run phases ([`crate::phases`]) and turns the
//! outcome into a [`ScenarioRun`], whose [`report`](ScenarioRun::report) is
//! `keyval.dev.sim-run/v2`. The report names the scenario (seed, swarm, workload, sizes), the
//! heal's outcome, the canonical trace by hash, and a verdict.
//!
//! **The verdict is honest about what judged it (P10, T-13).** No checker is wired to this
//! driver yet: the history checkers read `keyval.observation/v1` and the god-view, heal and
//! vacuity documents, and producing those from a finished run is separate work. So:
//! - a heal that used its whole budget in a valid scenario is a [`Verdict::Fail`], a finding
//!   of the run itself (liveness is K-9's to name precisely);
//! - a run the harness could not drive to its end is [`Verdict::Incomplete`];
//! - a run whose heal converged is also [`Verdict::Incomplete`], with the reason
//!   [`UNJUDGED`]: nothing has judged its history, and a run that was not judged is never
//!   a pass.
//!
//! The report therefore has `ok: false` for every run until the checkers are wired, and its
//! `checkers` object lists which ran (none) and which did not.

use serde_json::{Value as Json, json};
use types::Config;

use crate::cluster::Cluster;
use crate::observe::{self, Meta};
use crate::phases::{self, Finished, HEAL_BUDGET_TICKS, Heal, Stage};
use crate::scenario::{SCHEMA as SCENARIO_SCHEMA, Scenario};
use crate::scheduler::EventBudget;
use crate::slice::{PRNG, Verdict};
use crate::trace::{INCOMPLETE_MARKER, TraceHash, marks_incomplete};

/// The identifier of a scenario run's report.
pub const SCHEMA: &str = "keyval.dev.sim-run/v2";

/// What every scenario run says it is, before anything else (P6).
pub const LABEL: &str = "simulated scenario run: five production node cores over a simulated \
                         disk, network, clock and entropy in one process, under a generated or \
                         read scenario; not a real server or cluster (P6)";

/// Why a run whose heal converged is still not a pass.
pub const UNJUDGED: &str = "no checker has judged this run's history yet (the checkers are \
                            not wired to the simulator); an unjudged run is incomplete, never \
                            a pass (P10)";

/// The checkers a complete run needs (K-13's list), none of which ran.
const CHECKERS: [&str; 12] = [
    "K-1", "K-2", "K-3", "K-4", "K-5", "K-6", "K-7", "K-8", "K-9", "K-10", "K-11", "K-12",
];

/// What a crash pack keeps of a failed run beyond its trace (PLAN.md §10.9, T-8).
#[derive(Clone, Debug)]
pub struct Evidence {
    /// The operation history, `keyval.observation/v1`, or the reason it could not be built.
    pub history: Json,
    /// Every node's state when the run ended.
    pub god_view: Json,
    /// The faults the run injected, in order, with their virtual times.
    pub fault_timeline: Json,
}

/// What a run is started with beyond its scenario.
#[derive(Clone, Debug)]
pub struct Options {
    /// The heal's budget in ticks.
    pub heal_budget_ticks: u64,
    /// The build ID the manifest records (T-3).
    pub build_id: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            heal_budget_ticks: HEAL_BUDGET_TICKS,
            build_id: UNKNOWN_BUILD.to_owned(),
        }
    }
}

/// The build ID of a run whose caller did not say which build it is.
pub const UNKNOWN_BUILD: &str = "unknown";

/// One run of a scenario.
#[derive(Clone, Debug)]
pub struct ScenarioRun {
    /// The scenario's manifest record (§10.2): seed, swarm, workload, sizes.
    pub scenario: Json,
    /// The profile the run was started under: `swarm` for a seed's scenario, `scenario` for a
    /// file.
    pub profile: &'static str,
    /// The configuration's semantic fingerprint, 64 hex digits.
    pub config_fingerprint: String,
    /// The overall verdict.
    pub verdict: Verdict,
    /// What failed, when the verdict is a failure.
    pub failures: Vec<String>,
    /// Why the run is incomplete, when it is.
    pub incomplete: Vec<String>,
    /// How the heal ended, as the report records it.
    pub heal: Json,
    /// The canonical trace.
    pub trace: String,
    /// Events dispatched.
    pub events: u64,
    /// The virtual time the run ended at, in microseconds.
    pub end_us: u64,
    /// What a crash pack keeps, kept only when the verdict is a failure.
    pub evidence: Option<Evidence>,
    /// The options the run was started with.
    pub options: Options,
}

impl ScenarioRun {
    /// The canonical trace's hash.
    #[must_use]
    pub fn hash(&self) -> TraceHash {
        TraceHash::of(&self.trace)
    }

    /// The run's report, `keyval.dev.sim-run/v2`, with `trace_path` where the canonical trace
    /// was written, if it was.
    #[must_use]
    pub fn report(&self, trace_path: Option<&str>) -> Json {
        json!({
            "schema": SCHEMA,
            "ok": self.verdict == Verdict::Pass,
            "label": LABEL,
            "manifest": {
                "scenario_schema": SCENARIO_SCHEMA,
                "profile": self.profile,
                "prng": PRNG,
                "config_fingerprint": self.config_fingerprint,
                "scenario": self.scenario,
            },
            "verdict": self.verdict.as_str(),
            "failures": self.failures,
            "incomplete": self.incomplete,
            "heal": self.heal,
            "checkers": { "ran": Vec::<&str>::new(), "not_run": CHECKERS },
            "trace": {
                "hash": format!("{:016x}", self.hash().get()),
                "lines": self.trace.lines().count(),
                "events": self.events,
                "complete": !marks_incomplete(&self.trace),
                "path": trace_path,
            },
            "virtual_time_us": self.end_us,
            "crash_pack": Json::Null,
        })
    }
}

/// Runs `scenario` under `config` as `profile` (`swarm` or `scenario`).
#[must_use]
pub fn run(config: &Config, scenario: &Scenario, profile: &'static str) -> ScenarioRun {
    run_with(config, scenario, profile, HEAL_BUDGET_TICKS)
}

/// [`run`] with a heal budget of `budget_ticks`, for tests that starve the heal on purpose.
#[must_use]
pub fn run_with(
    config: &Config,
    scenario: &Scenario,
    profile: &'static str,
    budget_ticks: u64,
) -> ScenarioRun {
    let options = Options {
        heal_budget_ticks: budget_ticks,
        ..Options::default()
    };
    run_as(config, scenario, profile, options)
}

/// [`run`] started with `options`.
#[must_use]
pub fn run_as(
    config: &Config,
    scenario: &Scenario,
    profile: &'static str,
    options: Options,
) -> ScenarioRun {
    let budget_ticks = options.heal_budget_ticks;
    let base = ScenarioRun {
        scenario: scenario.manifest(),
        profile,
        config_fingerprint: config.semantic_fingerprint().to_string(),
        verdict: Verdict::Incomplete,
        failures: Vec::new(),
        incomplete: Vec::new(),
        heal: Json::Null,
        trace: format!("{INCOMPLETE_MARKER}\n"),
        events: 0,
        end_us: 0,
        evidence: None,
        options,
    };
    match phases::run_with(config, scenario, budget_ticks) {
        Ok(finished) => {
            let mut run = ScenarioRun {
                heal: heal_record(&finished.heal),
                trace: finished.cluster.trace().canonical(),
                events: finished.cluster.events().get(),
                end_us: finished.cluster.now().as_micros(),
                ..base
            };
            match &finished.heal {
                Heal::Stalled(stalled) => {
                    run.verdict = Verdict::Fail;
                    run.failures.push(format!(
                        "the heal used its whole budget ({} ticks) at stage {}: {} write(s) \
                         unresolved, {} node(s) lagging",
                        stalled.ticks,
                        stage_name(stalled.stage),
                        stalled.unresolved,
                        stalled.lagging.len()
                    ));
                    run.evidence = Some(evidence(
                        &finished,
                        config,
                        scenario,
                        run.profile,
                        &run.options,
                    ));
                }
                Heal::Converged(_) => run.incomplete.push(UNJUDGED.to_owned()),
            }
            run
        }
        Err(error) => {
            let mut run = base;
            run.incomplete
                .push(format!("the run could not be driven: {error:?}"));
            run
        }
    }
}

/// What a failed run leaves for its crash pack.
fn evidence(
    finished: &Finished,
    config: &Config,
    scenario: &Scenario,
    profile: &str,
    options: &Options,
) -> Evidence {
    let meta = Meta {
        seed: scenario.seed().get(),
        scenario_version: 1,
        profile: profile.to_owned(),
        build_id: options.build_id.clone(),
        prng: PRNG.to_owned(),
        max_events: EventBudget::DEFAULT.get(),
        chaos_ticks: finished.chaos_ticks,
        heal_budget_ticks: options.heal_budget_ticks,
        quiet_ticks: 0,
        phases: finished.phases,
    };
    let history = observe::document(&finished.cluster, &finished.clients, config, &meta)
        .unwrap_or_else(|error| json!({ "unavailable": format!("{error:?}") }));
    Evidence {
        history,
        god_view: god_view(&finished.cluster),
        fault_timeline: json!(
            finished
                .cluster
                .faults()
                .map(
                    |(at, fault)| json!({ "at_us": at.as_micros(), "fault": format!("{fault:?}") })
                )
                .collect::<Vec<_>>()
        ),
    }
}

/// Every node's state when the run ended: what the cluster reports of it.
fn god_view(cluster: &Cluster) -> Json {
    json!({
        "leader": cluster.leader().map(types::NodeId::get),
        "nodes": cluster.nodes().map(|node| {
            let controller = cluster.controller(node);
            json!({
                "node": node.get(),
                "incarnation": cluster.incarnation(node).map(|inc| format!("{inc:?}")),
                "running": controller.is_some(),
                "paused": cluster.is_paused(node),
                "lost": cluster.is_lost(node),
                "role": controller.map(|ctl| format!("{:?}", ctl.status().role)),
                "commit": controller.map(|ctl| format!("{:?}", ctl.status().commit)),
                "applied": controller.map(|ctl| format!("{:?}", ctl.state().applied_index())),
                "state_digest": controller.map(|ctl| format!("{:?}", ctl.state().digest())),
            })
        }).collect::<Vec<_>>(),
    })
}

const fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Resolving => "resolving",
        Stage::Marker => "marker",
        Stage::Applying => "applying",
    }
}

fn heal_record(heal: &Heal) -> Json {
    match heal {
        Heal::Converged(converged) => json!({
            "outcome": "converged",
            "ticks": converged.ticks,
            "resumed": converged.resumed,
            "lost": converged.lost.iter().map(|node| node.get()).collect::<Vec<_>>(),
            "nodes": converged.nodes.iter().map(|end| json!({
                "node": end.node.get(),
                "applied": end.applied.get(),
            })).collect::<Vec<_>>(),
        }),
        Heal::Stalled(stalled) => json!({
            "outcome": "stalled",
            "stage": stage_name(stalled.stage),
            "ticks": stalled.ticks,
            "unresolved": stalled.unresolved,
            "lagging": stalled.lagging.iter().map(|node| node.get()).collect::<Vec<_>>(),
        }),
    }
}

#[cfg(test)]
mod tests {
    // check: SIM-SCENARIO-RUN
    use std::path::Path;

    use types::KeyValueLimits;

    use super::*;
    use crate::clients::Profile as Workload;
    use crate::entropy::MasterSeed;

    const LOCAL: &str = include_str!("../../../config/local.toml");

    fn config() -> Config {
        let text = LOCAL.replace("max_value_bytes = 65536", "max_value_bytes = 1024");
        Config::parse(&text, Path::new("config")).unwrap()
    }

    fn limits() -> KeyValueLimits {
        config().key_value_limits()
    }

    fn scenario(seed: u64) -> Scenario {
        Scenario::generate(MasterSeed::new(seed), limits(), Workload::PutGet)
    }

    #[test]
    fn a_seeds_scenario_runs_to_a_complete_trace_and_is_reported() {
        let run = run(&config(), &scenario(3), "swarm");
        let report = run.report(None);
        assert_eq!(
            (
                report["schema"].as_str(),
                report["trace"]["complete"].as_bool(),
                report["manifest"]["scenario"]["seed"].as_u64(),
                report["manifest"]["profile"].as_str(),
            ),
            (Some(SCHEMA), Some(true), Some(3), Some("swarm")),
            "rule: a scenario run reports its schema, a complete trace and its seed; \
             report={report}"
        );
    }

    #[test]
    fn a_run_nothing_judged_is_incomplete_and_not_ok() {
        let run = run(&config(), &scenario(5), "swarm");
        assert_eq!(
            (
                run.verdict,
                run.report(None)["ok"].as_bool(),
                run.heal["outcome"].as_str()
            ),
            (Verdict::Incomplete, Some(false), Some("converged")),
            "rule: with no checker wired, a converged run is incomplete, never a pass (P10); \
             run={run:?}"
        );
        assert_eq!(
            run.incomplete,
            vec![UNJUDGED.to_owned()],
            "rule: the reason says nothing judged the history"
        );
    }

    #[test]
    fn the_same_scenario_gives_the_same_trace_hash() {
        let first = run(&config(), &scenario(7), "swarm");
        let again = run(&config(), &scenario(7), "swarm");
        assert_eq!(
            first.hash(),
            again.hash(),
            "rule: a scenario and its seed fix the run (T-4, T-10); seed=7"
        );
    }

    #[test]
    fn a_scenario_read_from_its_file_reproduces_the_seeds_run() {
        let original = scenario(11);
        let read = Scenario::parse(&original.to_text(), limits()).unwrap();
        let from_seed = run(&config(), &original, "swarm");
        let from_file = run(&config(), &read, "scenario");
        assert_eq!(
            from_seed.hash(),
            from_file.hash(),
            "rule: --scenario FILE reproduces the run of the seed that wrote it (T-10); seed=11"
        );
    }

    #[test]
    fn a_starved_heal_fails_the_run_and_names_its_stage() {
        let run = run_with(&config(), &scenario(13), "swarm", 1);
        assert_eq!(
            (
                run.verdict,
                run.heal["outcome"].as_str(),
                run.heal["ticks"].as_u64()
            ),
            (Verdict::Fail, Some("stalled"), Some(1)),
            "rule: a heal that uses its whole budget fails the run (liveness, K-9 names it); \
             seed=13 budget=1 run={run:?}"
        );
        assert!(
            run.failures
                .iter()
                .any(|failure| failure.contains("whole budget")),
            "rule: the failure says the budget was used; failures={:?}",
            run.failures
        );
    }
}
