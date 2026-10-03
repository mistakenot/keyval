//! `./dev check <ID>... [--case NAME]`: run registered checks by ID (PLAN.md §10.13,
//! §11.13).
//!
//! No vacuous passes (P10): an unknown ID runs nothing and fails; a selection that
//! matches zero cases fails; an ignored selected case fails; and a full run must execute
//! at least the check's `min_cases`.
//!
//! Until W05 lands `registry/checks.toml`, checks come from the built-in bootstrap
//! [`CATALOGUE`]. Each entry already carries the registry's fields (id, kind, covers,
//! class, tier, run selector, `min_cases`), and its code marker (`// check: <ID>`) sits in
//! the test module it selects, so W05 can move the rows across unchanged.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cargo::Cargo;
use crate::libtest::{self, Tally};
use crate::output::Report;
use crate::process::{self, Captured};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.check/v1";

/// Wall-clock limit for listing or running one check's cases, including compilation.
const RUN_LIMIT: Duration = Duration::from_mins(30);

/// Most output lines echoed for a failed check in human mode (CD-9).
const FAILURE_TAIL_LINES: usize = 120;

/// What sort of verification a check is (`checks.toml` `kind`). W05 adds the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A group of libtest cases.
    TestGroup,
    /// An in-process static check of the workspace's files or manifests.
    Static,
}

/// The claim class a check supports (§10.15). W05 adds the other classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimClass {
    /// Must hold on every execution; enforced by an assertion or checker.
    Invariant,
}

/// The gate tier that runs a check (§10.10). W05 adds the other tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Tier {
    /// T0, before every commit.
    T0,
}

/// How a check's cases are selected and executed.
#[derive(Clone, Copy, Debug)]
pub enum Selector {
    /// The libtest cases of `package` whose names contain any of `filters` (all cases
    /// if `filters` is empty).
    CargoTest {
        /// Cargo package that owns the tests.
        package: &'static str,
        /// Substrings; a case is selected if its name contains one of them.
        filters: &'static [&'static str],
    },
    /// An in-process static check over the workspace; it runs no tests.
    Static(StaticCheck),
}

/// The static checks xtask implements in-process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticCheck {
    /// No first-party `allow` of a CD lint; every other `allow` names its bead.
    LintAllows,
    /// Every crate inherits the workspace lints; deterministic crates carry the
    /// canonical deterministic configuration.
    LintTiers,
    /// Dependencies point inward only (CD-1, T-12).
    DepRule,
    /// Every direct third-party dependency is ledgered in DEPENDENCIES.md (CD-12).
    DepLedger,
    /// AGENTS.md carries every PLAN.md §11.5 rule verbatim (W01).
    AgentsRules,
}

impl StaticCheck {
    /// Runs the check against the workspace at `root`.
    pub fn run(self, root: &Path) -> Result<StaticOutcome, String> {
        match self {
            Self::LintAllows => crate::lint::check_allows(root),
            Self::LintTiers => crate::lint::check_tiers(root),
            Self::DepRule => crate::tiers::check_dependency_rule(root),
            Self::DepLedger => crate::ledger::check_ledger(root),
            Self::AgentsRules => crate::agents::check_agents_rules(root),
        }
    }

    /// The check's catalogue ID.
    pub const fn id(self) -> &'static str {
        match self {
            Self::LintAllows => "LINT-ALLOWS",
            Self::LintTiers => "LINT-TIERS",
            Self::DepRule => "DEP-RULE",
            Self::DepLedger => "DEP-LEDGER",
            Self::AgentsRules => "AGENTS-RULES",
        }
    }

    /// What one case is.
    pub const fn unit(self) -> &'static str {
        match self {
            Self::LintAllows => "source file(s)",
            Self::LintTiers => "crate(s)",
            Self::DepRule => "crate(s)+edge(s)",
            Self::DepLedger => "edge(s)+row(s)",
            Self::AgentsRules => "rule(s)+statement(s)",
        }
    }
}

