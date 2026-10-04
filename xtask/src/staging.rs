//! Registry-driven staging for `./dev gate fast` (PLAN.md §10.10 "Staged contents",
//! §10.11, §10.13 "Status lifecycle"; W05.4).
//!
//! - **What runs.** T0 runs every `registry/checks.toml` row with tier `T0` and status
//!   `implemented`, in file order, through its catalogue selector. A `planned` T0 row is
//!   reported with the work item whose `accept` list names it, and never fails the tier.
//! - **Judged from one run.** A cargo-test check is judged from its package's single
//!   test run in the gate (`cargo test -p <package> --all-targets`), never re-run: every
//!   case its filters select passes, none is ignored, and at least the row's `min_cases`
//!   ran (§10.11; owner decision, 2026-10-04).
//! - **Forward-only rows** (D-56). A row of the parent commit's `checks.toml` that the
//!   tree removes, an `implemented` row it marks anything else, and an `implemented` row
//!   whose `min_cases` it lowers or that it moves to a later tier (§10.6 "silent
//!   reclassification"; br-forward-only-min-cases-tier-xo5i) fail, unless
//!   `--allow-demotion <ID>=<bead>` names a closed bead labelled `plan-change` in the
//!   tree's tracker export (owner decision, 2026-10-04;
//!   br-registry-check-removal-passes-kdo9). An allowance that matches no demotion
//!   fails too, so none outlives its purpose.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::check::CheckDef;
use crate::libtest::Outcome;
use crate::workgraph::WorkItem;

#[derive(Deserialize)]
struct ChecksFile {
    #[serde(default)]
    check: Vec<Row>,
}

/// The fields of a `checks.toml` row that staging reads; the registry gate validates the
/// rest.
#[derive(Deserialize)]
struct Row {
    id: String,
    tier: String,
    status: String,
    min_cases: u32,
}

fn rows(checks: &str) -> Result<Vec<Row>, String> {
    toml::from_str::<ChecksFile>(checks)
        .map(|file| file.check)
        .map_err(|err| format!("invalid registry/checks.toml: {}", err.message()))
}

/// A catalogue check the tier runs, with the row's minimum case count.
#[derive(Clone, Copy, Debug)]
pub struct Staged {
    /// The catalogue entry, whose selector runs the check.
    pub def: &'static CheckDef,
    /// The row's `min_cases`.
    pub min_cases: u32,
}

/// A `planned` row the tier reports instead of running (§10.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Planned {
    /// The check ID.
    pub id: String,
    /// The work item whose `accept` list names the check; `None` until one does.
    pub work_item: Option<String>,
}

/// What a tier runs, and what it reports.
#[derive(Debug, Default)]
pub struct Staging {
    /// The checks to run, in `checks.toml` order.
    pub run: Vec<Staged>,
    /// The tier's `planned` rows: reported, never failed.
    pub planned: Vec<Planned>,
    /// The tier's rows examined.
    pub rows: usize,
    /// Rows the tier must run but cannot: each fails the staging step.
    pub problems: Vec<String>,
}

/// The rows of `tier` in `checks`, resolved against `catalogue`. `items` (the work graph)
/// names the work item of each `planned` row.
pub fn stage(
    checks: &str,
    items: &[WorkItem],
    tier: &str,
    catalogue: &'static [CheckDef],
) -> Result<Staging, String> {
    let mut staging = Staging::default();
    for row in rows(checks)?.into_iter().filter(|row| row.tier == tier) {
        staging.rows += 1;
        match row.status.as_str() {
            "implemented" => match catalogue.iter().find(|def| def.id == row.id) {
                Some(def) => staging.run.push(Staged {
                    def,
                    min_cases: row.min_cases,
                }),
                None => staging.problems.push(format!(
                    "implemented check {} has no catalogue selector, so the tier cannot run \
                     it (§10.13 condition 4)",
                    row.id
                )),
            },
            "planned" => staging.planned.push(Planned {
                work_item: work_item_for(&row.id, items),
                id: row.id,
            }),
            other => staging.problems.push(format!(
                "check {} has status {other:?}; the tier runs `implemented` rows and reports \
                 `planned` ones",
                row.id
            )),
        }
    }
    Ok(staging)
}

