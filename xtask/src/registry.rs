//! The parts of the verification registry (`registry/*.toml`, PLAN.md §10.13) that
//! acceptance and closure read: the work graph's acceptance selectors and requirement
//! lists, and requirement activation.
//!
//! **Activation rule (§10.13, D-70/D-72).** A requirement becomes `active` when **all**
//! work items listing it in their Reqs column have closed. Only §6 requirement IDs (rows
//! of `requirements.toml`) participate; principles, goals and section references in a
//! Reqs column are context. Only canonical work items count: closing a scoped subtask or
//! a discovered-work bead never activates anything, and a child may close before its
//! parent. Statuses are derived here and written by `./dev commit --close`, never by
//! hand (§11.5 rule 12).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::git::Git;

/// The work graph, relative to the root (W06).
pub const WORKGRAPH: &str = "registry/workgraph.toml";

/// The requirement registry, relative to the root (W05).
pub const REQUIREMENTS: &str = "registry/requirements.toml";

/// One `[[item]]` of `registry/workgraph.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Item {
    /// The §12.2 ID, e.g. `W07`.
    pub id: String,
    /// The Reqs cell, one entry per token.
    #[serde(default)]
    pub reqs: Vec<String>,
    /// Executable acceptance selectors, each a `./dev` command line.
    #[serde(default)]
    pub accept: Vec<String>,
    /// The tracker's bead ID, once `./dev beads import` has recorded it.
    #[serde(default)]
    pub bead: String,
}

#[derive(Deserialize)]
struct WorkgraphFile {
    #[serde(default)]
    item: Vec<Item>,
}

/// Parses `registry/workgraph.toml`.
pub fn parse_workgraph(text: &str) -> Result<Vec<Item>, String> {
    toml::from_str::<WorkgraphFile>(text)
        .map(|file| file.item)
        .map_err(|err| format!("invalid {WORKGRAPH}: {}", err.message()))
}

/// The work graph as committed in `tree` (a tree or commit), or `None` if it has none
/// yet. Acceptance and closure read the registry from committed trees only, never from
/// the shared checkout's working files, which other agents may be editing.
pub fn workgraph_at(git: &Git, tree: &str) -> Result<Option<Vec<Item>>, String> {
    git.file_at(tree, WORKGRAPH)?
        .map(|text| parse_workgraph(&text))
        .transpose()
}

/// The work item a bead implements: the item whose recorded `bead` is `bead`. A
/// `plan:<id>` label never identifies the item, because any bead can carry one; the
/// registry gate checks that the recorded bead, and only it, carries its item's label
/// (condition 8, br-item-closure-identity-unverified-xp14).
pub fn item_for_bead<'a>(items: &'a [Item], bead: &str) -> Option<&'a Item> {
    items
        .iter()
        .find(|item| !item.bead.is_empty() && item.bead == bead)
}

/// A requirement row: ID and declared status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// The §6 ID, e.g. `O-8`.
    pub id: String,
    /// `planned` or `active`.
    pub status: String,
}

#[derive(Deserialize)]
struct RequirementsFile {
    #[serde(default)]
    requirement: Vec<RequirementRow>,
}

#[derive(Deserialize)]
struct RequirementRow {
    id: String,
    status: String,
}

/// Parses `registry/requirements.toml` (`[[requirement]]` rows with `id` and `status`).
pub fn parse_requirements(text: &str) -> Result<Vec<Requirement>, String> {
    let file: RequirementsFile =
        toml::from_str(text).map_err(|err| format!("invalid {REQUIREMENTS}: {}", err.message()))?;
    file.requirement
        .into_iter()
        .map(|row| match row.status.as_str() {
            "planned" | "active" => Ok(Requirement {
                id: row.id,
                status: row.status,
            }),
            other => Err(format!(
                "{REQUIREMENTS}: requirement {} has status {other:?}; only planned or active",
                row.id
            )),
        })
        .collect()
}

/// The status changes a recomputation derives, in registry order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recomputed {
    /// Declared `planned`, derived `active`: every listing item has closed.
    pub activated: Vec<String>,
    /// Declared `active`, derived `planned`: a listing item is open again.
    pub demoted: Vec<String>,
}

impl Recomputed {
    /// Whether no status changes.
    pub const fn is_empty(&self) -> bool {
        self.activated.is_empty() && self.demoted.is_empty()
    }
}