/// What a static check examined, and what it found wrong.
#[derive(Debug)]
pub struct StaticOutcome {
    /// Items examined (files, crates, dependencies); zero is a failure (P10).
    pub cases: usize,
    /// One message per violation, each naming its location.
    pub violations: Vec<String>,
}

/// One registered check.
#[derive(Clone, Copy, Debug)]
pub struct CheckDef {
    /// Registry ID, e.g. `DEV-DOCTOR`.
    pub id: &'static str,
    /// One line on what it verifies.
    pub summary: &'static str,
    /// What sort of verification it is.
    pub kind: Kind,
    /// Requirement and principle IDs it supports.
    pub covers: &'static [&'static str],
    /// The claim class of its evidence.
    pub class: ClaimClass,
    /// The tier that runs it.
    pub tier: Tier,
    /// How its cases are selected and run.
    pub selector: Selector,
    /// Fewest cases a full (non-`--case`) run must execute. Never zero.
    pub min_cases: u32,
}

/// The built-in bootstrap catalogue (until W05's `registry/checks.toml`).
pub const CATALOGUE: &[CheckDef] = &[
    CheckDef {
        id: "XTASK",
        summary: "the whole xtask test suite (the bootstrap gate's xtask tests, §10.10)",
        kind: Kind::TestGroup,
        covers: &["P7", "P10", "O-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &[],
        },
        min_cases: 40,
    },
    CheckDef {
        id: "DEV-DOCTOR",
        summary: "./dev doctor reports each tool's version and fails on a missing one",
        kind: Kind::TestGroup,
        covers: &["O-8"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["doctor::"],
        },
        min_cases: 10,
    },
    CheckDef {
        id: "DEV-CHECK",
        summary: "./dev check fails unknown IDs, zero-case selections and ignored cases",
        kind: Kind::TestGroup,
        covers: &["P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["check::"],
        },
        min_cases: 6,
    },
    CheckDef {
        id: "LINT-PROBES",
        summary: "every denied lint and determinism entry fires on a probe; controls pass",
        kind: Kind::TestGroup,
        covers: &["CD-5", "CD-8", "CD-11", "P4", "T-1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["lint::probes::"],
        },
        min_cases: 19,
    },
    CheckDef {
        id: "LINT-ALLOWS",
        summary: "no first-party allow of a CD lint; every other allow names its bead",
        kind: Kind::Static,
        covers: &["CD-5", "CD-8", "CD-11", "P4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::LintAllows),
        min_cases: 10,
    },
    CheckDef {
        id: "LINT-TIERS",
        summary: "every crate inherits the workspace lints; deterministic crates carry the \
                  canonical deterministic configuration",
        kind: Kind::Static,
        covers: &["CD-5", "CD-11", "P4", "T-1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::LintTiers),
        min_cases: 4,
    },
    CheckDef {
        id: "DEP-RULE",
        summary: "dependencies point inward only; oracles depend on no production crate",
        kind: Kind::Static,
        covers: &["CD-1", "P3", "T-12", "P4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::DepRule),
        min_cases: 4,
    },
    CheckDef {
        id: "DEP-LEDGER",
        summary: "every direct dependency is in DEPENDENCIES.md at its locked version",
        kind: Kind::Static,
        covers: &["CD-12"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::DepLedger),
        min_cases: 5,
    },
    CheckDef {
        id: "AGENTS-RULES",
        summary: "AGENTS.md carries every PLAN.md §11.5 rule verbatim and replaces the \
                  ACFS git checklist",
        kind: Kind::Static,
        covers: &["P7", "P9", "CD-12"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::AgentsRules),
        min_cases: 18,
    },
    CheckDef {
        id: "DEV-GATE",
        summary: "./dev gate judges an exact tree, fails vacuous steps and writes a receipt",
        kind: Kind::TestGroup,
        covers: &["P7", "P10", "P11"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["gate::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "DEV-STEPLOG",
        summary: "scripted flows log expected/observed/rerun step records; skips fail",
        kind: Kind::TestGroup,
        covers: &["P10", "T-4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["steplog::"],
        },
        min_cases: 6,
    },
    CheckDef {
        id: "DEV-COMMIT",
        summary: "./dev commit gates and commits exactly the staged tree of the named paths",
        kind: Kind::TestGroup,
        covers: &["P11"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["commit::", "candidate::", "git::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "DEV-ACCEPT",
        summary: "./dev accept runs a bead's selectors on a committed tree in its own dir",
        kind: Kind::TestGroup,
        covers: &["P11", "P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["accept::", "tracker::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "DEV-CLOSE",
        summary: "closure needs a receipt for the work tree; activation waits for the last \
                  canonical parent; a failed spike stays open",
        kind: Kind::TestGroup,
        covers: &["P11", "P5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["closure::", "registry::"],
        },
        min_cases: 12,
    },
    CheckDef {
        id: "INVARIANT",
        summary: "invariant! holds silently and panics with its ID and context (CD-7)",
        kind: Kind::TestGroup,
        covers: &["CD-7", "CD-8"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["invariant"],
        },
        min_cases: 3,
    },
];

/// Outcome of one requested check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Ran and met every condition.
    Passed,
    /// Ran (or tried to) and did not pass.
    Failed,
    /// Not a registered check ID; nothing ran.
    Unknown,
    /// Registered, but not run because another requested ID was unknown.
    NotRun,
}

/// What happened to one requested check.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    /// The requested ID.
    pub id: String,
    /// Outcome.
    pub status: CheckStatus,
    /// What it verifies (registered checks only).
    pub summary: Option<&'static str>,
    /// Registry kind.
    pub kind: Option<Kind>,
    /// Registry tier.
    pub tier: Option<Tier>,
    /// Claim class.
    pub class: Option<ClaimClass>,
    /// Requirement and principle IDs it supports.
    pub covers: &'static [&'static str],
    /// The registered floor on executed cases.
    pub min_cases: Option<u32>,
    /// Cases the selection matched.
    pub selected: u32,
    /// Cases that ran and passed.
    pub passed: u32,
    /// Cases that ran and failed.
    pub failed: u32,
    /// Selected cases skipped by `#[ignore]` (a failure, P10).
    pub ignored: u32,
    /// Names of the failed cases.
    pub failures: Vec<String>,
    /// Exact commands that re-run this check and each failed case.
    pub rerun: Vec<String>,
    /// The cargo command lines executed.
    pub commands: Vec<String>,
    /// Wall-clock time spent.
    pub duration_ms: u64,
    /// Why the check did not pass, when it did not.
    pub detail: Option<String>,
    /// Tail of the captured output of a failed run (human mode only).
    #[serde(skip)]
    pub output_tail: Option<String>,
}

impl CheckResult {
    fn new(id: &str, def: Option<&CheckDef>, status: CheckStatus) -> Self {
        Self {
            id: id.to_owned(),
            status,
            summary: def.map(|d| d.summary),
            kind: def.map(|d| d.kind),
            tier: def.map(|d| d.tier),
            class: def.map(|d| d.class),
            covers: def.map_or(&[], |d| d.covers),
            min_cases: def.map(|d| d.min_cases),
            selected: 0,
            passed: 0,
            failed: 0,
            ignored: 0,
            failures: Vec::new(),
            rerun: Vec::new(),
            commands: Vec::new(),
            duration_ms: 0,
            detail: None,
            output_tail: None,
        }
    }

    fn fail(&mut self, detail: String) {
        self.status = CheckStatus::Failed;
        self.detail = Some(detail);
    }
}

/// The `./dev check` report (`keyval.dev.check/v1`).
#[derive(Debug, Serialize)]
pub struct CheckReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when every requested check passed.
    pub ok: bool,
    /// The `--case` filter, if one was given.
    pub case: Option<String>,
    /// How cargo was launched.
    pub runner: crate::cargo::Runner,
    /// One entry per requested ID, in request order.
    pub checks: Vec<CheckResult>,
}

/// Runs the checks named by `ids` from `catalogue`, in order, against the workspace at
/// `root`. If any ID is unknown, nothing runs.
pub fn run(
    root: &Path,
    catalogue: &[CheckDef],
    ids: &[String],
    case: Option<&str>,
    cargo: &Cargo,
) -> CheckReport {
    let resolved: Vec<(&String, Option<&CheckDef>)> = ids
        .iter()
        .map(|id| (id, catalogue.iter().find(|def| def.id == id)))
        .collect();
    let any_unknown = ids.is_empty() || resolved.iter().any(|(_, def)| def.is_none());
    let checks: Vec<CheckResult> = resolved
        .into_iter()
        .map(|(id, def)| match def {
            None => {
                let mut result = CheckResult::new(id, None, CheckStatus::Unknown);
                let known: Vec<&str> = catalogue.iter().map(|d| d.id).collect();
                result.detail = Some(format!(
                    "unknown check ID; registered: {}",
                    known.join(", ")
                ));
                result
            }
            Some(def) if any_unknown => {
                let mut result = CheckResult::new(id, Some(def), CheckStatus::NotRun);
                result.detail = Some("not run: another requested ID is unknown".to_owned());
                result
            }
            Some(def) => run_one(root, def, case, cargo),
        })
        .collect();
    CheckReport {
        schema: SCHEMA,
        ok: !checks.is_empty() && checks.iter().all(|c| c.status == CheckStatus::Passed),
        case: case.map(str::to_owned),
        runner: cargo.runner(),
        checks,
    }
}

/// Runs one registered check.
pub fn run_one(root: &Path, def: &CheckDef, case: Option<&str>, cargo: &Cargo) -> CheckResult {
    let started = Instant::now();
    let mut result = CheckResult::new(def.id, Some(def), CheckStatus::Failed);
    match def.selector {
        Selector::CargoTest { package, filters } => {
            run_cargo_test(&mut result, root, def, package, filters, case, cargo);
        }
        Selector::Static(check) => run_static(&mut result, root, def, check, case),
    }
    result.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if result.status != CheckStatus::Passed {
        result.rerun.insert(0, rerun_command(def.id, case));
    }
    result
}

fn run_cargo_test(
    result: &mut CheckResult,
    root: &Path,
    def: &CheckDef,
    package: &str,
    filters: &[&str],
    case: Option<&str>,
    cargo: &Cargo,
) {
    // 1. Resolve the selection statically, without running anything (§10.13 cond. 4).
    // `--all-targets` leaves out doctests in both steps: rustdoc splits test arguments on
    // whitespace, so a doctest name ("src/x.rs - f (line 3)") can never be selected
    // exactly. Doctests run in the gate's whole-workspace test step instead.
    let list_args = [
        "test",
        "--locked",
        "-p",
        package,
        "--all-targets",
        "--",
        "--list",
    ];
    result.commands.push(cargo.describe(&list_args));
    let listed = match execute(cargo, root, &list_args) {
        Ok(captured) => captured,
        Err(why) => return result.fail(format!("listing the cases failed: {why}")),
    };
    let selected: Vec<String> = libtest::parse_list(&listed.stdout)
        .into_iter()
        .filter(|name| filters.is_empty() || filters.iter().any(|f| name.contains(f)))
        .filter(|name| case.is_none_or(|c| name.contains(c)))
        .collect();
    result.selected = count(selected.len());
    if selected.is_empty() {
        return result.fail(format!(
            "the selection matched zero cases (filters {filters:?}, case {case:?}); \
             a zero-case selection is a failure, never a pass (P10)"
        ));
    }

    // 2. Run exactly the selected cases.
    let mut run_args: Vec<&str> = vec![
        "test",
        "--locked",
        "-p",
        package,
        "--all-targets",
        "--",
        "--exact",
    ];
    run_args.extend(selected.iter().map(String::as_str));
    result.commands.push(cargo.describe(&run_args));
    let ran = match execute_allowing_failure(cargo, root, &run_args) {
        Ok(captured) => captured,
        Err(why) => return result.fail(format!("running the cases failed: {why}")),
    };
    let tally = match libtest::parse_tally(&ran.stdout) {
        Ok(tally) => tally,
        Err(err) => return result.fail(err.to_string()),
    };
    result.passed = tally.passed;
    result.failed = tally.failed;
    result.ignored = tally.ignored;
    result.failures = libtest::failed_names(&ran.stdout);
    result.rerun.extend(
        result
            .failures
            .iter()
            .map(|name| rerun_command(def.id, Some(name))),
    );
    let min_cases = if case.is_some() { 1 } else { def.min_cases };
    match verdict(result.selected, tally, ran.status.success(), min_cases) {
        Ok(()) => result.status = CheckStatus::Passed,
        Err(why) => {
            result.output_tail = Some(tail(&ran.stdout, &ran.stderr));
            result.fail(why);
        }
    }
}

fn run_static(
    result: &mut CheckResult,
    root: &Path,
    def: &CheckDef,
    check: StaticCheck,
    case: Option<&str>,
) {
    if case.is_some() {
        return result.fail(
            "--case selects test cases; a static check has none to select, so run it whole"
                .to_owned(),
        );
    }
    let outcome = match check.run(root) {
        Ok(outcome) => outcome,
        Err(why) => return result.fail(format!("the check could not run: {why}")),
    };
    result.selected = count(outcome.cases);
    if !outcome.violations.is_empty() {
        result.failed = count(outcome.violations.len());
        result.failures = outcome.violations;
        return result.fail(format!(
            "{} violation(s) among {} case(s) examined",
            result.failed, result.selected
        ));
    }
    result.passed = result.selected;
    let tally = Tally {
        passed: result.passed,
        failed: 0,
        ignored: 0,
    };
    match verdict(result.selected, tally, true, def.min_cases) {
        Ok(()) => result.status = CheckStatus::Passed,
        Err(why) => result.fail(why),
    }
}

/// Whether a run of `selected` cases that produced `tally` passes. `exit_ok` is the
/// harness exit status; `min_cases` the floor on executed cases.
pub fn verdict(selected: u32, tally: Tally, exit_ok: bool, min_cases: u32) -> Result<(), String> {
    if tally.failed > 0 {
        return Err(format!("{} of {selected} case(s) failed", tally.failed));
    }
    if tally.ignored > 0 {
        return Err(format!(
            "{} selected case(s) are #[ignore]d; an ignored required case fails (P10)",
            tally.ignored
        ));
    }
    if !exit_ok {
        return Err("the test harness exited unsuccessfully".to_owned());
    }
    if tally.passed != selected {
        return Err(format!(
            "{selected} case(s) were selected but {} ran",
            tally.passed
        ));
    }
    if tally.passed < min_cases {
        return Err(format!(
            "only {} case(s) ran; this check requires at least {min_cases}",
            tally.passed
        ));
    }
    Ok(())
}

fn execute(cargo: &Cargo, root: &Path, args: &[&str]) -> Result<Captured, String> {
    let captured = execute_allowing_failure(cargo, root, args)?;
    if captured.status.success() {
        Ok(captured)
    } else {
        Err(format!(
            "{} exited with {}\n{}",
            cargo.describe(args),
            captured.status,
            tail(&captured.stdout, &captured.stderr)
        ))
    }
}

fn execute_allowing_failure(cargo: &Cargo, root: &Path, args: &[&str]) -> Result<Captured, String> {
    process::run(&mut cargo.command(root, args), RUN_LIMIT)
        .map_err(|err| format!("{}: {err}", cargo.describe(args)))
}

fn rerun_command(id: &str, case: Option<&str>) -> String {
    case.map_or_else(
        || format!("./dev check {id}"),
        |case| format!("./dev check {id} --case {case}"),
    )
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn tail(stdout: &str, stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().chain(stdout.lines()).collect();
    let skip = lines.len().saturating_sub(FAILURE_TAIL_LINES);
    let mut out = String::new();
    if skip > 0 {
        let _ = writeln!(out, "[{skip} earlier line(s) omitted]");
    }
    for line in lines.iter().skip(skip) {
        let _ = writeln!(out, "{line}");
    }
    out
}

impl Report for CheckReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut out = String::new();
        for check in &self.checks {
            let seconds = Duration::from_millis(check.duration_ms).as_secs_f64();
            let _ = match check.status {
                CheckStatus::Passed => writeln!(
                    out,
                    "check {}: passed ({} case(s), {}) in {seconds:.1}s",
                    check.id,
                    check.passed,
                    // A --case selection needs one case, not the full-run floor.
                    self.case.as_ref().map_or_else(
                        || format!("min {}", check.min_cases.unwrap_or(0)),
                        |case| format!("--case {case:?}")
                    )
                ),
                CheckStatus::Failed | CheckStatus::Unknown | CheckStatus::NotRun => writeln!(
                    out,
                    "check {}: {} - {}",
                    check.id,
                    match check.status {
                        CheckStatus::Unknown => "UNKNOWN",
                        CheckStatus::NotRun => "NOT RUN",
                        CheckStatus::Failed | CheckStatus::Passed => "FAILED",
                    },
                    check.detail.as_deref().unwrap_or("")
                ),
            };
            for name in &check.failures {
                let _ = writeln!(out, "  failed: {name}");
            }
            for command in &check.rerun {
                let _ = writeln!(out, "  rerun:  {command}");
            }
            if let Some(tail) = &check.output_tail {
                let _ = writeln!(out, "  --- output tail ---\n{tail}  --- end ---");
            }
        }
        out.push_str(if self.ok {
            "check: ok"
        } else {
            "check: FAILED"
        });
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-CHECK
    use super::*;
    use std::path::PathBuf;

    fn tally(passed: u32, failed: u32, ignored: u32) -> Tally {
        Tally {
            passed,
            failed,
            ignored,
        }
    }

    #[test]
    fn verdict_requires_every_selected_case_to_pass() {
        let cases = [
            (
                (3, tally(3, 0, 0), true, 3),
                true,
                "all selected cases passed",
            ),
            ((3, tally(2, 1, 0), false, 1), false, "a case failed"),
            (
                (3, tally(2, 0, 1), true, 1),
                false,
                "an ignored case is a failure",
            ),
            (
                (3, tally(3, 0, 0), false, 1),
                false,
                "harness exit status failed",
            ),
            (
                (3, tally(2, 0, 0), true, 1),
                false,
                "fewer ran than were selected",
            ),
            ((3, tally(3, 0, 0), true, 4), false, "below min_cases"),
            (
                (0, tally(0, 0, 0), true, 1),
                false,
                "zero cases is never a pass",
            ),
        ];
        for ((selected, t, exit_ok, min), should_pass, rule) in cases {
            let observed = verdict(selected, t, exit_ok, min);
            assert_eq!(
                observed.is_ok(),
                should_pass,
                "rule: {rule}; selected={selected} tally={t:?} exit_ok={exit_ok} \
                 min_cases={min} observed={observed:?}"
            );
        }
    }

    #[test]
    fn catalogue_ids_are_unique_and_min_cases_nonzero() {
        for (i, def) in CATALOGUE.iter().enumerate() {
            assert!(
                def.min_cases > 0,
                "rule: min_cases is never zero; id={}",
                def.id
            );
            assert!(
                CATALOGUE.iter().skip(i + 1).all(|other| other.id != def.id),
                "rule: check IDs are unique; duplicate id={}",
                def.id
            );
        }
    }

    #[test]
    fn every_catalogue_check_has_exactly_one_code_marker() {
        let root = crate::test_support::workspace_root();
        let sources: Vec<String> = crate::lint::first_party_sources(&root)
            .unwrap()
            .iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect();
        for def in CATALOGUE {
            let marker = format!("// check: {}\n", def.id);
            let found: usize = sources.iter().map(|s| s.matches(&marker).count()).sum();
            assert_eq!(
                found, 1,
                "rule: each check declares its ID in code exactly once (§10.13 cond. 3); \
                 marker={marker:?} found={found}"
            );
        }
    }

    #[test]
    fn an_unknown_id_runs_nothing_and_fails() {
        let ids = ["DEV-CHECK".to_owned(), "NOPE".to_owned()];
        let report = run(
            Path::new("/nonexistent"),
            CATALOGUE,
            &ids,
            None,
            &Cargo::local(),
        );
        assert!(!report.ok, "observed={report:?}");
        let statuses: Vec<CheckStatus> = report.checks.iter().map(|c| c.status).collect();
        assert_eq!(
            statuses,
            [CheckStatus::NotRun, CheckStatus::Unknown],
            "rule: unknown IDs are rejected before anything runs"
        );
    }

    #[test]
    fn an_empty_request_fails() {
        let report = run(
            Path::new("/nonexistent"),
            CATALOGUE,
            &[],
            None,
            &Cargo::local(),
        );
        assert!(!report.ok, "rule: a request for no checks is not a pass");
    }

    // --- Runs against a fixture crate with real cargo. ---

    const FIXTURE_PACKAGE: &str = "libtest-fixture";

    fn fixture_root() -> PathBuf {
        crate::test_support::manifest_dir().join("tests/fixtures/libtest-fixture")
    }

    fn nested_cargo() -> Cargo {
        crate::test_support::nested_cargo("keyval-nested")
    }

    fn fixture_check(filter: &'static [&'static str], min_cases: u32) -> CheckDef {
        CheckDef {
            id: "FIXTURE",
            summary: "fixture",
            kind: Kind::TestGroup,
            covers: &[],
            class: ClaimClass::Invariant,
            tier: Tier::T0,
            selector: Selector::CargoTest {
                package: FIXTURE_PACKAGE,
                filters: filter,
            },
            min_cases,
        }
    }

    fn run_fixture(
        filter: &'static [&'static str],
        case: Option<&str>,
        min_cases: u32,
    ) -> CheckResult {
        let def = fixture_check(filter, min_cases);
        run_one(&fixture_root(), &def, case, &nested_cargo())
    }

    #[test]
    fn fixture_passing_selection_passes_with_exact_counts() {
        let result = run_fixture(&["passes_"], None, 2);
        assert_eq!(result.status, CheckStatus::Passed, "observed={result:#?}");
        assert_eq!(
            (result.selected, result.passed),
            (2, 2),
            "observed={result:#?}"
        );
    }

    #[test]
    fn fixture_zero_case_selection_fails_without_running() {
        let result = run_fixture(&["passes_"], Some("no_such_case"), 1);
        assert_eq!(result.status, CheckStatus::Failed, "observed={result:#?}");
        assert_eq!(result.selected, 0);
        assert_eq!(
            result.commands.len(),
            1,
            "rule: an empty selection is rejected after listing, before running; \
             observed={result:#?}"
        );
    }

    #[test]
    fn fixture_failing_case_fails_and_names_a_rerun_command() {
        let result = run_fixture(&["fails_"], None, 1);
        assert_eq!(result.status, CheckStatus::Failed, "observed={result:#?}");
        assert_eq!(
            result.failures,
            ["tests::fails_on_purpose"],
            "observed={result:#?}"
        );
        assert!(
            result
                .rerun
                .contains(&"./dev check FIXTURE --case tests::fails_on_purpose".to_owned()),
            "rule: a failure prints the exact re-run command (W01.6); observed={result:#?}"
        );
    }

    #[test]
    fn fixture_ignored_case_fails() {
        let result = run_fixture(&["ignored_"], None, 1);
        assert_eq!(result.status, CheckStatus::Failed, "observed={result:#?}");
        assert_eq!(result.ignored, 1, "observed={result:#?}");
    }

    #[test]
    fn fixture_selection_below_min_cases_fails() {
        let result = run_fixture(&["passes_"], None, 3);
        assert_eq!(result.status, CheckStatus::Failed, "observed={result:#?}");
        assert!(
            result
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("at least 3")),
            "observed={result:#?}"
        );
    }
}