/// The first work item whose `accept` list runs `./dev check` with `id`: any selector
/// naming it among its arguments, so multi-ID and `--case` selectors count too. REGISTRY-FILES
/// matches a planned row the same way (br-cirk).
pub fn work_item_for(id: &str, items: &[WorkItem]) -> Option<String> {
    items
        .iter()
        .find(|item| {
            item.accept.iter().any(|selector| {
                selector
                    .strip_prefix("./dev check ")
                    .is_some_and(|args| args.split_whitespace().any(|arg| arg == id))
            })
        })
        .map(|item| item.id.clone())
}

/// Which declared field of a `checks.toml` row moved backwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The status: the row was removed, or an `implemented` row was marked otherwise.
    Status,
    /// An `implemented` row's `min_cases` decreased.
    MinCases,
    /// An `implemented` row moved to a later tier, out of the gates that ran it.
    Tier,
}

/// A check whose declared row moved backwards (D-56).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Demotion {
    /// The check ID.
    pub id: String,
    /// What moved.
    pub field: Field,
    /// Its value in the parent commit.
    pub from: String,
    /// Its value in the tree; `None` when the tree removed the row.
    pub to: Option<String>,
    /// The closed `plan-change` bead that approved it, once approved.
    pub plan_change: Option<String>,
}

impl Demotion {
    /// The move, e.g. `implemented → planned`, `min_cases 19 → 1` or `tier T0 → T1`.
    pub fn describe(&self) -> String {
        let to = self.to.as_deref().unwrap_or("removed");
        match self.field {
            Field::Status => format!("{} → {to}", self.from),
            Field::MinCases => format!("min_cases {} → {to}", self.from),
            Field::Tier => format!("tier {} → {to}", self.from),
        }
    }
}

/// Compares the parent commit's `checks.toml` with the tree's. A demotion is a removed
/// row, an `implemented` row marked anything else, and, for a row `implemented` on both
/// sides, a lower `min_cases` or a later tier (§10.6, §10.13;
/// br-forward-only-min-cases-tier-xo5i). Raising `min_cases`, moving to an earlier tier
/// and promotions are free. Returns the rows compared and the demotions found, in parent
/// order. With no parent registry, every row of the tree is new.
pub fn demotions(parent: Option<&str>, tree: &str) -> Result<(usize, Vec<Demotion>), String> {
    let after = rows(tree)?;
    let Some(parent) = parent else {
        return Ok((after.len(), Vec::new()));
    };
    let before = rows(parent)?;
    let mut found = Vec::new();
    for old in &before {
        let demotion = |field: Field, from: String, to: Option<String>| Demotion {
            id: old.id.clone(),
            field,
            from,
            to,
            plan_change: None,
        };
        let Some(new) = after.iter().find(|row| row.id == old.id) else {
            found.push(demotion(Field::Status, old.status.clone(), None));
            continue;
        };
        if old.status != "implemented" {
            continue;
        }
        if new.status != old.status {
            found.push(demotion(
                Field::Status,
                old.status.clone(),
                Some(new.status.clone()),
            ));
            continue;
        }
        if new.min_cases < old.min_cases {
            found.push(demotion(
                Field::MinCases,
                old.min_cases.to_string(),
                Some(new.min_cases.to_string()),
            ));
        }
        if later_tier(&old.tier, &new.tier) {
            found.push(demotion(
                Field::Tier,
                old.tier.clone(),
                Some(new.tier.clone()),
            ));
        }
    }
    Ok((before.len(), found))
}

/// Whether `new` is a later tier than `old` (T0, T1, T2, T3): a check moved there runs less
/// often. A tier that does not read as `T<n>` counts as later unless it is unchanged, so an
/// unreadable tier never takes a check out of T0 unnoticed.
fn later_tier(old: &str, new: &str) -> bool {
    let rank = |tier: &str| tier.strip_prefix('T').and_then(|n| n.parse::<u32>().ok());
    match (rank(old), rank(new)) {
        (Some(before), Some(after)) => after > before,
        _ => old != new,
    }
}

