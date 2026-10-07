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
pub const SCHEMA: &str = "keyval.dev.check/v2";

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
    /// A checker of §10.5 (K-…): libtest cases that judge a property of runs.
    Checker,
}

/// The claim class a check supports (§10.15). W05 adds the other classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimClass {
    /// Must hold on every execution; enforced by an assertion or checker.
    Invariant,
    /// No violation in N runs of a stated build: evidence, not proof.
    SimulationEvidenced,
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
    /// The vendored Raft library matches VENDOR.toml and its codegen stays pinned (W02).
    RaftVendor,
    /// The vendored Raft library keeps the W02.2 audit's verdicts: no clocks, threads,
    /// unexpected entropy, randomly seeded std hash collections or pointer formatting.
    RaftAudit,
    /// `api/openapi.yaml` meets PLAN.md §8, judged with the fixtures in `api/fixtures/` (W03.3).
    Spec,
    /// registry/workgraph.toml mirrors PLAN.md §12.2/§12.3 and is acyclic (W06).
    Workgraph,
    /// The verification registry agrees with the plan, the code, the spec, the tests
    /// and the work graph (§10.13; W05.2).
    Registry,
    /// The Raft spike's traces are byte-identical across fresh processes (W02.4).
    SpikeDeterminism,
    /// Every audit report under `reports/audits/` names its tree and commit, and each of
    /// its findings names a bead the tracker export holds (§11.9, W20).
    AuditReports,
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
            Self::RaftVendor => crate::vendor::check_vendor(root),
            Self::RaftAudit => crate::raft_audit::check_raft_audit(root),
            Self::Spec => crate::spec::check_spec(root),
            Self::Workgraph => crate::workgraph::check_workgraph(root),
            Self::Registry => {
                crate::registry_gate::check_registry(root).map(|verdict| StaticOutcome {
                    cases: verdict.cases,
                    violations: verdict.violations,
                })
            }
            Self::SpikeDeterminism => crate::spike::check_spike(root),
            Self::AuditReports => crate::audit_reports::check_audit_reports(root),
        }
    }

    /// [`StaticCheck::run`], plus the findings a report-mode condition lists without
    /// failing (REGISTRY's conditions 5 and 6 until W32).
    fn run_reporting(self, root: &Path) -> Result<(StaticOutcome, Vec<String>), String> {
        if self == Self::Registry {
            let verdict = crate::registry_gate::check_registry(root)?;
            let outcome = StaticOutcome {
                cases: verdict.cases,
                violations: verdict.violations,
            };
            Ok((outcome, verdict.reported))
        } else {
            self.run(root).map(|outcome| (outcome, Vec::new()))
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
            Self::RaftVendor => "RAFT-VENDOR",
            Self::RaftAudit => "RAFT-AUDIT",
            Self::Spec => "SPEC",
            Self::Workgraph => "WORKGRAPH",
            Self::Registry => "REGISTRY",
            Self::SpikeDeterminism => "SPIKE-DETERMINISM",
            Self::AuditReports => "AUDIT-REPORTS",
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
            Self::RaftVendor => "file(s)+assertion(s)",
            Self::RaftAudit => "file(s)+rule(s)",
            Self::Spec => "rule(s)+fixture(s)+mutant(s)",
            Self::Workgraph => "field(s)+edge(s)+rule(s)",
            Self::Registry => "row(s)+marker(s)+edge(s)",
            Self::SpikeDeterminism => "seed-process run(s)+control",
            Self::AuditReports => "header(s)+finding(s)+bead(s)",
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
        id: "RAFT-VENDOR",
        summary: "vendor/raft matches VENDOR.toml file for file, and raft-proto's code \
                  generation stays pinned (vendored output, no build script)",
        kind: Kind::Static,
        covers: &["P4", "T-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::RaftVendor),
        min_cases: 83,
    },
    CheckDef {
        id: "RAFT-AUDIT",
        summary: "vendored raft-rs keeps the W02.2 audit's verdicts outside test code: no \
                  unexpected entropy, clock, thread, randomly seeded std hash collection or \
                  pointer formatting",
        kind: Kind::Static,
        covers: &["P4", "T-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::RaftAudit),
        min_cases: 32,
    },
    CheckDef {
        id: "SPEC",
        summary: "api/openapi.yaml meets PLAN.md §8 (OpenAPI 3.1, operations, codes, no-store, \
                  byte limits, §8.6 examples); instance fixtures hold; every mutant breaks its rule",
        kind: Kind::Static,
        covers: &[
            "F-1", "F-2", "F-3", "F-4", "F-5", "F-6", "F-7", "F-8", "F-9", "F-10", "O-1",
        ],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::Spec),
        min_cases: 174,
    },
    CheckDef {
        id: "WORKGRAPH",
        summary: "registry/workgraph.toml mirrors PLAN.md §12.2/§12.3 (id, deps, reqs, area) \
                  and is acyclic (§10.13 condition 8, plan half)",
        kind: Kind::Static,
        covers: &["G1", "P5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::Workgraph),
        min_cases: 501,
    },
    CheckDef {
        id: "DEV-BEADS",
        summary: "./dev beads import adopts or creates one bead per work item, mirrors only \
                  work-item edges, records bead IDs, refuses drift and is idempotent",
        kind: Kind::TestGroup,
        covers: &["G1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["beads::", "workgraph::"],
        },
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
        id: "GATE-STAGING",
        summary: "./dev gate fast runs the implemented T0 rows of registry/checks.toml, judging \
                  cargo-test checks from one test run per package against min_cases, reports \
                  planned rows with their work item, and refuses a check-status demotion \
                  without a closed plan-change bead (§10.10, §10.11, D-56)",
        kind: Kind::TestGroup,
        covers: &["P5", "P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &[
                "staging::",
                "gate::tests::registry_",
                "libtest::tests::outcomes",
                "commit::tests::the_wrapper_hands_the_gate",
            ],
        },
        min_cases: 12,
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
        min_cases: 23,
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
        min_cases: 8,
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
        id: "REGISTRY-FILES",
        summary: "registry/requirements.toml equals PLAN.md §6 with derived statuses; \
                  registry/checks.toml mirrors the catalogue row for row, in order, and every \
                  other row is a planned check with a floor that runs its own ID and is named \
                  by an accept list as staging matches it; a broken fixture fails each rule",
        kind: Kind::TestGroup,
        covers: &["P5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["registry::files::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "REGISTRY",
        summary: "the verification registry agrees with the plan, the code, the spec, the \
                  tests and the work graph (§10.13 conditions 1-8; 5-6 reported until W32)",
        kind: Kind::Static,
        covers: &["P5", "P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::Registry),
        min_cases: 923,
    },
    CheckDef {
        id: "REGISTRY-FIXTURES",
        summary: "each §10.13 registry gate condition has a fixture that makes it fail, and \
                  malformed rows fail whatever their status",
        kind: Kind::TestGroup,
        covers: &["P5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["registry_gate::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "TRACEABILITY",
        summary: "./dev report traceability renders every requirement with its derived status, \
                  covering checks, latest receipts and pass on a tree",
        kind: Kind::TestGroup,
        covers: &["P5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["traceability::"],
        },
        min_cases: 6,
    },
    CheckDef {
        id: "DEV-PRUNE",
        summary: "./dev prune keeps the newest accept trees and every failing run, removes \
                  the rest with their target dirs, and logs every removal",
        kind: Kind::TestGroup,
        covers: &["CD-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["prune::", "cargo::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "DEV-SCHEMAS",
        summary: "every versioned ./dev JSON output matches its schema in schemas/dev/, and \
                  each schema rejects its negative fixture",
        kind: Kind::TestGroup,
        covers: &["O-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["schemas::"],
        },
        min_cases: 19,
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
    CheckDef {
        id: "NEWTYPES",
        summary: "each kind of quantity has its own type; ranges (D-77, F-4), checked arithmetic \
                  and explicit conversions hold at their edges",
        kind: Kind::TestGroup,
        covers: &["CD-3", "CD-4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["counts::", "ids::", "log_index::", "range::"],
        },
        min_cases: 13,
    },
    CheckDef {
        id: "KEY-VALUE",
        summary: "Key::parse and Value::parse enforce DM-1/DM-2 in decoded UTF-8 bytes; keys are \
                  stored exactly and order byte-wise (DM-4)",
        kind: Kind::TestGroup,
        covers: &["DM-1", "DM-2", "DM-4", "CD-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["data::"],
        },
        min_cases: 5,
    },
    CheckDef {
        id: "DOMAIN-ENUMS",
        summary: "lifecycle, CAS expectation, mutation, decision and attempt states are \
                  enums whose invalid combinations cannot be built (CD-2)",
        kind: Kind::TestGroup,
        covers: &["CD-2", "CD-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["lifecycle::", "mutation::", "outcome::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "ERROR-CODES",
        summary: "ErrorCode equals api/openapi.yaml's error codes in order, with the §8.4 \
                  statuses and terminality",
        kind: Kind::TestGroup,
        covers: &["CD-6", "F-10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["error_code::"],
        },
        min_cases: 3,
    },
    CheckDef {
        id: "PORTS",
        summary: "the ports are core-owned types: inbound step -> ordered effects, stale \
                  completions recognisable, bounded in-process reads, a library-neutral \
                  Consensus port (§7.2, §7.3)",
        kind: Kind::TestGroup,
        covers: &["CD-1", "T-1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &[
                "client::",
                "consensus::",
                "effects::",
                "in_process::",
                "telemetry::",
            ],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "COMPILE-FAIL",
        summary: "forging a types value past its constructor, or comparing a Version with a \
                  LogIndex, does not compile (CD-2, CD-3, CD-4)",
        kind: Kind::TestGroup,
        covers: &["CD-2", "CD-3", "CD-4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["lint::probes::compile_fail::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "DEP-RULE-FIXTURES",
        summary: "the dependency rule rejects each forbidden edge, including a deterministic \
                  crate on an adapter crate (CD-1, T-12)",
        kind: Kind::TestGroup,
        covers: &["CD-1", "T-12"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["tiers::tests::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "SPIKE-DETERMINISM",
        summary: "the Raft spike's seeded 3-node loop gives byte-identical traces for 100 seeds \
                  x 3 fresh processes, with crash-restart and re-election, and a wall-clock \
                  control diverges (§7.3, W02)",
        kind: Kind::Static,
        covers: &["P4", "T-3", "T-11"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::SpikeDeterminism),
        min_cases: 301,
    },
    CheckDef {
        id: "SIM-SCHEDULER",
        summary: "the simulator orders events by virtual time, seeded tie priority and \
                  insertion sequence; entropy streams split per subsystem and per node \
                  incarnation never shift each other; an exhausted event budget is \
                  incomplete (§10.2)",
        kind: Kind::TestGroup,
        covers: &["P4", "T-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["entropy::", "scheduler::"],
        },
        min_cases: 18,
    },
    CheckDef {
        id: "SIM-TRACE",
        summary: "the canonical trace keeps logical endpoints and bytes in fixed formats, \
                  excludes host data, fails closed when required observations overflow their \
                  bound, and compares completely; the god view records every node's Raft, \
                  journal and applied state (§10.2)",
        kind: Kind::TestGroup,
        covers: &["T-3", "T-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["trace::", "god_view::"],
        },
        min_cases: 24,
    },
    CheckDef {
        id: "K-14",
        summary: "replay identity: the same binary running the same seed in fresh OS processes \
                  gives byte-identical canonical traces and event counts (T0: 2 seeds x 2 \
                  processes); a mismatch fails, reporting its first differing event and \
                  keeping both traces, and a changing-counter control is caught (§10.5, §10.7)",
        kind: Kind::Checker,
        covers: &["T-3", "T-11", "P4"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["replay::"],
        },
        min_cases: 12,
    },
    CheckDef {
        id: "CONFIG-RULES",
        summary: "the config parser refuses each §7.10 inconsistency with a specific error, reads only \
                  its strict grammar, and accepts config/local.toml and every \
                  config/fixtures file (O-5, D-78, D-79)",
        kind: Kind::TestGroup,
        covers: &["O-5", "CD-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["config::tests::"],
        },
        min_cases: 35,
    },
    CheckDef {
        id: "CONFIG-FINGERPRINT",
        summary: "the semantic-config fingerprint covers exactly the semantic limits, the command format, \
                  the SCAN caps and the sorted voter set, and its in-crate SHA-256 matches \
                  FIPS 180-4 (DUR-8, D-75)",
        kind: Kind::TestGroup,
        covers: &["DUR-8"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["config::fingerprint::tests::", "sha256::tests::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "LIMITS",
        summary: "the limits registry names every §7.11 bound once and matches the API spec; bounded \
                  queues and maps refuse past their limits at N-1/N/N+1; a non-loopback address \
                  produces the O-9 warning (CD-9, O-9)",
        kind: Kind::TestGroup,
        covers: &["CD-9", "O-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &[
                "limits::tests::",
                "bounded::tests::",
                "config::exposure::tests::",
            ],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "JOURNAL-FRAMES",
        summary: "journal frames follow the §7.6 layout with header and frame CRC-32C (in-crate, RFC \
                  3720 vectors); every damaged field is refused; genesis and Raft-batch \
                  goldens decode and re-encode byte for byte (DUR-5)",
        kind: Kind::TestGroup,
        covers: &["DUR-5", "CD-12"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "journal",
            filters: &["crc32c::tests::", "frame::tests::", "golden::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "JOURNAL-LOG",
        summary: "the logical log appends, truncates logically (never at or below the persisted \
                  commit), indexes and caches entries without the cache ever changing a \
                  result, refuses impossible histories, and only grows: no compaction, no \
                  rewrite, until the DM-7 hard stop; genesis verification refuses a \
                  conflicting start (DUR-4, DUR-8, CD-10)",
        kind: Kind::TestGroup,
        covers: &["DUR-4", "DUR-8", "CD-9", "CD-10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "journal",
            filters: &["log_tests::", "log::invariant_fixtures::"],
        },
        min_cases: 18,
    },
    CheckDef {
        id: "COMMAND-CODEC",
        summary: "the canonical command codec round-trips every command shape, accepts only \
                  canonical bytes, refuses malformed or oversized input with typed errors and \
                  never panics, applies the configured key and value limits, and every valid \
                  command fits 66 KiB (§7.5, §7.11, D-80)",
        kind: Kind::TestGroup,
        covers: &["DM-1", "DM-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["codec::tests::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "APPLY-PUT",
        summary: "apply sets a PUT's key with version = its execution index, an empty entry only \
                  advances the applied index, keys iterate in byte order, replay rebuilds the \
                  same state, PUT, DELETE and compare-and-swap all execute, and out-of-order or \
                  undecodable entries fire their invariants (§7.5, DM-3, D-10)",
        kind: Kind::TestGroup,
        covers: &["DM-3", "DM-4", "F-1", "F-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["apply::tests::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "REQUEST-HISTORY",
        summary: "the request history records each request ID's first execution (index, SHA-256 \
                  digest of the canonical command, terminal result) forever; the same command \
                  again replays the stored result and another command under the ID is a \
                  conflict, neither changing state; replay recomputes both exactly (§7.5 step 3, \
                  DM-5, C-6)",
        kind: Kind::TestGroup,
        covers: &["DM-5", "F-6", "C-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["history::tests::"],
        },
        min_cases: 5,
    },
    CheckDef {
        id: "JOURNAL-RECOVERY",
        summary: "recovery verifies the genesis, replays the frames into the live log, \
                  discards only a torn tail (at most one maximum frame, with no valid \
                  later frame anywhere after it, skipping an intact header's payload) \
                  and refuses corruption, impossible index or term relationships, a \
                  genesis mismatch and a blank start; every recovery golden journal \
                  recovers exactly as pinned (DUR-5, DUR-6, DUR-8, DUR-9, D-60, D-69)",
        kind: Kind::TestGroup,
        covers: &["DUR-5", "DUR-6", "DUR-8", "DUR-9", "CD-10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "journal",
            filters: &["recovery_tests::", "golden_recovery::"],
        },
        min_cases: 20,
    },
    CheckDef {
        id: "CAPACITY",
        summary: "the live-state quota admits N-1 and N and refuses N+1 for both live keys and \
                  decoded key+value bytes, checking keys first; equal and shrinking overwrites \
                  always pass; a refused write is terminal and replays; the \
                  committed-command-bytes counter counts first executions only and is identical \
                  after replay (DM-6, DM-7)",
        kind: Kind::TestGroup,
        covers: &["DM-6", "DM-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["capacity::tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "STATE-INVARIANTS",
        summary: "every state-machine invariant is armed: a corrupted-state fixture makes each \
                  fire with its ID, node and entry (applied beyond commit, a version that would \
                  not grow, live state over quota, counter underflow or overflow on an overwrite \
                  or a delete, history past the applied entries, a request executed twice) \
                  (CD-7)",
        kind: Kind::TestGroup,
        covers: &["CD-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["apply::invariant_fixtures::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "STATE-PROPERTIES",
        summary: "over seeded random logs with retries, conflicts and quota refusals: version = \
                  execution index, a replay returns the original result, a conflicting command \
                  changes nothing, a quota refusal is terminal and deterministic, a DELETE \
                  removes its key and reports whether it existed, a key's versions strictly \
                  increase across delete and re-create, and a compare-and-swap whose expectation \
                  fails is a terminal version_mismatch that changes nothing, decided strictly \
                  before capacity, while one whose expectation holds acts as a PUT; failures \
                  shrink and name their seed (DM-3, DM-5, DM-6, C-6)",
        kind: Kind::TestGroup,
        covers: &["DM-3", "DM-5", "DM-6", "C-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["properties::"],
        },
        min_cases: 5,
    },
    CheckDef {
        id: "MODEL",
        summary: "the reference model, written from the plan with no production dependency \
                  (T-12), applies committed entries in order: PUT and CAS versions are the \
                  execution index, DELETE reports existed, a CAS mismatch is decided before \
                  capacity, DM-6 key and byte capacity refusals are terminal, a replay returns \
                  the stored result and a conflict changes nothing; DM-7 history admission \
                  refuses a new write once the commit index reaches max_log_entries, changing \
                  nothing; SCAN pages follow §8.5 and §7.11 exactly (T-7, §10.14)",
        kind: Kind::TestGroup,
        covers: &["T-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "model",
            filters: &["apply::", "data::", "scan::"],
        },
        min_cases: 34,
    },
    CheckDef {
        id: "STORAGE-CONTRACT",
        summary: "the one adapter-agnostic byte-storage contract suite (a missing \
                  journal is Missing, create never clobbers, appends read back exactly, \
                  reads are never silently short, truncate shortens and never lengthens, \
                  synced state survives a clean restart, the lock refuses a second \
                  holder, and execute completes each sync with its own sequence and \
                  incarnation and reports failures as StorageFailed) passes on the \
                  in-memory reference and catches every broken reference (§7.2, P3)",
        kind: Kind::TestGroup,
        covers: &["P3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["contract_reference::", "contract_broken::"],
        },
        min_cases: 23,
    },
    CheckDef {
        id: "RAFT-ADAPTER",
        summary: "the raft-rs adapter's construction and Consensus port: every committed config \
                  constructs a RawNode for each voter (D-78), every setting reaches the library \
                  config, election timeouts come only from the seeded entropy port, entries, \
                  hard state and messages convert losslessly and unknown protobuf fields are \
                  refused (H-3), the journal serves the library's log reads, a Ready writes one \
                  frame exactly when it must sync, every public read answers while a frame \
                  syncs, and a restart resumes from the recovered hard state and hands over \
                  only the committed entries past its replayed index (§7.3, §7.4, §7.7, CD-1, \
                  P4)",
        kind: Kind::TestGroup,
        covers: &["CD-1", "P4", "O-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &[
                "config::tests::",
                "convert::tests::",
                "entropy::tests::",
                "storage::tests::",
                "node_tests::",
                "node::invariant_fixtures::",
            ],
        },
        min_cases: 38,
    },
    CheckDef {
        id: "STORAGE-CONTRACT-SIM",
        summary: "the simulated disk passes the one byte-storage contract suite \
                  unmodified, every case in its own test (§7.2, P3)",
        kind: Kind::TestGroup,
        covers: &["P3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["contract_sim_disk::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "SIM-DISK",
        summary: "the simulated disk's crash model: a seed-chosen prefix of unsynced \
                  appends survives (none, some or all) with an optional zero or garbage \
                  fill to the append's extent, synced bytes are never damaged, a file \
                  whose directory entry was never synced is lost, an unsynced truncation \
                  may be lost, a crash ends every handle and frees the lock, a lost sync \
                  completion is stale for the next incarnation, and the outcome is a \
                  function of the seed; a sync writes only its unsynced bytes, so the \
                  durable image equals the journal after every sync at linear cost \
                  (§10.2, §10.10, DUR-7, DUR-9)",
        kind: Kind::TestGroup,
        covers: &["DUR-7", "DUR-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["disk::tests::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "STORAGE-CONTRACT-FS",
        summary: "the real-file adapter passes the one byte-storage contract suite \
                  unmodified, every case in its own test (§7.2, P3)",
        kind: Kind::TestGroup,
        covers: &["P3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "fs_storage",
            filters: &["contract_fs::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "FS-STORAGE",
        summary: "the real-file adapter syncs the parent directory when it creates the data \
                  directory and again when it finds one already there (an interrupted init \
                  re-runs), a recorded OS-call trace shows mkdir then fsync of the parent, \
                  sync_dir an fsync of the data directory and sync an fdatasync of the journal, \
                  in the order the port calls arrive, its LOCK refuses a second process until \
                  the holder exits, a missing directory is Missing while one that cannot be \
                  examined is Failed and every call stays within its documented errors on real \
                  OS failures, a failed write, truncate or sync latches the handle so no later \
                  call reports success, and failures keep their cause for the root's log (§7.6, \
                  §7.7, §7.11, §10.17, DUR-7, DUR-8)",
        kind: Kind::TestGroup,
        covers: &["DUR-7", "DUR-8"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "fs_storage",
            filters: &["tests::"],
        },
        min_cases: 17,
    },
    CheckDef {
        id: "RAFT-READY-LOOP",
        summary: "the Ready loop persists, then releases: no vote grant, append acknowledgement \
                  or other message leaves before its sender has synced the term, vote and \
                  entries it depends on, and no entry applies before its frame is synced at the \
                  applying node with the term its synced log holds (scripted, seeded and \
                  lone-voter runs judged against each node's synced journal), a frame never \
                  persists a commit index alone, inputs wait while a sync is outstanding, an \
                  old-incarnation SyncDone is ignored, committed entries apply once each in \
                  index order and drain past the per-Ready bound before the next input, applied \
                  progress is reported, and a hard stop stops the loop (§7.4, DUR-2, K-11, D-28)",
        kind: Kind::TestGroup,
        covers: &["DUR-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &[
                "ready_loop_tests::",
                "ready_loop::invariant_fixtures::",
                "term_change_tests::",
            ],
        },
        min_cases: 22,
    },
    CheckDef {
        id: "JOURNAL-EXAMINE",
        summary: "the doctor's journal examination reads a journal exactly as startup \
                  recovery does and reaches recovery's verdict on every recovery golden, \
                  names the torn tail it would discard (incomplete, checksum-failing, \
                  unreadable or stale, with offset and length), keeps what was read \
                  before a refusal, and verifies the genesis binding only when given the \
                  configuration (O-7)",
        kind: Kind::TestGroup,
        covers: &["O-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "journal",
            filters: &["examine_tests::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "FS-DOCTOR",
        summary: "the doctor's read-only data-directory handle opens the journal \
                  read-only, creates and changes nothing (every file byte for byte), \
                  probes the lock without creating LOCK, reports a live directory as \
                  live, and holds the lock while it reads so no node can start meanwhile \
                  (O-7)",
        kind: Kind::TestGroup,
        covers: &["O-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "fs_storage",
            filters: &["doctor_handle::"],
        },
        min_cases: 4,
    },
    CheckDef {
        id: "DOCTOR-CLI",
        summary: "keyval doctor, run as the real binary on real data directories, \
                  reaches startup recovery's verdict on every recovery golden (clean, \
                  the torn tail it would discard, the refusal), verifies the genesis \
                  binding against --config/--node, refuses a live directory unexamined, \
                  prints a keyval.cli.doctor/v2 report that validates against its \
                  schema, and leaves every file byte for byte as it was (O-7, O-6)",
        kind: Kind::TestGroup,
        covers: &["O-7", "O-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "cli",
            filters: &["doctor_cli::"],
        },
        min_cases: 16,
    },
    CheckDef {
        id: "OBSERVATION-SCHEMA",
        summary: "the observation schema keyval.observation/v1, the oracles' only input \
                  (T-12), follows the schema conventions; its two fixture runs (a marker heal \
                  and a drain heal) validate, together exercise every record kind, operation, \
                  attempt state, phase, sync state, fault action, role and process state, keep \
                  the stream rules (ordering, incarnations, logs, certificates, generations, \
                  resolutions) and are correct runs (DUR-2 cover, the served prefix, converged \
                  digests); the outcome table covers exactly the (operation, code) pairs §8.4 \
                  allows; each negative fixture is rejected only at its pointer; the README \
                  documents every record kind and its checkers (§10.2, §10.5)",
        kind: Kind::TestGroup,
        covers: &["T-12"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["observation_schema::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "PEER-ENVELOPE",
        summary: "the peer-envelope codec (§8.2, D-63, D-71): a sealed message opens at its \
                  destination byte for byte in the documented wire format; a mismatched protocol \
                  version, cluster instance, semantic fingerprint, sender (not another voter, or \
                  not the node the transport received it from) or destination drops the envelope \
                  and is counted under its own reason, as are malformed and oversized envelopes, \
                  inadmissible messages and a message whose own sender is not the envelope's; \
                  random bytes and every changed header byte drop; under a valid header, random \
                  and mutated payloads reach the decoder, and malformed protobuf (a wrong wire \
                  type, an unknown entry type, a cut varint, an unknown field) is dropped and \
                  counted under message, without a panic in these runs (exhaustive fuzzing is \
                  W53's, §10.19); and a three-voter cluster whose every message travels sealed \
                  elects a leader and commits with nothing dropped",
        kind: Kind::TestGroup,
        covers: &["CD-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["envelope_tests::"],
        },
        min_cases: 12,
    },
    CheckDef {
        id: "PEER-ADMISSION",
        summary: "the adapter steps only peer messages v1 exchanges (br-xx08; §7.3, §7.8, \
                  §8.2; W02 audit S-1, H-3): Append, Heartbeat, RequestVote and \
                  RequestPreVote and their responses, addressed to this node from another \
                  voter; it refuses every other message type, snapshots and snapshot \
                  requests, contexts and priorities, term 0, terms and indices above 2^53 - 1, \
                  entries v1 never replicates or that do not follow their predecessor, and \
                  unknown fields at every depth, refused before decoding by a bounded wire \
                  scan (no groups, lengths within the bytes, at most 65,537 entries and 5 \
                  snapshot node IDs) and canonical bytes only (br-umjh, br-808c); a vote \
                  request from a non-voter, a \
                  proposal, a snapshot or a configuration change from the wire never reaches \
                  the library, a follower that knows its leader never forwards a \
                  proposal, an append whose previous entry pairs index and term 0 wrongly \
                  (br-0mio), a term above the 2^52 ceiling (br-fdal; term-space exhaustion \
                  is a stated §2.2 limit) and a heartbeat committing past this node's log \
                  (br-1jtp) are \
                  refused, \
                  an append conflicting below the commit index is answered without a \
                  panic, and a heartbeat from another voter still reaches the library",
        kind: Kind::TestGroup,
        covers: &["CD-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["admit_tests::", "decode_tests::"],
        },
        min_cases: 31,
    },
    CheckDef {
        id: "DIFFERENTIAL",
        summary: "the state machine matches the reference model on 400 seeded random logs of PUTs, empty \
                  entries and GETs under DM-6 quotas, generated with the model so refused PUTs are replayed \
                  after making room: every apply result with its echoed request ID, every GET through its \
                  read path, the applied index, live counters and committed command bytes (the model \
                  decides which entries count, the codec their length; DM-7) after every step, and the \
                  final map in iteration order and the request history agree; the logs reach every outcome \
                  and boundary, a byte refusal one byte over the quota included, each at or above a floor; \
                  and thirteen test-local mutants around the real state machine are each caught by their \
                  own comparison (apply, GET, counters, map, stored results, unknown IDs) and shrunk (T-7, \
                  §10.14)",
        kind: Kind::TestGroup,
        covers: &["T-7"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["differential::"],
        },
        min_cases: 16,
    },
    CheckDef {
        id: "DIFFERENTIAL-DELETE-CAS",
        summary: "the differential test extended to DELETE and compare-and-swap (T-7, F-3, F-5, \
                  DM-3, DM-6, W27.3): on 400 seeded logs mixing PUTs, DELETEs and \
                  compare-and-swaps (expecting what the key holds, absence, the next version or \
                  an index up to the next entry's) with GETs, empty entries, replays and \
                  conflicts under DM-6 quotas, generated with the model, DIFFERENTIAL's harness \
                  compares every apply result with its echoed request ID, every GET, the applied \
                  index, live counters and committed command bytes after every step, and the \
                  final map and request history against the reference model; the logs reach \
                  DELETEs of live and absent keys, re-created keys, CASes that create and that \
                  swap, mismatches naming a version and naming absence, mismatches whose write \
                  would also break DM-6, CASes the quota refuses, replayed deletes and \
                  mismatches, conflicts and GETs of deleted keys, each at or above a floor; and \
                  five broken variants (capacity checked before the expectation, a CAS applied \
                  as a PUT, a DELETE keeping its bytes, a DELETE always reporting existed, a \
                  mismatch without its current version) are each caught by their own comparison \
                  and shrunk",
        kind: Kind::TestGroup,
        covers: &["T-7", "F-3", "F-5"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["differential_delete_cas::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "DIFFERENTIAL-SCAN",
        summary: "the differential test extended to SCAN (T-7, F-4, §8.5, §7.11, W28.2): on seeded \
                  logs of PUTs (many of them 60 to 65 KB), DELETEs and empty entries over 44 keys, \
                  every SCAN (bounds on and between keys, an optional after, limits including the \
                  invalid 0 and 129, and start > end) gets the same refusal or the same page from \
                  the state machine as from the reference model, items with values and versions, \
                  has_more and next_after, and continued traversals agree page by page; a designed \
                  log puts the 1 MiB byte cap at N - 1, N and N + 1; the logs reach every way a \
                  page ends; and six broken variants (a page one short, a lost has_more, a wrong \
                  next_after, a stale view, a version off by one, a limit one too loose) are each \
                  caught by a SCAN comparison",
        kind: Kind::TestGroup,
        covers: &["T-7", "F-4"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["differential_scan::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "RAFT-PINS",
        summary: "the raft-rs 0.7.0 semantics the Ready loop relies on, pinned on the \
                  library itself: a non-leader's Ready messages are all persisted messages \
                  and a leader's all ordinary; a follower's acknowledgement waits for its \
                  entries; a non-leader has no messages after advance_append; committed \
                  entries are handed over once each, only up to the persisted index; \
                  advance_append applies nothing, advance also marks applied what earlier \
                  Readies handed over, and advance_apply_to moves only the applied index; and \
                  the per-Ready committed bound leaves work for an immediate drain (§7.4, \
                  W13.4)",
        kind: Kind::TestGroup,
        covers: &["DUR-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["ordering_tests::pins::"],
        },
        min_cases: 5,
    },
    CheckDef {
        id: "RAFT-ORDERING",
        summary: "three Ready loops under seeded hostile schedules (late and reordered \
                  syncs, completions from crashed incarnations, a reordering, duplicating and \
                  lossy network, inputs during a sync, crashes that lose unsynced bytes and \
                  restart through journal recovery): no message leaves on unsynced state \
                  (DUR-2), one node leads each term, every node and incarnation applies one \
                  history, an idle loop has no ready work left, and after the faults heal \
                  every node applies everything ever committed; a crashed incarnation's \
                  completion never completes its successor, an early or repeated SyncDone \
                  breaks its invariant, and a peer's snapshot never reaches a Ready (§7.4, \
                  W13.4)",
        kind: Kind::TestGroup,
        covers: &["DUR-2", "C-2"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["ordering_tests::adversarial::"],
        },
        min_cases: 6,
    },
    CheckDef {
        id: "RAFT-BOUNDS",
        summary: "however far commit runs ahead of applied, the library's election-attempt and \
                  vote-commit scans for pending configuration changes read nothing from the \
                  journal (W02 audit U-1, U-2), and a Ready hands over committed entries only \
                  within max_committed_size_per_ready (U-3), on a 2,000-entry log (§7.3, §7.11)",
        kind: Kind::TestGroup,
        covers: &["CD-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["large_log_tests::"],
        },
        min_cases: 3,
    },
    CheckDef {
        id: "RAFT-CLUSTER",
        summary: "scripted in-memory clusters of Ready loops, one and three voters, elect one \
                  leader, commit and apply the same entries at the same indices in log order on \
                  every node (C-1), and every entry committed before a restart, of a lone voter \
                  through journal recovery or of a majority, is applied again at the same index \
                  (C-2) (§7.4, D-45)",
        kind: Kind::TestGroup,
        covers: &["C-1", "C-2"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "raft_adapter",
            filters: &["cluster_tests::"],
        },
        min_cases: 3,
    },
    CheckDef {
        id: "FIXTURE-LISTS",
        summary: "config/fixtures/ holds no fixture that the parser's acceptance list (types, \
                  CONFIG-RULES) or the RawNode construction list (raft_adapter, RAFT-ADAPTER) \
                  lacks, so the D-78 tests cover every committed fixture (O-5, D-78, P10)",
        kind: Kind::TestGroup,
        covers: &["O-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["fixture_lists::"],
        },
        min_cases: 2,
    },
    CheckDef {
        id: "MODEL-SERVER",
        summary: "the reference model server serves every operation of api/openapi.yaml a \
                  single node can show over the model: writes as the next log entry, same-ID \
                  retries and conflicts from the history with no new entry, terminal 409/507 \
                  results that replay, 507 history_full (not submitted) for a new write at \
                  max_log_entries after the history's answer, status lookups, SCAN pages, \
                  health and cluster status; \
                  strict parsing (RFC 3986 query decoding with unescaped '+', bad escapes, \
                  invalid UTF-8, repeated and unknown parameters refused; closed JSON object \
                  bodies with unknown, duplicate and null-but-present fields refused; request \
                  IDs named once well-formed; byte limits after decoding; 413 over 512 KiB, \
                  delivered before the close; 415 without application/json); HTTP/1.1 \
                  keep-alive, strict framing and Cache-Control: no-store on every response \
                  (§8, §10.14)",
        kind: Kind::TestGroup,
        covers: &["T-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "model",
            filters: &["server::"],
        },
        min_cases: 29,
    },
    CheckDef {
        id: "SIM-NET",
        summary: "the simulated network (§9.1, §10.2, W16.1): delivery follows each copy's own \
                  seeded delay, never send order; loss follows its probability on peer links \
                  and client paths; a duplicate is an independent copy; a symmetric partition \
                  cuts every link across it both ways and an asymmetric cut one way, \
                  catching traffic in flight too; each directed link queue drops and counts \
                  past its message or byte bound and never grows; client paths use their own \
                  profile and are never partitioned; copies in flight are bounded; the same \
                  seed gives the same fates; and real sealed peer envelopes cross unchanged \
                  and open at the destination. Also (br-3lhv): every fate is counted under its \
                  own field alone; a link queue holds exactly its byte bound; deliveries and \
                  their records keep their endpoints and bytes; and client paths lose in both \
                  directions",
        kind: Kind::TestGroup,
        covers: &["T-3", "P3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["net_tests::", "net_profile_tests::"],
        },
        min_cases: 22,
    },
    CheckDef {
        id: "SIM-NET-SCENARIOS",
        summary: "five real Ready loops exchange real sealed peer envelopes over the simulated \
                  network, driven by the scheduler, under each §9.1 network fault: loss, delay \
                  and reordering, duplication, a 3/2 partition with the old leader in the \
                  minority, a leader that can send but not receive, link-queue overflow, \
                  restarts with packets in flight (stale packets reach the new incarnation as \
                  ordinary stale messages) and client responses lost after a committed \
                  application; every run keeps one leader per term and one applied history, \
                  sends nothing on unsynced state, and commits every command once the faults \
                  heal (W16.2)",
        kind: Kind::TestGroup,
        covers: &["C-2", "P3"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["scenarios::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "SIM-FAULTS",
        summary: "the full §9.1 fault vocabulary on the real five-node simulated cluster \
                  (W37.1): crash keeping nothing, a prefix, a garbage tail or everything of \
                  the unsynced bytes; crashes armed before a sync, after a sync (its \
                  completion reaching the next incarnation as a stale one) and before a \
                  client reply (a retry replays the committed result); an all-node crash; \
                  the loss of each of the ten node pairs and a refused third loss; symmetric \
                  3/2, 4/1 and 2/2/1 partitions, directed cycles and one-way cuts; lost and \
                  duplicated client responses and peer messages, delay and reordering; a \
                  leader paused past an election timeout; a crash clearing a pause; \
                  scheduled faults giving one trace however the run is polled; and refusal \
                  of faults naming unknown nodes or past times. Faults that change state are \
                  logged only when they do (a crash of a down node is not a fault that \
                  happened). Simulation evidence from one fixed seed per scenario, not proof",
        kind: Kind::TestGroup,
        covers: &["T-3", "P3"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["nemesis_tests::"],
        },
        min_cases: 31,
    },
    CheckDef {
        id: "MODEL-SPEC",
        summary: "the reference model server, run as its own binary over real HTTP, \
                  answers every operation with a status the spec declares (or 405/415 \
                  with the invalid_request envelope), Content-Type: application/json, \
                  Cache-Control: no-store and a body that validates against that \
                  status's schema (the SPEC validator, extension keywords included): \
                  every write and read status a single node shows, the spec's PUT and \
                  CAS examples reproduced exactly, every request-body and parameter case \
                  of api/fixtures/ accepted or refused as the fixtures say, and the raw \
                  strict-JSON and RFC 3986 query rules (W30, T-7, §8.1)",
        kind: Kind::TestGroup,
        covers: &["T-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["model_spec::"],
        },
        min_cases: 6,
    },
    CheckDef {
        id: "NODE-CONTROLLER",
        summary: "the node controller's mechanism (§7.4, §7.7, §7.10–§7.12, W14.1): start replays \
                  exactly the committed entries and is Ready, truncating and syncing a torn tail \
                  first and feeding the library nothing (ticks owed, peers waiting, attempts still \
                  handed over) until that sync completes, and refuses a committed entry it cannot \
                  read back; the inbound queue takes 1,024 client attempts and peer envelopes, \
                  then drops and counts peers and answers clients busy, while ticks merge into a \
                  count and completions always get in; inputs leave in arrival order, and while a \
                  sync is outstanding client attempts are handed over, peer envelopes wait and \
                  proposals wait for the library; deadlines fire on ticks even then, and a burst \
                  of overdue ticks is processed at most 8 per turn between queued inputs; a \
                  completion from another incarnation is ignored and counted; a failed storage \
                  effect fails the node, which drops and counts peers, those already queued at \
                  once so their places go to client attempts, and feeds the library nothing; three \
                  voters elect and commit with every message sealed and opened by the controllers \
                  and nothing dropped; under one write per sync cycle a leader still takes its \
                  followers' responses (a queued peer envelope goes before a waiting proposal, \
                  br-m7x7), keeps leading and commits; the same inputs give the same effects; and \
                  each NODE-* invariant fires on corrupt state; the controller logs RoleChanged \
                  whenever the library's role, term or believed leader changes, once per change \
                  and never otherwise (O-3, br-qb4p)",
        kind: Kind::TestGroup,
        covers: &["CD-9", "CD-7", "T-3", "P3", "O-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &[
                "queue_tests::",
                "controller_tests::",
                "startup_tests::",
                "invariant_fixtures::",
            ],
        },
        min_cases: 36,
    },
    CheckDef {
        id: "NODE-WRITE-PATH",
        summary: "the write path (§7.8, F-7, A-5, DM-7, C-5, W14.2, W27.3), for every write \
                  kind: the fixed decision order answers at its first applicable step: \
                  recovering on a node that is not Ready, unavailable while stopping, busy past \
                  256 waiting attempts or 16 MiB of their commands even for a known request ID, \
                  the original result (replayed) or request_id_conflict from the applied history \
                  on any Ready node, attach to a pending same-ID proposal (at most 16 waiters), \
                  not_leader naming the believed leader or unavailable with none, history_full \
                  at max_log_entries or max_history_bytes (entries named first), busy past \
                  max_uncommitted_bytes (exact at the bound, submitted proposals counted, \
                  reopening as proposals apply), then propose; a proposal is handed to the \
                  library once, even after a queued owner's deadline and a retry or the waiter \
                  behind it (br-nxsb); an attempt is submitted when handed to the library and \
                  answered only by the application of an entry carrying its request ID: its own \
                  command's result, or request_id_conflict at once when another command for the \
                  ID applies first, the attempts behind answered from the history, a queued \
                  proposal withdrawn and a submitted one's bytes in flight until its copy \
                  applies or the leadership is lost (br-evs6); a synchronous drop is busy and \
                  attached waiters unknown_outcome; a same-ID attempt with another command \
                  waits, then gets request_id_conflict on apply or proposes its own if the \
                  proposal failed unapplied; at its deadline a submitted or attached attempt is \
                  unknown_outcome and one never proposed busy; leadership loss, a new term or \
                  failure answers submitted attempts unknown_outcome and re-decides the rest; \
                  shutdown answers unknown_outcome or unavailable; DELETE and compare-and-swap \
                  take the same path, proposed exactly as sent and decided only when their entry \
                  applies (C-5): a CAS expecting the version an earlier pending PUT will give \
                  matches although its key was absent at admission, one expecting absence then \
                  mismatches, and their retries replay the original result; end to end through \
                  the controller, a lone leader commits and replays PUT, DELETE and \
                  compare-and-swap, a replay is answered while a sync is outstanding, a deadline \
                  gives unknown_outcome, and a follower redirects to the leader; a seeded sweep \
                  of 200 attempt schedules (hand-overs, synchronous drops, its own and other \
                  leaders' entries applying, deadlines, leadership changes, history ceilings) \
                  never answers a submitted attempt not_submitted and never invents a result \
                  (A-5, W29.3); each NODE-WRITE-* invariant fires on corrupt state",
        kind: Kind::TestGroup,
        covers: &["F-7", "A-5", "DM-7", "C-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["write_path_tests::", "write_path::invariant_fixtures::"],
        },
        min_cases: 40,
    },
    CheckDef {
        id: "NODE-READ-PATH",
        summary: "the read path and the request lifecycle (§7.9, F-2, F-4, F-9, A-2, C-3, C-4, \
                  C-8, W14.3, W28.2): a GET answers the value and version, or not_found, from the \
                  applied prefix in one turn with served_by (node, applied index); a read during \
                  an outstanding sync shows only the applied prefix, never the unsynced entry; the \
                  status lookup answers the stored result if applied, unknown otherwise, proposing \
                  nothing; a node that is not Ready answers recovering and a stopping one \
                  unavailable; every applied entry, the election no-op included, is observed once \
                  in index order before what it answers, and a restarted node observes its \
                  replayed prefix; reported prefixes never go back within an incarnation; a \
                  follower serves reads from its own applied prefix; and a SCAN page is answered \
                  in one turn from the applied prefix it reports, with served_by, continues after \
                  next_after, never shows an unsynced entry, is refused like a GET when the node \
                  is not Ready or is stopping, and is served by followers too",
        kind: Kind::TestGroup,
        covers: &["A-2", "C-3", "C-4", "C-8", "F-2", "F-4", "F-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["lifecycle_tests::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "NODE-ATTEMPT-TABLE",
        summary: "the attempt table through core inputs (§7.8, §8.4, F-7, A-2, A-5, DM-7, \
                  W14.4): every PUT and GET outcome path, driven by inputs to controllers \
                  running the real request lifecycle: unavailable with no leader known, and \
                  not_leader naming the believed leader, which follows a new election; busy past \
                  256 waiting writes even for a known request ID while a GET is still served, \
                  past 16 waiters per request, one byte past max_uncommitted_bytes (an exact fit \
                  admitted, the bound reopening as entries apply, the refused write never \
                  executed), and for a GET and a write when the inbound queue is full; once a \
                  storage failure fails the node, its submitted and attached writes answer \
                  unknown_outcome and every other attempt recovering; a journal hard stop while \
                  a proposal drains answers it unknown_outcome; shutdown answers unknown_outcome \
                  or unavailable; at max_log_entries the leader answers history_full while a \
                  follower redirects and both replay a known retry; a follower answers a known \
                  request ID from its history, never redirecting it; a same-command retry \
                  attaches without a second entry and shares the result; another command with \
                  the same ID waits, then gets request_id_conflict; at the deadline submitted \
                  and attached attempts answer unknown_outcome and ones never handed over busy; \
                  a submitted write is answered only by its own entry's apply; a leadership \
                  loss, through a later term's message or check_quorum, answers submitted and \
                  attached attempts unknown_outcome before any deadline and redirects the rest",
        kind: Kind::TestGroup,
        covers: &["A-2", "A-5", "DM-7", "F-7"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["attempt_table_tests::"],
        },
        min_cases: 17,
    },
    CheckDef {
        id: "APPLY-DELETE",
        summary: "DELETE in apply (§7.5, F-3, DM-3, DM-5, DM-6, DM-7, W27.1): a DELETE removes a \
                  live key and returns existed = true, and deleting an absent key succeeds with \
                  existed = false and changes no live state; it assigns no version, and a \
                  re-created key gets its re-creating index; at the key quota it is never \
                  refused and frees room for a new key; its result is recorded once in the \
                  history, so a duplicate entry replays existed = true and another command with \
                  its ID conflicts; its canonical length counts toward DM-7 once; and a log with \
                  deletes replays to the same state and results",
        kind: Kind::TestGroup,
        covers: &["F-3", "DM-3", "DM-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["apply::delete_tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "STATE-SCAN",
        summary: "SCAN in the state machine (§8.5, §7.11, F-4, DM-4, W28.1): a page holds the keys \
                  in [start, end) after an exclusive `after`, in byte-wise UTF-8 order; it takes \
                  at most limit items and 1 MiB of decoded key+value bytes, stopping before the \
                  item that would exceed either cap and never skipping to a smaller one, and \
                  always returns the first item; has_more and next_after (the last returned key) \
                  are set exactly when an in-range key remains, and neither that probe item's \
                  bytes nor next_after's count; an independent, hand-derived Unicode byte-order \
                  fixture; over seeded states, each page is the longest in-range run the caps \
                  allow and continued pages return every in-range key once, in order",
        kind: Kind::TestGroup,
        covers: &["F-4", "DM-4"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["scan_tests::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "SERVER-INIT",
        summary: "keyval-server init and the start's preflight (§7.7 step 0 and steps 1–4, D-52, \
                  DUR-7, DUR-8, O-6, W18.1): init creates the data directory beside the config \
                  file, takes the lock, creates journal.log and syncs the directory before \
                  appending the genesis frame and syncing it, through the byte-storage port in \
                  that order, binding the directory to the instance ID, node, voters and \
                  semantic configuration; it completes an interrupted init over the directory \
                  alone, an empty journal or a partial genesis (truncated away), and never \
                  re-initialises a valid genesis, a genesis for another configuration or other \
                  content, leaving each unchanged; a held lock or an unknown node writes \
                  nothing, and a failed sync stops init at that step; run refuses a missing data \
                  directory without creating it, no journal, an empty journal or an incomplete \
                  genesis, a held lock, another voter set or other semantic limits, and a \
                  damaged journal, modifying nothing; an initialised directory passes the \
                  preflight holding the lock, and run then says it does not serve yet (W18.2); \
                  the binary exits 0, 1 for a refusal and 2 for usage or configuration, its \
                  --json output conforming to keyval.server.init/v1 and keyval.server.error/v1",
        kind: Kind::TestGroup,
        covers: &["DUR-7", "DUR-8", "O-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &["init_tests::", "run_tests::", "cli_tests::"],
        },
        min_cases: 15,
    },
    CheckDef {
        id: "APPLY-CAS",
        summary: "compare-and-swap in apply (§7.5 step 4, F-5, DM-3, DM-5, DM-6, DM-7, W27.2): \
                  the expectation (absent, or a version) is checked against the state when the \
                  entry applies; a match sets the key with version = its execution index; a \
                  mismatch is a terminal version_mismatch naming the current version or its \
                  absence, changes no live state, is recorded once in the history and counts its \
                  command bytes toward DM-7; mismatch is decided strictly before capacity, so a \
                  failed expectation whose write would also exceed DM-6 is still a mismatch; a \
                  matching CAS over the key or byte quota is capacity_exceeded; a duplicate \
                  entry replays the original mismatch even when it would now match; and after \
                  delete and re-create only the new version matches",
        kind: Kind::TestGroup,
        covers: &["F-5", "DM-3", "DM-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "statemachine",
            filters: &["apply::cas_tests::"],
        },
        min_cases: 8,
    },
    CheckDef {
        id: "SERVER-RUNTIME",
        summary: "the production node's wiring (§7.12, §7.2, §7.10, §7.11, O-3, W18.2): one \
                  serial controller thread owns the core and is fed through one bounded \
                  hand-off: client attempts and peer messages up to controller_queue_inputs \
                  (1,024), past which a client attempt is refused busy (not submitted) and a \
                  peer message dropped, both counted, while every attempt handed over is still \
                  answered; I/O completions and shutdown bypass the bound (INBOX-UNREFUSABLE), \
                  and ticks merge into a count; storage effects run in order on an I/O thread \
                  whose completions return as inputs, so a write answered by the node survives a \
                  restart and is applied again from the journal; a ticker turns the monotonic \
                  clock into ticks, never more than elapsed, a pause coming back as one batch, \
                  and drives elections; entropy is seeded once at boot from the operating \
                  system, logged, and each draw moves its stream on; the node's log is one JSON \
                  object per line (boot with the seed, lifecycle changes, observations, stop), \
                  behind a log_queue_records (4,096) queue whose full drops are counted and \
                  reported in the log; and the node's essential threads (controller, I/O, clock, \
                  log writer, HTTP accept) run under a watch: a panic on any of them halts the \
                  node, which then reads failed, takes no more input, logs thread_panicked, and \
                  stops (CD-7, A-2, br-7tjt); \
                  the bound counts the inputs the controller holds, taken and \
                  not yet judged or queued in the core, so it applies once and \
                  refusals are counted once (RUNTIME-ONE-BOUND, br-rl0u)",
        kind: Kind::TestGroup,
        covers: &["O-3", "DUR-3", "CD-9"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &[
                "runtime_tests::",
                "inbox::tests::",
                "clock::tests::",
                "entropy::tests::",
                "logs::tests::",
                "watch::tests::",
            ],
        },
        min_cases: 22,
    },
    CheckDef {
        id: "SIM-CLUSTER",
        summary: "the simulation composition root (§7.13, §10.2, W17.1): every voter of a \
                  five-node config runs as the production core (controller, request lifecycle, \
                  raft adapter, journal) over the simulated disk, network, clock and entropy in \
                  one thread; the cores elect one leader every node knows; a PUT commits and \
                  applies on all five nodes and a GET reads it back; a follower crashed with \
                  unsynced bytes and a crashed leader restart as new incarnations from the bytes \
                  their disks kept and catch up, and across seeds the crashes do lose unsynced \
                  bytes; a sync completion of an earlier incarnation is ignored; a lossy, \
                  duplicating network still commits; the god view's log mirror agrees with each \
                  core's last index and term after every step and, audited in full, with each \
                  disk's written and durable journal, and a deposed leader's replaced suffix is \
                  never called durable before its sync; the trace records every client attempt's \
                  invocation and first response, reads and status lookups under no request ID, and \
                  one first execution per request across replays; how often the driver polls never \
                  changes the run, and nothing is scheduled before the time it ran to; and the \
                  same seed gives the same trace",
        kind: Kind::TestGroup,
        covers: &["T-2", "DUR-3", "P3"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["cluster_tests::"],
        },
        min_cases: 14,
    },
    CheckDef {
        id: "SERVER-HTTP",
        summary: "the client API over HTTP (§8, §7.12, O-1, O-9, W18.3), std-only HTTP/1.1: a \
                  PUT parses into domain types at the boundary and refuses, 400 invalid_request, \
                  a missing or malformed Idempotency-Key, a missing, repeated, unescaped-plus, \
                  empty or 257-byte key, a missing Content-Type, an unknown field, a duplicate \
                  member, malformed UTF-8, a wrong type, trailing data and a missing body, with \
                  request_id exactly when a well-formed ID was sent, and 413 too_large past 64 \
                  KiB of value or 512 KiB of body; every core answer renders with §8.4's status \
                  and details (VersionedWriteResult with version == index, replays, \
                  request_id_conflict, capacity_exceeded, a 307 naming the leader's client URL \
                  with the key re-encoded, busy, unavailable and unknown_outcome with every \
                  voter's URL, recovering, history_full; ReadResult, not_found and read refusals \
                  without write fields); health live and ready and /v1/cluster render the \
                  published snapshot; a hand-off refusal is busy and a silent core \
                  unknown_outcome; queries are percent-decoded strictly and re-encoded exactly; \
                  heads over 16 KiB, chunked bodies and bare-LF lines are refused; every \
                  response is no-store; DELETE, CAS, SCAN and the status lookup are refused \
                  until served; a non-loopback address warns; end to end a real node answers \
                  PUT, GET, health and cluster over loopback, keeps a connection alive and \
                  closes one that asks",
        kind: Kind::TestGroup,
        covers: &["F-1", "F-2", "O-1", "O-9", "CD-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &[
                "serve_tests::",
                "api::tests::",
                "http::tests::",
                "query::tests::",
                "serve::tests::",
            ],
        },
        min_cases: 18,
    },
    CheckDef {
        id: "REQUEST-STATUS",
        summary: "the request-status lookup (§7.9, §8.2, §8.3, F-9, P8, A-2, W29.1), through the \
                  controller: a request this node has applied is known with its original \
                  terminal result, whatever it decided (a set, a DELETE that existed or not, a \
                  version_mismatch naming the current version or its absence, and a live-quota \
                  capacity_exceeded on every node of a cluster), and retries or other commands \
                  under its ID never change it; one it has not applied is unknown, which never \
                  means it did not happen: the same request turns known once its entry applies, \
                  on a lone leader after its outstanding sync and on a partitioned follower once \
                  it catches up; the lookup is answered in one turn with served_by and nothing \
                  else, never proposing or recording anything, so the ID later executes as a \
                  first execution; and a node that is not Ready answers recovering, or \
                  unavailable while stopping, never known or unknown",
        kind: Kind::TestGroup,
        covers: &["F-9", "P8", "A-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["status_tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "REQUEST-ID-RACE",
        summary: "the same-request-ID race (§7.8, §7.5 step 3, F-6, C-6, A-5, W29.2), across \
                  nodes and leader changes through the real request lifecycle: two commands \
                  under one request ID never both execute; on one leader the command admitted \
                  first is the original in either order and the other is request_id_conflict \
                  naming it; a command already in the new leader's log stays the original over \
                  one proposed there later; a command lost with its leader yields to the one \
                  proposed at the next, its submitted attempt answering unknown_outcome and a \
                  waiter behind it a non-terminal refusal; a waiter behind a stalled proposal \
                  times out busy and its submitted owner unknown_outcome, never a conflict \
                  before the original is known; and a seeded sweep of two clients racing and \
                  retrying through isolated leaders and redirects ends, every run, with exactly \
                  one original and one conflict that every node agrees on, reaching both \
                  winners, unknown outcomes and redirects at or above floors",
        kind: Kind::TestGroup,
        covers: &["F-6", "C-6", "A-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["race_tests::"],
        },
        min_cases: 5,
    },
    CheckDef {
        id: "SERVER-RESTART",
        summary: "a real single node killed and restarted (§7.7, DUR-1, DUR-3, W18.4) through \
                  test-only PID-receipt helpers that signal only the PID their receipt names, \
                  while it is the unreaped child they started (rule 18): writes acknowledged \
                  before a SIGKILL all read back after the restart with their versions, rebuilt \
                  from the recovered journal alone (the new incarnation logs starting, \
                  recovering and ready, and applies up to the last acknowledged write before any \
                  new one; O-3), health reports ready (O-1), and a retry replays its original \
                  result (F-6); a SIGKILL during a burst of concurrent writes loses no \
                  acknowledged write, and each unanswered write resolves on retry to its \
                  original result or one first execution; a forged or missing receipt signals \
                  nothing. Process-crash evidence: SIGKILL keeps the page cache, so a missing \
                  fsync is invisible here (D-46; W24)",
        kind: Kind::TestGroup,
        covers: &["DUR-1", "DUR-3", "F-6", "O-1", "O-3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &["restart_tests::"],
        },
        min_cases: 4,
    },
    CheckDef {
        id: "HISTORY-ADMISSION",
        summary: "history admission and the live-quota terminal flow end to end (§7.11, §8.3, \
                  §8.4, DM-7, DM-6, A-5, F-10, W29.3), through controllers on a three-voter \
                  small-limits cluster: once the state machine's committed command bytes at the \
                  leader's applied index reach max_history_bytes, the leader refuses a new PUT \
                  or DELETE before proposal with history_full naming max_history_bytes, not \
                  submitted, appending nothing, while a follower still redirects, a retry of an \
                  executed request replays and status lookups and GETs keep working; a new \
                  leader's election no-op still commits past the ceiling while new writes there \
                  stay history_full; and a write over the live-key quota is a terminal \
                  capacity_exceeded, answered as a written result, replayed to a retry even \
                  after room appears, executing again only under a new request ID",
        kind: Kind::TestGroup,
        covers: &["DM-7", "DM-6", "A-5", "F-10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "node",
            filters: &["admission_tests::"],
        },
        min_cases: 3,
    },
    CheckDef {
        id: "SIM-RUN",
        summary: "`./dev sim run --seed 42 --profile slice` gives identical traces in two fresh \
                  processes and labels its output a slice (§12.2 W17): seeds 42 and 43, each run \
                  by two fresh processes of the keyval-sim binary `./dev sim run` starts, through \
                  K-14's fresh-process harness, give byte-identical canonical traces; seed 42's \
                  report conforms to keyval.dev.sim-run/v1, labels the run a simulated slice (P6), \
                  passes, and names its trace by an FNV-1a hash recomputed from the definition; \
                  the slice elects, writes and reads back, crashes a follower holding unsynced \
                  bytes, restarts it caught up and audits every mirror, on seeds 1 to 8 too; a run \
                  cut short by its event budget is incomplete in its verdict and its trace's \
                  bytes, a finding fails a run, and a pass needs every step observed (P10); seed \
                  43's trace differs from 42's (the negative control); and the binary refuses \
                  unusable arguments with exit 2 and no report",
        kind: Kind::TestGroup,
        covers: &["T-3", "T-11", "P6"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["sim_run::", "slice_tests::"],
        },
        min_cases: 10,
    },
    CheckDef {
        id: "DEV-SIM",
        summary: "./dev sim run starts keyval-sim with the seed, profile, configuration (default \
                  config/local.toml, relative to the root) and a trace path under artifacts/sim/, \
                  parses its report strictly (unknown fields, another schema, ok disagreeing with \
                  the verdict, two reports are refused), maps its exit 0 and 1 to the report (ok \
                  only for a pass), its exit 2 to a usage error carrying its own message, and a \
                  build failure, a signal, no report or a launch that cannot start to an internal \
                  error, never a pass (P10); prints the run's simulated-slice label first (P6); \
                  and refuses --scenario before anything runs (W37)",
        kind: Kind::TestGroup,
        covers: &["O-6", "P6", "P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["sim::tests::"],
        },
        min_cases: 9,
    },
    CheckDef {
        id: "CLIENT-RETRY",
        summary: "The client retry policy shared by the CLI and the simulated clients (§10.3, \
                  §12.2 W36.1): one logical operation keeps its request ID and command across \
                  every attempt, follows not_leader at once, backs off with bounded jitter, \
                  resends after unknown_outcome or silence or asks the status lookup (a status \
                  of unknown means resend, P8), and stops at the attempt, node and deadline \
                  bounds of §7.11 with an unresolved outcome, never a new ID",
        kind: Kind::TestGroup,
        covers: &["F-6", "A-5", "O-4", "P8"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "types",
            filters: &["retry::tests::"],
        },
        min_cases: 10,
    },
    CheckDef {
        id: "SIM-CLIENTS",
        summary: "The seeded simulated clients (§10.3, §12.2 W36.1): a seed generates the same \
                  plan of 1 to 8 clients with hot-key contention and the edge-case key and value \
                  corpus, and driving it against the simulated five-node cluster gives the same \
                  sends and outcomes every time, every write carrying a unique request ID and \
                  every retry the same ID and command",
        kind: Kind::TestGroup,
        covers: &["F-6", "A-5", "P4"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["clients_tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "SIM-OBSERVE",
        summary: "The observation recording and misuse stream (§10.2, §10.3, §12.2 W36.2): the cluster logs every client attempt's invocation, response and delivery, and a run's document is a valid keyval.observation/v1 that keeps the stream rules; the opt-in misuse stream reuses a request ID with a different command only after an observed terminal result, is answered request_id_conflict and is excluded from logical-write counts; a seed decides the document byte for byte",
        kind: Kind::TestGroup,
        covers: &["F-6", "A-5", "P4"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["observe_tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "PEER-TRANSPORT",
        summary: "The HTTP peer transport (§8.2, §7.11, W21.1): every node serves POST /raft on its peer address, taking only well-formed posts that name a sender (400 without, 404 and 405 for other paths and methods, 413 past the envelope limit) and handing the body to the core, whose W13 codec alone judges the envelope; each peer has one bounded send queue (64 messages, 8 MiB) that drops and counts what does not fit; three real processes elect a leader and replicate a write through it. Real processes on loopback: test evidence, not proof",
        kind: Kind::TestGroup,
        covers: &["F-7", "A-1", "A-3", "P3"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &["peer::tests::", "transport_tests::"],
        },
        min_cases: 7,
    },
    CheckDef {
        id: "DEMO-M1",
        summary: "The M1 owner demo (§12.2 W19, P6): `./dev demo m1` derives implemented-versus-pending from the work graph and the tracker export (an item is implemented exactly when its bead is closed), rewrites only the per-run lines of the one-voter fixture, signals a node only by its own pid receipt, parses HTTP responses strictly, and its report names the exact commands, build, visible limitations and, for a run that did not pass, says so last. Unit evidence for the demo's logic; the end-to-end run is test evidence, not proof",
        kind: Kind::TestGroup,
        covers: &["G1", "P6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "xtask",
            filters: &["demo::tests::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "AUDIT-REPORTS",
        summary: "Every audit report under reports/audits/ (§11.9, W20) names its milestone, auditor, independence, tree ID and commit in a header, and every `## F-<n>` finding names a `Bead: <id>` the tracker export holds, so no finding goes unfiled; a report with no findings must say `Findings: none`, and a missing report, an unreadable export or a vacuous selection fails (P10). Checks the report's form and bead links, not the audit's judgement",
        kind: Kind::Static,
        covers: &["P10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::Static(StaticCheck::AuditReports),
        min_cases: 30,
    },
    CheckDef {
        id: "CLUSTER-ROUTING",
        summary: "Five-process operation and write routing (F-7, §8.1, §8.4, W21.2): five real processes started from config/local.toml (distinct client and peer addresses and data directories) elect one leader and replicate a write to all five; a write to a follower is 307 with a typed not_leader body and one Location at the leader's client address carrying the same path and re-encoded key, and sending the same request ID there commits; a follower answers an applied request ID from its own history without redirecting and serves reads locally; with no quorum a write is 503 unavailable listing the five configured nodes and no Location; after the leader is killed the other four elect and redirect to the new one. Real processes on loopback: test evidence, not proof",
        kind: Kind::TestGroup,
        covers: &["F-7", "A-1", "A-3", "O-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &["routing_tests::"],
        },
        min_cases: 4,
    },
    CheckDef {
        id: "ENVELOPE-IDENTITY",
        summary: "Peer-envelope identity on a real node's peer address (§8.2, W21.3): an envelope with another cluster instance ID, protocol version, semantic fingerprint, a sender that is not the node it came from, is outside the voter set or is the receiver itself, or another destination is dropped and counted (one input_dropped log record each) while a matching envelope from a configured voter logs no drop; each forged envelope is also opened by a local codec of the same identity, which names the one reason it is refused for. The node does not yet report drop counts by reason (those are PEER-ENVELOPE's). Real processes on loopback: test evidence, not proof",
        kind: Kind::TestGroup,
        covers: &["F-7", "A-3", "O-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "server",
            filters: &["identity_tests::"],
        },
        min_cases: 2,
    },
    CheckDef {
        id: "K-1",
        summary: "History checker K-1, durability (§10.5, W38.1): every success-acknowledged write (a terminal ok whose response reached its client) has its first execution, taken from the reference model's replay of the committed log, at its reported index with its reported version or existed, and every recoverable node's final log, rebuilt from recovered, append and truncate records, holds that entry's bytes at that index; a later legitimate overwrite passes, and a lost acknowledged write, a wrong index or effect, and an entry missing from or different on a recoverable node each fail. Hand-made histories and the two keyval.observation/v1 fixture runs: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["C-2", "DUR-1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k1::tests::"],
        },
        min_cases: 19,
    },
    CheckDef {
        id: "K-2",
        summary: "History checker K-2, exactly-once (§10.5, W38.1): every terminal execution response for a request ID (ok, version_mismatch or capacity_exceeded, a write's own or inside a known status answer) reports the index and decision of the one first execution the reference model's replay of the committed log gives it; two first executions, responses that disagree, an execution with no committed command and a conflicting reuse answered as an execution each fail, while request_id_conflict answers, replays and a second copy of the command in the log pass. Hand-made histories and the two keyval.observation/v1 fixture runs: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["C-6", "F-6", "DM-5"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k2::tests::"],
        },
        min_cases: 10,
    },
    CheckDef {
        id: "K-8",
        summary: "Raft safety K-8 (§10.5, W40.1): on the god view after every step, election safety (one leader per term, one vote per term; leaders of different terms may overlap), log matching on entry bytes and term, leader completeness at election, state-machine safety (commit and applied within the certified prefix, node logs agree with certified entries within commit, one state digest per applied index, one certificate per index) and leader append-only, with commitment decided by commit certificates recomputed from accept records (a majority accepting an entry in its own term; an old-term entry on a majority of disks is not committed by itself; certificates survive node loss). Hand-made five-voter god views, each property broken once, and the two keyval.observation/v1 fixture runs plus a mutated copy: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["C-1", "C-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k8::tests::"],
        },
        min_cases: 33,
    },
    CheckDef {
        id: "K-11",
        summary: "Persistence ordering K-11 (§10.5, W40.2): on the trace, every vote grant, append acknowledgement, client success, published read and acceptance rests only on durability a completed sync already provided, namely the term and vote and the entries and never the commit index; a sync covers the log as of its request, a stale completion covers nothing (B-17) and a truncation uncovers a replaced entry until a later sync. Hand-made five-voter traces with each release broken once (B-02, B-03), the two keyval.observation/v1 fixture runs as controls and mutated copies that fail: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["DUR-1", "DUR-2", "C-2"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k11::tests::"],
        },
        min_cases: 24,
    },
    CheckDef {
        id: "K-12",
        summary: "Unknown-outcome honesty K-12 (§10.5, W40.2): after submission a write is answered a terminal decision or unknown_outcome and never a guessed failure (not_leader, busy, unavailable, recovering, history_full), not_submitted names only an attempt never submitted (a dropped proposal retracts its submission, an attached attempt counts as submitted), an attempt gets one response, a status answer never contradicts itself, every logical write has exactly one resolution, and a write resolved not_executed or refused has no decision or stored result and, after a submission, rests on unknown status lookups only. Hand-made traces with each rule broken once (B-15), the two keyval.observation/v1 fixture runs as controls and mutated copies that fail: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["A-5", "P8", "F-10"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k12::tests::"],
        },
        min_cases: 25,
    },
    CheckDef {
        id: "SWARM-SCENARIO",
        summary: "swarm configurations and scenario files (W37.2, §9.1, §9.4, §10.4, \
                  T-10): each seed draws a non-empty subset of the nine fault types \
                  and an intensity, with a no-fault control and a manifest record \
                  that reads back; generated schedules are only self-undoing \
                  episodes (crash and restart, pause and resume, partition and heal, \
                  cut and mend, a network fault and the healthy profile), at most \
                  two nodes lost, no network episodes overlapping, no conflicting \
                  steps at one instant and every step before chaos ends, so three \
                  voters are up, recovered and connected on links within 50 ms when \
                  chaos ends (400 seeds, two workloads; 150 seeds run to the end of \
                  chaos and judged on the real cluster); a hand-edited file is \
                  refused as malformed (unknown fields, bad keys, values, IDs) or as \
                  scenario invalid (§10.16: unbounded chaos or steps, a plan clients \
                  would not generate, steps whose same-instant order could matter, \
                  nodes left down or paused, links left cut or slow); and a file \
                  reads back to its scenario and reproduces its seed's run exactly \
                  (same trace hash). Armed crash points are not in the file. \
                  Simulation evidence from a few hundred seeds, not proof",
        kind: Kind::TestGroup,
        covers: &["T-10"],
        class: ClaimClass::SimulationEvidenced,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "sim",
            filters: &["swarm_tests::", "scenario_tests::"],
        },
        min_cases: 31,
    },
    CheckDef {
        id: "K-3",
        summary: "Real-time order K-3 (§10.5, W38.2): over logical writes (attempts collapsed by request ID), a write invoked after another write's first terminal decision was delivered to a client executes at a larger log index, with indices taken from the reference model's replay of the committed log and never from a response; a retry returning its original smaller index never violates it, an undelivered answer is no observation, a delivered known status answer is, and a misuse attempt is neither an invocation nor an observation. Hand-made histories, each rule broken once: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["C-1"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k3::tests::"],
        },
        min_cases: 11,
    },
    CheckDef {
        id: "K-4",
        summary: "Model-replay results K-4 (§10.5, W38.2): replaying the committed log through the reference model reproduces every reported write result, namely the index, the version (DM-3), existed (F-3), a CAS mismatch with the correct current version or its absence (F-5), a CAS success against a model mismatch and the reverse, capacity outcomes and the limit hit (DM-6) and a result of the wrong kind, and a request_id_conflict names the first execution's index and answers only a conflicting reuse (F-6); status answers are judged like responses and misuse attempts are left to K-2. Hand-made histories, each rule broken once, with retries returning the original index as controls: test evidence of the checker, not proof",
        kind: Kind::TestGroup,
        covers: &["C-1", "C-5", "F-1", "F-3", "F-5", "F-6", "DM-3", "DM-6"],
        class: ClaimClass::Invariant,
        tier: Tier::T0,
        selector: Selector::CargoTest {
            package: "checker",
            filters: &["k4::tests::"],
        },
        min_cases: 17,
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
    /// Findings a report-mode condition lists without failing the check (§10.13
    /// conditions 5 and 6 until W32).
    pub reported: Vec<String>,
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
            reported: Vec::new(),
            output_tail: None,
        }
    }

    fn fail(&mut self, detail: String) {
        self.status = CheckStatus::Failed;
        self.detail = Some(detail);
    }
}

/// The `./dev check` report (`keyval.dev.check/v2`).
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
    let (outcome, reported) = match check.run_reporting(root) {
        Ok(run) => run,
        Err(why) => return result.fail(format!("the check could not run: {why}")),
    };
    result.reported = reported;
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
            for line in &check.reported {
                let _ = writeln!(out, "  reported: {line}");
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
