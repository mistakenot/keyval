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

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

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

/// Reads `<root>/registry/workgraph.toml`, or `None` if it does not exist yet.
pub fn load_workgraph(root: &Path) -> Result<Option<Vec<Item>>, String> {
    read_optional(&root.join(WORKGRAPH))?
        .map(|text| parse_workgraph(&text))
        .transpose()
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// The work item a bead implements: the item whose recorded `bead` is `bead`, else the
/// item named by the bead's `plan:W<nn>` label (until W06's import records bead IDs).
pub fn item_for_bead<'a>(items: &'a [Item], bead: &str, labels: &[String]) -> Option<&'a Item> {
    items.iter().find(|item| item.bead == bead).or_else(|| {
        let plan = labels
            .iter()
            .find_map(|label| label.strip_prefix("plan:"))?;
        items.iter().find(|item| item.id == plan)
    })
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

/// Reads `<root>/registry/requirements.toml`, or `None` if it does not exist yet.
pub fn load_requirements_text(root: &Path) -> Result<Option<String>, String> {
    read_optional(&root.join(REQUIREMENTS))
}

/// The requirements that become active when `closed` (a canonical item's ID) closes,
/// given every item's closed-ness after the closure (`is_closed(item_id)`).
pub fn newly_active(
    items: &[Item],
    requirements: &[Requirement],
    closed: &str,
    is_closed: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let Some(item) = items.iter().find(|item| item.id == closed) else {
        return Vec::new();
    };
    item.reqs
        .iter()
        .filter(|req| {
            requirements
                .iter()
                .any(|r| &r.id == *req && r.status == "planned")
        })
        .filter(|req| {
            items
                .iter()
                .filter(|other| other.reqs.contains(req))
                .all(|other| other.id == closed || is_closed(&other.id))
        })
        .cloned()
        .collect()
}

/// `text` with the status of each requirement in `ids` changed from `planned` to
/// `active`, editing only those `status` lines so comments and layout survive.
pub fn activate(text: &str, ids: &[String]) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut current: Option<String> = None;
    let mut changed: BTreeMap<String, bool> = ids.iter().map(|id| (id.clone(), false)).collect();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("[[") || trimmed.starts_with('[') {
            current = None;
        }
        if let Some(value) = key_value(trimmed, "id") {
            current = Some(value);
        }
        let target = current.as_ref().filter(|id| changed.contains_key(*id));
        if let (Some(id), Some(status)) = (target, key_value(trimmed, "status")) {
            if status != "planned" {
                return Err(format!("{REQUIREMENTS}: {id} is {status}, not planned"));
            }
            out.push_str(&line.replacen("\"planned\"", "\"active\"", 1));
            changed.insert(id.clone(), true);
            continue;
        }
        out.push_str(line);
    }
    let missing: Vec<&String> = changed
        .iter()
        .filter(|(_, done)| !**done)
        .map(|(id, _)| id)
        .collect();
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "{REQUIREMENTS}: no planned status line for {missing:?}"
        ))
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
    fn a_bead_maps_to_its_item_by_recorded_id_or_plan_label() {
        let items = items();
        let by_bead = item_for_bead(&items, "br-w49-status", &[]);
        assert_eq!(by_bead.map(|i| i.id.as_str()), Some("W49"));
        let labels = vec!["m0".to_owned(), "plan:W01".to_owned()];
        let by_label = item_for_bead(&items, "br-w01-x", &labels);
        assert_eq!(by_label.map(|i| i.id.as_str()), Some("W01"));
        let subtask = item_for_bead(&items, "br-w01-1-x", &["sub:W01".to_owned()]);
        assert_eq!(
            subtask, None,
            "rule: a subtask is never its parent's work item"
        );
    }

    #[test]
    fn a_requirement_activates_only_when_its_last_listing_item_closes() {
        let (items, reqs) = (items(), requirements());
        let w49_open = |id: &str| id != "W49";
        assert_eq!(
            newly_active(&items, &reqs, "W01", &w49_open),
            Vec::<String>::new(),
            "rule: O-8 stays planned while W49, which also lists it, is open (§10.13)"
        );
        let all_closed = |_: &str| true;
        assert_eq!(
            newly_active(&items, &reqs, "W49", &all_closed),
            ["O-8"],
            "rule: the last listing item's closure activates it"
        );
    }

    #[test]
    fn only_section_6_requirements_activate_and_unknown_items_activate_nothing() {
        let (items, reqs) = (items(), requirements());
        let all_closed = |_: &str| true;
        let activated = newly_active(&items, &reqs, "W01", &all_closed);
        assert_eq!(
            activated,
            ["O-8"],
            "rule: P7 and CD-8 are principles, not §6 rows, so they never activate"
        );
        assert_eq!(
            newly_active(&items, &reqs, "W01.3", &all_closed),
            Vec::<String>::new(),
            "rule: a subtask closing before its parent activates nothing"
        );
    }

    #[test]
    fn activation_edits_only_the_status_lines() {
        let edited = activate(REQUIREMENTS_FIXTURE, &["O-8".to_owned()]).unwrap();
        let reparsed = parse_requirements(&edited).unwrap();
        assert_eq!(reparsed[0].status, "active");
        assert_eq!(reparsed[1].status, "planned");
        assert!(
            edited.contains("status = \"active\" # derived by ./dev commit --close"),
            "rule: comments and layout survive; edited=\n{edited}"
        );
        assert_eq!(edited.lines().count(), REQUIREMENTS_FIXTURE.lines().count());
        assert!(
            activate(&edited, &["O-8".to_owned()]).is_err(),
            "already active"
        );
        assert!(
            activate(REQUIREMENTS_FIXTURE, &["X-1".to_owned()]).is_err(),
            "unknown ID"
        );
    }

    #[test]
    fn hand_written_statuses_other_than_planned_or_active_are_rejected() {
        let text = "[[requirement]]\nid = \"O-8\"\nstatus = \"passed\"\n";
        assert!(parse_requirements(text).is_err_and(|e| e.contains("passed")));
    }
}