/// `--allow-demotion <CHECK-ID>=<bead>`: the bead is the owner-approved plan change that
/// covers demoting the check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowance {
    /// The check ID.
    pub check: String,
    /// The `plan-change` bead.
    pub bead: String,
}

impl FromStr for Allowance {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.split_once('=') {
            Some((check, bead)) if !check.trim().is_empty() && !bead.trim().is_empty() => {
                Ok(Self {
                    check: check.trim().to_owned(),
                    bead: bead.trim().to_owned(),
                })
            }
            _ => Err(format!(
                "--allow-demotion {text:?} is not <CHECK-ID>=<plan-change bead>"
            )),
        }
    }
}

/// Approves each demotion an allowance covers, if its bead is a closed `plan-change` bead
/// in the tracker `export`. Returns the approved demotions and one violation for every
/// demotion left unapproved and every allowance that covers no demotion.
pub fn approve(
    found: Vec<Demotion>,
    allowances: &[Allowance],
    export: &str,
) -> (Vec<Demotion>, Vec<String>) {
    let demoted: Vec<String> = found.iter().map(|d| d.id.clone()).collect();
    let mut approved = Vec::new();
    let mut violations = Vec::new();
    for mut demotion in found {
        let moved = demotion.describe();
        let Some(allowance) = allowances.iter().find(|a| a.check == demotion.id) else {
            violations.push(format!(
                "check {} moved backwards ({moved}); a check's declared status, min_cases and \
                 tier are forward-only, and only an owner-approved plan change may move one \
                 back: --allow-demotion {}=<closed plan-change bead> (D-56, §10.6, §10.13)",
                demotion.id, demotion.id
            ));
            continue;
        };
        match plan_change_closed(&allowance.bead, export) {
            Ok(()) => {
                demotion.plan_change = Some(allowance.bead.clone());
                approved.push(demotion);
            }
            Err(why) => violations.push(format!(
                "check {} moved backwards ({moved}), but --allow-demotion names {}, {why}",
                demotion.id, allowance.bead
            )),
        }
    }
    for allowance in allowances {
        if !demoted.contains(&allowance.check) {
            violations.push(format!(
                "--allow-demotion {}={} covers no demotion: check {} did not move backwards; \
                 drop the flag",
                allowance.check, allowance.bead, allowance.check
            ));
        }
    }
    (approved, violations)
}

/// Whether `bead` is closed and labelled `plan-change` in the tracker export.
fn plan_change_closed(bead: &str, export: &str) -> Result<(), String> {
    let found = export
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| entry["id"] == bead)
        .ok_or_else(|| "which the tree's tracker export lacks".to_owned())?;
    let labelled = found["labels"]
        .as_array()
        .is_some_and(|labels| labels.iter().any(|label| label == "plan-change"));
    if !labelled {
        return Err("which is not labelled plan-change".to_owned());
    }
    if found["status"] != "closed" {
        return Err(format!(
            "which is {}, not closed: an open plan change is not yet decided",
            found["status"]
        ));
    }
    Ok(())
}

/// One package's test run in the gate: its per-case outcomes, and why the run itself
/// failed, if it did.
#[derive(Clone, Debug)]
pub struct PackageRun {
    /// The cargo package.
    pub package: String,
    /// Every case's outcome, in output order.
    pub outcomes: Vec<(String, Outcome)>,
    /// Why cargo did not finish cleanly (a build error, a failing case, a timeout).
    pub failure: Option<String>,
    /// Why the outcomes cannot be trusted, if they cannot: the run stopped early or did
    /// not add up ([`crate::libtest::reconcile`]). A failure outside a check's selection
    /// leaves the run complete; an incomplete run fails every check of its package.
    pub incomplete: Option<String>,
}