/// The status recomputation hook that every `./dev commit --close` runs (W05.3): each
/// requirement whose declared status differs from its derivation given `closed`, the
/// closed work items as the closure commits them. It recomputes every requirement, both
/// ways, not just the closing item's: a closure also catches up an activation that a
/// closure outside the wrapper (an owner override) missed, and demotes the requirements
/// of an item whose bead was reopened. Condition 1 demands the exact derivation on every
/// tree (§10.13, br-item-closure-identity-unverified-xp14).
pub fn recompute(
    items: &[Item],
    requirements: &[Requirement],
    closed: &BTreeSet<String>,
) -> Recomputed {
    let mut changes = Recomputed::default();
    for r in requirements {
        let derived = derived_status(items, &r.id, closed);
        if r.status == "planned" && derived == "active" {
            changes.activated.push(r.id.clone());
        } else if r.status == "active" && derived == "planned" {
            changes.demoted.push(r.id.clone());
        }
    }
    changes
}

/// PLAN.md §6's requirement rows as `(id, level)`, in plan order: every table row whose
/// ID cell is a requirement ID, whatever its level cell holds, so that a malformed level
/// fails condition 1 instead of dropping the row (br-item-closure-identity-unverified-xp14).
pub fn plan_requirements(plan: &str) -> Vec<(String, String)> {
    let section = plan
        .split("\n## 6. Requirements")
        .nth(1)
        .and_then(|rest| rest.split("\n## 7.").next())
        .unwrap_or_default();
    section
        .lines()
        .filter_map(|line| {
            let mut cells = line.strip_prefix("| ")?.split(" | ");
            let (id, level) = (cells.next()?, cells.next()?);
            requirement_id(id).then(|| (id.to_owned(), level.to_owned()))
        })
        .collect()
}

/// The §6 requirement families.
pub const REQUIREMENT_FAMILIES: &[&str] = &["DM", "F", "C", "A", "DUR", "O", "T"];

/// `DM-1`, `DUR-9`, `F-10`: a §6 family and a number.
pub fn requirement_id(id: &str) -> bool {
    id.split_once('-').is_some_and(|(family, number)| {
        REQUIREMENT_FAMILIES.contains(&family)
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The work items whose recorded bead is closed in a tracker export
/// (`.beads/issues.jsonl`), judged as `./dev commit --close` judges them. A `plan:<id>`
/// label on any other bead closes nothing (br-item-closure-identity-unverified-xp14).
pub fn closed_items(items: &[Item], export: &str) -> BTreeSet<String> {
    let closed: BTreeSet<String> = export
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|bead| bead["status"] == "closed")
        .filter_map(|bead| bead["id"].as_str().map(str::to_owned))
        .collect();
    closed_by(items, &closed)
}

/// The work items whose recorded bead is one of `closed_beads`.
pub fn closed_by(items: &[Item], closed_beads: &BTreeSet<String>) -> BTreeSet<String> {
    items
        .iter()
        .filter(|item| !item.bead.is_empty() && closed_beads.contains(&item.bead))
        .map(|item| item.id.clone())
        .collect()
}

/// The status §10.13 derives for requirement `id`: `active` once every work item listing
/// it has closed, and `planned` while one is open or none lists it (never vacuously
/// active, P10).
pub fn derived_status(items: &[Item], id: &str, closed: &BTreeSet<String>) -> &'static str {
    let listing: Vec<&Item> = items
        .iter()
        .filter(|item| item.reqs.iter().any(|r| r == id))
        .collect();
    if !listing.is_empty() && listing.iter().all(|item| closed.contains(&item.id)) {
        "active"
    } else {
        "planned"
    }
}

/// `text` with `changes` applied: each activated requirement's status changed from
/// `planned` to `active`, each demoted one's from `active` to `planned`. Only those
/// `status` lines are edited, so comments and layout survive.
pub fn apply(text: &str, changes: &Recomputed) -> Result<String, String> {
    let flips = changes
        .activated
        .iter()
        .map(|id| (id, ("planned", "active")))
        .chain(changes.demoted.iter().map(|id| (id, ("active", "planned"))));
    let mut wanted: BTreeMap<String, (&str, &str, bool)> = BTreeMap::new();
    for (id, (from, to)) in flips {
        wanted.insert(id.clone(), (from, to, false));
    }
    let mut out = String::with_capacity(text.len());
    let mut current: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            current = None;
        }
        if let Some(value) = key_value(trimmed, "id") {
            current = Some(value);
        }
        let target = current
            .as_ref()
            .and_then(|id| wanted.get_mut(id).map(|w| (id, w)));
        if let (Some((id, (from, to, done))), Some(status)) = (target, key_value(trimmed, "status"))
        {
            if status != *from {
                return Err(format!("{REQUIREMENTS}: {id} is {status}, not {from}"));
            }
            out.push_str(&line.replacen(&format!("\"{from}\""), &format!("\"{to}\""), 1));
            *done = true;
            continue;
        }
        out.push_str(line);
    }
    let missing: Vec<&String> = wanted
        .iter()
        .filter(|(_, (_, _, done))| !*done)
        .map(|(id, _)| id)
        .collect();
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!("{REQUIREMENTS}: no status line for {missing:?}"))
    }
}