/// Judges a cargo-test check from its package's run: the run must be complete, the cases
/// `filters` select (all of them when empty) must all pass, none may be ignored, and at
/// least `min_cases` must have run (§10.11, §10.13, P10). Returns the passed cases.
pub fn judge_cargo(
    package: &str,
    filters: &[&str],
    min_cases: u32,
    runs: &[PackageRun],
) -> Result<u64, String> {
    let run = runs
        .iter()
        .find(|run| run.package == package)
        .ok_or_else(|| {
            format!("package {package} had no test run in this gate; is it a workspace member?")
        })?;
    if let Some(why) = &run.incomplete {
        // A family is never reported passed while any of its cases may be missing (§10.13,
        // br-judge-cargo-partial-run-afvw).
        return Err(format!(
            "package {package}'s test run is incomplete, so none of its outcomes can be \
             trusted: {why}"
        ));
    }
    let selected: Vec<&(String, Outcome)> = run
        .outcomes
        .iter()
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|f| name.contains(f)))
        .collect();
    if selected.is_empty() {
        return Err(run.failure.as_ref().map_or_else(
            || {
                format!(
                    "the selection matched zero cases of package {package} (filters \
                     {filters:?}); a zero-case selection is a failure, never a pass (P10)"
                )
            },
            |why| format!("package {package}'s test run failed and selected no case: {why}"),
        ));
    }
    let named = |wanted: Outcome| -> Vec<&str> {
        selected
            .iter()
            .filter(|(_, outcome)| *outcome == wanted)
            .map(|(name, _)| name.as_str())
            .collect()
    };
    let failed = named(Outcome::Failed);
    if !failed.is_empty() {
        return Err(format!(
            "{} case(s) failed: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    let ignored = named(Outcome::Ignored);
    if !ignored.is_empty() {
        return Err(format!(
            "{} case(s) are #[ignore]d: {}; an ignored case fails (P10)",
            ignored.len(),
            ignored.join(", ")
        ));
    }
    let passed = u64::try_from(named(Outcome::Passed).len()).unwrap_or(u64::MAX);
    if passed < u64::from(min_cases) {
        return Err(format!(
            "{passed} case(s) passed, fewer than the registered min_cases {min_cases} (§10.11)"
        ));
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    // check: GATE-STAGING
    use super::*;
    use crate::check::{ClaimClass, Kind, Selector, StaticCheck, Tier};

    const CATALOGUE: &[CheckDef] = &[
        CheckDef {
            id: "A-TESTS",
            summary: "a",
            kind: Kind::TestGroup,
            covers: &["P5"],
            class: ClaimClass::Invariant,
            tier: Tier::T0,
            selector: Selector::CargoTest {
                package: "alpha",
                filters: &["a::"],
            },
            min_cases: 2,
        },
        CheckDef {
            id: "B-STATIC",
            summary: "b",
            kind: Kind::Static,
            covers: &["P5"],
            class: ClaimClass::Invariant,
            tier: Tier::T0,
            selector: Selector::Static(StaticCheck::LintAllows),
            min_cases: 1,
        },
    ];

    fn row(id: &str, tier: &str, status: &str) -> String {
        format!(
            "\n[[check]]\nid = \"{id}\"\nsummary = \"x\"\nkind = \"static\"\ncovers = [\"P5\"]\n\
             class = \"invariant\"\ntier = \"{tier}\"\nrun = \"\"\nmin_cases = 3\n\
             status = \"{status}\"\n"
        )
    }

    fn checks(rows: &[String]) -> String {
        format!("schema = \"keyval.registry.checks/v1\"\n{}", rows.concat())
    }

    fn item(id: &str, accept: &[&str]) -> WorkItem {
        WorkItem {
            id: id.to_owned(),
            title: String::new(),
            deps: Vec::new(),
            reqs: Vec::new(),
            area: Vec::new(),
            milestone: "m0".to_owned(),
            accept: accept.iter().map(|&s| s.to_owned()).collect(),
            bead: String::new(),
        }
    }

    #[test]
    fn the_tier_runs_its_implemented_rows_in_order_and_reports_planned_ones() {
        let text = checks(&[
            row("B-STATIC", "T0", "implemented"),
            row("A-TESTS", "T0", "implemented"),
            row("C-LATER", "T0", "planned"),
            row("D-NIGHTLY", "T2", "implemented"),
            row("E-ORPHAN", "T0", "planned"),
        ]);
        let items = [
            item("W07", &["./dev check XTASK"]),
            item("W09", &["./dev check K-1 C-LATER --case x"]),
        ];
        let staging = stage(&text, &items, "T0", CATALOGUE).unwrap();
        let run: Vec<(&str, u32)> = staging
            .run
            .iter()
            .map(|s| (s.def.id, s.min_cases))
            .collect();
        assert_eq!(
            run,
            [("B-STATIC", 3), ("A-TESTS", 3)],
            "rule: implemented T0 rows run in checks.toml order with the row's min_cases; \
             staging={staging:#?}"
        );
        assert_eq!(
            staging.planned,
            [
                Planned {
                    id: "C-LATER".to_owned(),
                    work_item: Some("W09".to_owned()),
                },
                Planned {
                    id: "E-ORPHAN".to_owned(),
                    work_item: None,
                },
            ],
            "rule: planned rows are reported with the work item whose accept list names them \
             (§10.10)"
        );
        assert_eq!(staging.rows, 4, "rule: only the tier's rows are examined");
        assert_eq!(staging.problems, Vec::<String>::new());
    }

    #[test]
    fn an_implemented_row_the_tier_cannot_run_is_a_problem() {
        let text = checks(&[
            row("Z-GONE", "T0", "implemented"),
            row("Y-ODD", "T0", "retired"),
        ]);
        let staging = stage(&text, &[], "T0", CATALOGUE).unwrap();
        assert_eq!(staging.problems.len(), 2, "staging={staging:#?}");
        assert!(
            staging.problems[0].contains("Z-GONE") && staging.problems[1].contains("Y-ODD"),
            "staging={staging:#?}"
        );
        assert!(
            stage("check = 3", &[], "T0", CATALOGUE).is_err(),
            "rule: an unreadable registry is an error, never an empty tier"
        );
    }

    #[test]
    fn removing_a_row_or_demoting_an_implemented_one_is_a_demotion() {
        let parent = checks(&[
            row("A", "T0", "implemented"),
            row("B", "T0", "implemented"),
            row("C", "T0", "planned"),
            row("D", "T0", "planned"),
            row("E", "T0", "implemented"),
        ]);
        let tree = checks(&[
            row("A", "T0", "implemented"),
            row("B", "T0", "planned"),
            row("D", "T0", "implemented"),
            row("F", "T0", "planned"),
        ]);
        assert_eq!(
            demotions(None, &tree).unwrap(),
            (4, Vec::new()),
            "rule: without a parent registry every row is new"
        );
        let (cases, found) = demotions(Some(&parent), &tree).unwrap();
        assert_eq!(cases, 5, "rule: every parent row is compared");
        let moves: Vec<(&str, &str, Option<&str>)> = found
            .iter()
            .map(|d| (d.id.as_str(), d.from.as_str(), d.to.as_deref()))
            .collect();
        assert_eq!(
            moves,
            [
                ("B", "implemented", Some("planned")),
                ("C", "planned", None),
                ("E", "implemented", None),
            ],
            "rule: removal (of any row) and implemented → planned move backwards; promotion \
             and new rows do not (D-56, kdo9)"
        );
    }

    /// A `checks.toml` row with its tier, status and `min_cases`.
    fn row_with(id: &str, tier: &str, status: &str, min_cases: u32) -> String {
        row(id, tier, status).replace("min_cases = 3", &format!("min_cases = {min_cases}"))
    }

    #[test]
    fn lowering_min_cases_or_moving_to_a_later_tier_is_a_demotion() {
        let parent = checks(&[
            row_with("A", "T0", "implemented", 5),
            row_with("B", "T0", "implemented", 5),
            row_with("C", "T1", "implemented", 5),
            row_with("D", "T0", "planned", 5),
            row_with("E", "T0", "implemented", 5),
            row_with("G", "T2", "implemented", 5),
            row_with("H", "T0", "implemented", 5),
        ]);
        let tree = checks(&[
            row_with("A", "T0", "implemented", 4),
            row_with("B", "T1", "implemented", 5),
            row_with("C", "T0", "implemented", 5),
            row_with("D", "T0", "planned", 1),
            row_with("E", "T0", "implemented", 9),
            row_with("G", "nightly", "implemented", 5),
            row_with("H", "T2", "implemented", 2),
        ]);
        let (cases, found) = demotions(Some(&parent), &tree).unwrap();
        assert_eq!(cases, 7);
        let moves: Vec<(&str, Field, String)> = found
            .iter()
            .map(|d| (d.id.as_str(), d.field, d.describe()))
            .collect();
        assert_eq!(
            moves,
            [
                ("A", Field::MinCases, "min_cases 5 → 4".to_owned()),
                ("B", Field::Tier, "tier T0 → T1".to_owned()),
                ("G", Field::Tier, "tier T2 → nightly".to_owned()),
                ("H", Field::MinCases, "min_cases 5 → 2".to_owned()),
                ("H", Field::Tier, "tier T0 → T2".to_owned()),
            ],
            "rule: an implemented row's min_cases never drops and it never moves to a later \
             (or unreadable) tier unnoticed; raising min_cases (E), an earlier tier (C) and a \
             planned row's numbers (D) are free (§10.6, §10.13, xo5i)"
        );
        let (approved, violations) = approve(found, &[allow("H=br-pc")], EXPORT);
        assert_eq!(
            approved.iter().map(Demotion::describe).collect::<Vec<_>>(),
            ["min_cases 5 → 2", "tier T0 → T2"],
            "rule: one plan change covers every move of its check"
        );
        assert_eq!(
            violations.len(),
            3,
            "rule: A, B and G each need an approval; violations={violations:#?}"
        );
        assert!(
            violations[0].contains("min_cases 5 → 4")
                && violations[0].contains("--allow-demotion A="),
            "violations={violations:#?}"
        );
    }

    const EXPORT: &str = "\
{\"id\":\"br-pc\",\"status\":\"closed\",\"labels\":[\"plan-change\",\"needs-owner\"]}
{\"id\":\"br-open\",\"status\":\"open\",\"labels\":[\"plan-change\"]}
{\"id\":\"br-task\",\"status\":\"closed\",\"labels\":[\"discovered\"]}
";

    fn demoted(id: &str) -> Demotion {
        Demotion {
            id: id.to_owned(),
            field: Field::Status,
            from: "implemented".to_owned(),
            to: None,
            plan_change: None,
        }
    }

    fn allow(text: &str) -> Allowance {
        text.parse().unwrap()
    }

    #[test]
    fn only_a_closed_plan_change_bead_approves_a_demotion() {
        let (approved, violations) = approve(vec![demoted("A")], &[], EXPORT);
        assert!(approved.is_empty(), "approved={approved:#?}");
        assert!(
            violations.len() == 1 && violations[0].contains("--allow-demotion A="),
            "rule: an unapproved demotion fails and names the flag; violations={violations:#?}"
        );
        let (approved, violations) = approve(vec![demoted("A")], &[allow("A=br-pc")], EXPORT);
        assert_eq!(violations, Vec::<String>::new());
        assert_eq!(
            approved[0].plan_change.as_deref(),
            Some("br-pc"),
            "rule: the approving bead is recorded"
        );
        for (bead, why) in [
            ("br-open", "not closed"),
            ("br-task", "not labelled plan-change"),
            ("br-missing", "lacks"),
        ] {
            let (approved, violations) =
                approve(vec![demoted("A")], &[allow(&format!("A={bead}"))], EXPORT);
            assert!(
                approved.is_empty() && violations.len() == 1 && violations[0].contains(why),
                "rule: {bead} cannot approve ({why}); violations={violations:#?}"
            );
        }
    }

    #[test]
    fn an_allowance_that_covers_no_demotion_fails() {
        let (approved, violations) = approve(Vec::new(), &[allow("A=br-pc")], EXPORT);
        assert_eq!(approved, Vec::<Demotion>::new());
        assert!(
            violations.len() == 1 && violations[0].contains("covers no demotion"),
            "violations={violations:#?}"
        );
        assert!("A".parse::<Allowance>().is_err());
        assert!("=br-pc".parse::<Allowance>().is_err());
        assert!("A=".parse::<Allowance>().is_err());
    }

    fn run(outcomes: &[(&str, Outcome)], failure: Option<&str>) -> Vec<PackageRun> {
        vec![PackageRun {
            package: "alpha".to_owned(),
            outcomes: outcomes
                .iter()
                .map(|&(name, outcome)| (name.to_owned(), outcome))
                .collect(),
            failure: failure.map(str::to_owned),
            incomplete: None,
        }]
    }

    #[test]
    fn a_cargo_check_on_an_incomplete_package_run_fails_whatever_it_selected() {
        let mut runs = run(
            &[("a::one", Outcome::Passed), ("a::two", Outcome::Passed)],
            Some("cargo test exited unsuccessfully"),
        );
        assert_eq!(
            judge_cargo("alpha", &["a::"], 2, &runs),
            Ok(2),
            "a complete run with a failure outside the selection still judges the check"
        );
        runs[0].incomplete = Some("a test binary ... stopped before its summary".to_owned());
        let verdict = judge_cargo("alpha", &["a::"], 1, &runs);
        assert!(
            verdict
                .as_ref()
                .is_err_and(|e| e.contains("incomplete") && e.contains("stopped before")),
            "rule: a family is never reported passed while any of its cases may be missing, \
             even when the cases that did report passed and meet min_cases (§10.13, afvw); \
             verdict={verdict:?}"
        );
    }

    #[test]
    fn a_cargo_check_is_judged_from_its_package_run() {
        let runs = run(
            &[
                ("a::one", Outcome::Passed),
                ("a::two", Outcome::Passed),
                ("b::other", Outcome::Failed),
            ],
            Some("cargo test exited unsuccessfully"),
        );
        assert_eq!(
            judge_cargo("alpha", &["a::"], 2, &runs),
            Ok(2),
            "rule: only the selected cases count; a failure outside the selection is the \
             tests step's to report"
        );
        assert_eq!(
            judge_cargo("alpha", &[], 1, &runs).map_err(|e| e.contains("b::other")),
            Err(true)
        );
    }

    #[test]
    fn a_cargo_check_fails_on_zero_failed_ignored_or_too_few_cases() {
        let runs = run(
            &[
                ("a::one", Outcome::Passed),
                ("a::two", Outcome::Ignored),
                ("c::bad", Outcome::Failed),
            ],
            None,
        );
        let fails = |filters: &[&str], min: u32, why: &str| {
            let verdict = judge_cargo("alpha", filters, min, &runs);
            assert!(
                verdict.as_ref().is_err_and(|e| e.contains(why)),
                "rule: {why}; filters={filters:?} verdict={verdict:?}"
            );
        };
        fails(&["zzz::"], 1, "zero cases");
        fails(&["c::"], 1, "failed");
        fails(&["a::"], 1, "ignore");
        fails(&["a::one"], 2, "fewer than the registered min_cases 2");
        let verdict = judge_cargo("beta", &[], 1, &runs);
        assert!(
            verdict.as_ref().is_err_and(|e| e.contains("no test run")),
            "verdict={verdict:?}"
        );
        let broken = run(&[], Some("error[E0425]: cannot find value"));
        let verdict = judge_cargo("alpha", &[], 1, &broken);
        assert!(
            verdict.as_ref().is_err_and(|e| e.contains("E0425")),
            "rule: a run that built nothing says why; verdict={verdict:?}"
        );
    }
}