/// The string value of `key = "value"` on a TOML line, if that is what the line holds.
fn key_value(line: &str, key: &str) -> Option<String> {
    let rest = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    let value = rest.strip_prefix('"')?;
    let end = value.find('"')?;
    value.get(..end).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    // Selected by DEV-CLOSE (closure:: and registry::).
    use super::*;

    const WORKGRAPH_FIXTURE: &str = r#"
schema = "keyval.registry.workgraph/v1"

[[item]]
id = "W01"
title = "Bootstrap"
deps = []
reqs = ["P7", "CD-8", "O-8"]
area = ["root", "xtask"]
milestone = "m0"
accept = ["./dev check XTASK", "./dev check DEV-DOCTOR"]
bead = ""

[[item]]
id = "W49"
title = "Status view"
deps = ["W01"]
reqs = ["O-8"]
area = ["xtask"]
milestone = "m4"
accept = []
bead = "br-w49-status"
"#;

    const REQUIREMENTS_FIXTURE: &str = r#"# Requirements (fixture)
schema = "keyval.registry.requirements/v1"

[[requirement]]
id = "O-8"
level = "MUST"
status = "planned" # derived by ./dev commit --close
title = "./dev status and doctor"

[[requirement]]
id = "O-6"
level = "MUST"
status = "planned"
title = "Robot mode"
"#;

    fn items() -> Vec<Item> {
        parse_workgraph(WORKGRAPH_FIXTURE).unwrap()
    }

    fn requirements() -> Vec<Requirement> {
        parse_requirements(REQUIREMENTS_FIXTURE).unwrap()
    }

    #[test]
    fn workgraph_items_carry_reqs_selectors_and_beads() {
        let items = items();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].accept,
            ["./dev check XTASK", "./dev check DEV-DOCTOR"]
        );
        assert_eq!(items[1].bead, "br-w49-status");
    }

    #[test]
    fn a_bead_maps_to_its_item_only_by_the_recorded_id() {
        let items = items();
        let by_bead = item_for_bead(&items, "br-w49-status");
        assert_eq!(by_bead.map(|i| i.id.as_str()), Some("W49"));
        assert_eq!(
            item_for_bead(&items, "br-w01-x"),
            None,
            "rule: a bead that is not recorded for an item is no work item, whatever \
             `plan:` label it carries (br-item-closure-identity-unverified-xp14)"
        );
        assert_eq!(
            item_for_bead(&items, ""),
            None,
            "rule: W01 records no bead yet, and an empty ID names no bead"
        );
        assert_eq!(
            item_for_bead(&items, "br-w01-1-x"),
            None,
            "rule: a subtask is never its parent's work item"
        );
    }

    fn closed(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|&id| id.to_owned()).collect()
    }

    #[test]
    fn a_requirement_activates_only_when_its_last_listing_item_closes() {
        let (items, reqs) = (items(), requirements());
        assert_eq!(
            recompute(&items, &reqs, &closed(&["W01"])).activated,
            Vec::<String>::new(),
            "rule: O-8 stays planned while W49, which also lists it, is open (§10.13)"
        );
        assert_eq!(
            recompute(&items, &reqs, &closed(&["W01", "W49"])).activated,
            ["O-8"],
            "rule: the last listing item's closure activates it"
        );
    }

    #[test]
    fn only_section_6_requirements_activate_and_unknown_items_activate_nothing() {
        let (items, reqs) = (items(), requirements());
        assert_eq!(
            recompute(&items, &reqs, &closed(&["W01", "W49"])).activated,
            ["O-8"],
            "rule: P7 and CD-8 are principles, not §6 rows, so they never activate; O-6, \
             which no item lists, is never vacuously active (P10)"
        );
        assert_eq!(
            recompute(&items, &reqs, &closed(&["W01.3"])),
            Recomputed::default(),
            "rule: a subtask is no work item, so its closure activates nothing"
        );
    }

    #[test]
    fn the_recompute_catches_up_a_missed_activation_and_demotes_a_reopened_item() {
        let items = parse_workgraph(
            "[[item]]\nid = \"W01\"\nreqs = [\"O-8\"]\n\n\
             [[item]]\nid = \"W49\"\nreqs = [\"O-6\"]\n",
        )
        .unwrap();
        let reqs = requirements();
        assert_eq!(
            recompute(&items, &reqs, &closed(&["W01", "W49"])).activated,
            ["O-8", "O-6"],
            "rule: W49 closed earlier outside the wrapper; the next closure (W01's) also \
             activates O-6, in registry order (W05.3)"
        );
        let both = Recomputed {
            activated: vec!["O-8".to_owned(), "O-6".to_owned()],
            demoted: Vec::new(),
        };
        let active = parse_requirements(&apply(REQUIREMENTS_FIXTURE, &both).unwrap()).unwrap();
        assert_eq!(
            recompute(&items, &active, &closed(&["W49"])),
            Recomputed {
                activated: Vec::new(),
                demoted: vec!["O-8".to_owned()],
            },
            "rule: W01's bead was reopened, so O-8 derives planned again and is demoted; \
             condition 1 demands the exact derivation (br-item-closure-identity-unverified-\
             xp14). O-6, still covered by closed W49, stays active"
        );
    }

    fn activated(ids: &[&str]) -> Recomputed {
        Recomputed {
            activated: ids.iter().map(|&id| id.to_owned()).collect(),
            demoted: Vec::new(),
        }
    }

    #[test]
    fn activation_edits_only_the_status_lines() {
        let edited = apply(REQUIREMENTS_FIXTURE, &activated(&["O-8"])).unwrap();
        let reparsed = parse_requirements(&edited).unwrap();
        assert_eq!(reparsed[0].status, "active");
        assert_eq!(reparsed[1].status, "planned");
        assert!(
            edited.contains("status = \"active\" # derived by ./dev commit --close"),
            "rule: comments and layout survive; edited=\n{edited}"
        );
        assert_eq!(edited.lines().count(), REQUIREMENTS_FIXTURE.lines().count());
        assert!(
            apply(&edited, &activated(&["O-8"])).is_err(),
            "already active"
        );
        assert!(
            apply(REQUIREMENTS_FIXTURE, &activated(&["X-1"])).is_err(),
            "unknown ID"
        );
        let demoted = Recomputed {
            activated: Vec::new(),
            demoted: vec!["O-8".to_owned()],
        };
        let restored = apply(&edited, &demoted).unwrap();
        assert_eq!(
            restored, REQUIREMENTS_FIXTURE,
            "rule: a demotion edits the same line back, comments included"
        );
        assert!(
            apply(REQUIREMENTS_FIXTURE, &demoted).is_err(),
            "a planned requirement cannot be demoted"
        );
    }

    #[test]
    fn a_section_6_row_with_a_malformed_level_is_kept_for_condition_1() {
        let plan = "# Plan\n\n## 6. Requirements\n\n| ID | Level | Requirement |\n|---|---|---|\n\
                    | F-1 | MUST | PUT |\n| F-2 | **SHOULD** | GET |\n| P4 | MUST | not §6 |\n\
                    \n## 7. Architecture\n";
        assert_eq!(
            plan_requirements(plan),
            [
                ("F-1".to_owned(), "MUST".to_owned()),
                ("F-2".to_owned(), "**SHOULD**".to_owned())
            ],
            "rule: a requirement row is never dropped for its level cell; condition 1 fails \
             it (br-item-closure-identity-unverified-xp14)"
        );
    }

    #[test]
    fn hand_written_statuses_other_than_planned_or_active_are_rejected() {
        let text = "[[requirement]]\nid = \"O-8\"\nstatus = \"passed\"\n";
        assert!(parse_requirements(text).is_err_and(|e| e.contains("passed")));
    }
}

/// The committed registry files (W05.1): `requirements.toml` equals PLAN.md §6 with derived
/// statuses, and `checks.toml` mirrors the bootstrap catalogue in `check.rs`. W05.2 turns
/// these into the T0 registry gate's conditions; until then they hold the files in step.
#[cfg(test)]
mod files {
    // check: REGISTRY-FILES
    use super::*;
    use crate::check::CATALOGUE;
    use crate::test_support::workspace_root;
    use std::fs;

    const CHECKS: &str = "registry/checks.toml";
    const TRACKER_EXPORT: &str = ".beads/issues.jsonl";

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RequirementsFile {
        schema: String,
        requirement: Vec<RequirementEntry>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RequirementEntry {
        id: String,
        level: String,
        status: String,
        title: String,
        #[serde(default)]
        notes: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ChecksFile {
        schema: String,
        check: Vec<CheckEntry>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CheckEntry {
        id: String,
        summary: String,
        kind: String,
        covers: Vec<String>,
        api: Vec<String>,
        class: String,
        tier: String,
        run: String,
        min_cases: u32,
        status: String,
        #[serde(default)]
        catches: Vec<String>,
    }

    fn read(path: &str) -> String {
        let full = workspace_root().join(path);
        fs::read_to_string(&full).unwrap_or_else(|err| panic!("{}: {err}", full.display()))
    }

    fn requirements_file() -> RequirementsFile {
        toml::from_str(&read(REQUIREMENTS)).unwrap_or_else(|err| panic!("{REQUIREMENTS}: {err}"))
    }

    #[test]
    fn requirements_toml_equals_plan_section_6() {
        let file = requirements_file();
        assert_eq!(file.schema, "keyval.registry.requirements/v1");
        let rows: Vec<(String, String)> = file
            .requirement
            .iter()
            .map(|r| (r.id.clone(), r.level.clone()))
            .collect();
        let plan = plan_requirements(&read("PLAN.md"));
        assert!(plan.len() >= 60, "PLAN.md §6 parsed to {} rows", plan.len());
        assert_eq!(
            rows, plan,
            "rule: requirement IDs, levels and order equal PLAN.md §6; the plan wins (§0.4, \
             §10.13 condition 1)"
        );
        for r in &file.requirement {
            assert!(
                !r.title.trim().is_empty(),
                "rule: every row has a title; {r:?}"
            );
            assert!(
                r.level == "MUST" || r.notes.as_deref().is_some_and(|n| !n.trim().is_empty()),
                "rule: a SHOULD or MAY requirement records its disposition (§13); {r:?}"
            );
        }
        assert!(
            parse_requirements(&read(REQUIREMENTS)).is_ok(),
            "rule: statuses are planned or active"
        );
    }

    #[test]
    fn requirement_statuses_are_derived_from_the_work_graph_and_the_tracker() {
        let items = parse_workgraph(&read(WORKGRAPH)).unwrap();
        let closed = closed_items(&items, &read(TRACKER_EXPORT));
        let wrong: Vec<String> = requirements_file()
            .requirement
            .iter()
            .filter_map(|r| {
                let derived = derived_status(&items, &r.id, &closed);
                (r.status != derived)
                    .then(|| format!("{} is {} but derives {derived}", r.id, r.status))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "rule: a requirement's status is derived from the closed work items listing it, \
             never typed by hand (§10.13, §11.5 rule 12); a closure outside ./dev commit \
             --close must apply the activation too: {wrong:?} (closed items: {closed:?})"
        );
    }

    #[test]
    fn a_requirement_activates_only_when_every_listing_item_is_closed() {
        let items = parse_workgraph(
            "[[item]]\nid = \"W01\"\nreqs = [\"O-8\", \"P7\"]\n\n\
             [[item]]\nid = \"W49\"\nreqs = [\"O-8\"]\n",
        )
        .unwrap();
        let none = BTreeSet::new();
        let w01: BTreeSet<String> = ["W01".to_owned()].into();
        let both: BTreeSet<String> = ["W01".to_owned(), "W49".to_owned()].into();
        assert_eq!(derived_status(&items, "O-8", &none), "planned");
        assert_eq!(
            derived_status(&items, "O-8", &w01),
            "planned",
            "rule: one open listing item keeps it planned"
        );
        assert_eq!(derived_status(&items, "O-8", &both), "active");
        assert_eq!(
            derived_status(&items, "C-7", &both),
            "planned",
            "rule: a requirement no item lists is never vacuously active (P10)"
        );
        let recorded = parse_workgraph(
            "[[item]]\nid = \"W01\"\nreqs = [\"O-8\"]\nbead = \"br-a\"\n\n\
             [[item]]\nid = \"W49\"\nreqs = [\"O-8\"]\nbead = \"br-b\"\n",
        )
        .unwrap();
        let export = "{\"id\":\"br-a\",\"status\":\"closed\",\"labels\":[\"plan:W01\"]}\n\
                      {\"id\":\"br-b\",\"status\":\"open\",\"labels\":[\"plan:W49\"]}\n\
                      {\"id\":\"br-x\",\"status\":\"closed\",\"labels\":[\"plan:W49\"]}\n";
        assert_eq!(
            closed_items(&recorded, export),
            w01,
            "rule: only the recorded bead closes an item; br-x, mislabelled plan:W49, closes \
             nothing (br-item-closure-identity-unverified-xp14)"
        );
        assert!(
            closed_items(&items, export).is_empty(),
            "rule: an item that records no bead is never closed"
        );
    }

    #[test]
    fn checks_toml_mirrors_the_bootstrap_catalogue() {
        let file: ChecksFile =
            toml::from_str(&read(CHECKS)).unwrap_or_else(|err| panic!("{CHECKS}: {err}"));
        assert_eq!(file.schema, "keyval.registry.checks/v1");
        // Implemented rows mirror the catalogue exactly, in order. A planned row has no
        // executable yet, so it is not in the catalogue; the work item whose accept list
        // names it will implement it (W05.2's planned rows; owner decision on
        // br-w10-3-differential-tests-6ez).
        let (implemented, planned): (Vec<&CheckEntry>, Vec<&CheckEntry>) =
            file.check.iter().partition(|c| c.status == "implemented");
        let ids: Vec<&str> = implemented.iter().map(|c| c.id.as_str()).collect();
        let catalogue: Vec<&str> = CATALOGUE.iter().map(|def| def.id).collect();
        assert_eq!(
            ids, catalogue,
            "rule: checks.toml has one implemented row per catalogue check, in catalogue \
             order; a new check lands with its row (§11.5 rule 12)"
        );
        let graph = crate::workgraph::parse(&read("registry/workgraph.toml"))
            .unwrap_or_else(|err| panic!("registry/workgraph.toml: {err}"));
        let accepted: BTreeSet<&str> = graph
            .iter()
            .flat_map(|item| item.accept.iter().map(String::as_str))
            .collect();
        for row in &planned {
            assert!(
                row.status == "planned"
                    && row.min_cases > 0
                    && !catalogue.contains(&row.id.as_str())
                    && accepted.contains(format!("./dev check {}", row.id).as_str()),
                "rule: every other row is planned, has a floor, has no catalogue entry yet, and \
                 is named by the accept list of the work item that implements it; {row:?}"
            );
            assert!(
                row.api.is_empty() && row.catches.is_empty(),
                "rule: no current check names API operations or catches bug variants; {row:?}"
            );
        }
        for (row, def) in implemented.iter().zip(CATALOGUE) {
            let json = |value: serde_json::Value| value.as_str().unwrap_or_default().to_owned();
            let expected = (
                def.summary.to_owned(),
                json(serde_json::to_value(def.kind).unwrap()),
                def.covers
                    .iter()
                    .map(|c| (*c).to_owned())
                    .collect::<Vec<_>>(),
                json(serde_json::to_value(def.class).unwrap()),
                json(serde_json::to_value(def.tier).unwrap()),
                format!("./dev check {}", def.id),
                def.min_cases,
            );
            let observed = (
                row.summary.clone(),
                row.kind.clone(),
                row.covers.clone(),
                row.class.clone(),
                row.tier.clone(),
                row.run.clone(),
                row.min_cases,
            );
            assert_eq!(
                observed, expected,
                "rule: checks.toml row {} mirrors check.rs",
                def.id
            );
            assert!(
                row.min_cases > 0 && row.status == "implemented",
                "rule: min_cases is never zero, and every catalogue check is implemented \
                 (D-56); {row:?}"
            );
            assert!(
                row.api.is_empty() && row.catches.is_empty(),
                "rule: no current check names API operations or catches bug variants; {row:?}"
            );
        }
    }
}
