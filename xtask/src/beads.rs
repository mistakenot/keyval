//! `./dev beads import [--dry-run]`: one bead per work item, with the work graph's
//! dependencies, and the plan-ID ↔ bead-ID mapping recorded in the work graph
//! (PLAN.md §11.11, §11.2 step 3; W06).
//!
//! - **Adopt or create.** A bead belongs to a work item by its `plan:W<nn>` label. One
//!   such bead is adopted as it is: no field of an adopted bead is ever edited, so
//!   acceptance additions and notes survive. None creates one, labelled with the item's
//!   milestone and joined to that milestone's epic. More than one is refused, as is a
//!   recorded bead ID that disagrees with the label, or a `plan:` label the work graph
//!   lacks. Bead IDs are recorded, never guessed.
//! - **Edges.** Only `blocks` edges between two work-item beads mirror the work graph's
//!   `deps`. Missing ones are added and others removed. Edges touching a subtask,
//!   discovered-work, bug or epic bead are never touched (§10.13 condition 8).
//! - **Safety.** A work graph that drifts from PLAN.md (WORKGRAPH) or has a cycle is
//!   refused, so plan changes flow plan → work graph → beads, never the other way. Any
//!   refusal applies nothing. The tracker is changed only through `br` (`--json`, §11.1).
//! - **Idempotent.** A second import finds nothing to do.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::output::Report;
use crate::process;
use crate::workgraph::{self, WorkItem};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.beads-import/v1";

const BR_LIMIT: Duration = Duration::from_mins(2);

/// A bead as the importer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedBead {
    /// Tracker ID.
    pub id: String,
    /// Labels, e.g. `m0`, `plan:W06`, `milestone`.
    pub labels: Vec<String>,
}

/// A bead the importer creates for a work item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewBead {
    /// The work item, e.g. `W54`.
    pub item: String,
    /// `W<nn>: <title>` (§11.4).
    pub title: String,
    /// The item's milestone label and `plan:W<nn>`.
    pub labels: Vec<String>,
    /// The milestone epic the bead joins (parent-child).
    pub epic: String,
    /// Context, requirements, area and acceptance, from the work graph.
    pub description: String,
}

/// The tracker operations the importer needs.
pub trait BeadGraph {
    /// Every bead, closed ones included, with its labels.
    fn beads(&self) -> Result<Vec<TrackedBead>, String>;
    /// For each of `ids`, the beads it depends on through `blocks` edges.
    fn blocks(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<String>>, String>;
    /// Creates `bead` and returns its new ID.
    fn create(&self, bead: &NewBead) -> Result<String, String>;
    /// Makes `bead` depend on `on` (`blocks`).
    fn add_blocks(&self, bead: &str, on: &str) -> Result<(), String>;
    /// Removes `bead`'s `blocks` dependency on `on`.
    fn remove_blocks(&self, bead: &str, on: &str) -> Result<(), String>;
    /// Joins `bead` to `epic` (parent-child).
    fn add_parent(&self, bead: &str, epic: &str) -> Result<(), String>;
    /// Exports the tracker database to `.beads/issues.jsonl`.
    fn flush(&self) -> Result<(), String>;
}

/// A dependency between two work items: `item` depends on `on`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Edge {
    /// The dependant.
    pub item: String,
    /// What it depends on.
    pub on: String,
}

/// What an import does, or would do.
#[derive(Debug, Default, Serialize)]
pub struct ImportPlan {
    /// Items whose existing bead is adopted, with that bead.
    pub adopted: BTreeMap<String, String>,
    /// Beads to create.
    pub create: Vec<NewBead>,
    /// Work-item edges to add.
    pub add: Vec<Edge>,
    /// Work-item edges to remove.
    pub remove: Vec<Edge>,
    /// Items whose recorded `bead` changes (created ones get their ID when applied).
    pub record: Vec<String>,
    /// Why the import cannot proceed; any refusal applies nothing.
    pub refusals: Vec<String>,
}

impl ImportPlan {
    /// True when there is nothing to change.
    pub const fn is_noop(&self) -> bool {
        self.create.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.record.is_empty()
    }
}

/// Plans an import of `items` into a tracker holding `beads`, whose work-item beads have
/// the `blocks` dependencies in `blocks`.
pub fn plan(
    items: &[WorkItem],
    beads: &[TrackedBead],
    blocks: &BTreeMap<String, Vec<String>>,
) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut labelled: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for bead in beads {
        let plans: Vec<&str> = bead
            .labels
            .iter()
            .filter_map(|label| label.strip_prefix("plan:"))
            .collect();
        if plans.len() > 1 {
            plan.refusals.push(format!(
                "{} carries {} plan labels ({}); a bead implements one work item",
                bead.id,
                plans.len(),
                plans.join(", ")
            ));
        }
        for item in plans {
            labelled.entry(item).or_default().push(&bead.id);
        }
    }
    let ids: BTreeSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    for (label, holders) in &labelled {
        if !ids.contains(label) {
            plan.refusals.push(format!(
                "{} labelled plan:{label}, which the work graph lacks; fix the plan or the label",
                holders.join(", ")
            ));
        }
    }
    for item in items {
        match labelled.get(item.id.as_str()).map(Vec::as_slice) {
            Some([bead]) => {
                if !item.bead.is_empty() && item.bead != *bead {
                    plan.refusals.push(format!(
                        "{} records bead {}, but plan:{} is on {bead}; IDs are never guessed",
                        item.id, item.bead, item.id
                    ));
                }
                if item.bead.is_empty() {
                    plan.record.push(item.id.clone());
                }
                plan.adopted.insert(item.id.clone(), (*bead).to_owned());
            }
            Some(several) if several.len() > 1 => plan.refusals.push(format!(
                "plan:{} is on {} beads ({}); exactly one bead per work item",
                item.id,
                several.len(),
                several.join(", ")
            )),
            Some(_) | None => {
                if item.bead.is_empty() {
                    match new_bead(item, beads) {
                        Ok(bead) => {
                            plan.record.push(item.id.clone());
                            plan.create.push(bead);
                        }
                        Err(why) => plan.refusals.push(why),
                    }
                } else {
                    plan.refusals.push(format!(
                        "{} records bead {}, which no longer carries plan:{}",
                        item.id, item.bead, item.id
                    ));
                }
            }
        }
    }
    let item_of: BTreeMap<&str, &str> = plan
        .adopted
        .iter()
        .map(|(item, bead)| (bead.as_str(), item.as_str()))
        .collect();
    let actual: BTreeSet<Edge> = plan
        .adopted
        .iter()
        .flat_map(|(item, bead)| {
            blocks
                .get(bead)
                .into_iter()
                .flatten()
                .filter_map(|on| item_of.get(on.as_str()))
                .map(|on| Edge {
                    item: item.clone(),
                    on: (*on).to_owned(),
                })
        })
        .collect();
    let wanted: BTreeSet<Edge> = items
        .iter()
        .flat_map(|i| {
            i.deps.iter().map(|on| Edge {
                item: i.id.clone(),
                on: on.clone(),
            })
        })
        .collect();
    plan.add = wanted.difference(&actual).cloned().collect();
    plan.remove = actual.difference(&wanted).cloned().collect();
    plan
}

fn new_bead(item: &WorkItem, beads: &[TrackedBead]) -> Result<NewBead, String> {
    let epic = beads
        .iter()
        .find(|b| b.labels.iter().any(|l| l == "milestone") && b.labels.contains(&item.milestone))
        .ok_or_else(|| {
            format!(
                "{} needs a bead, but no epic carries the labels milestone and {}; epics are \
                 created by hand (§11.2 step 3)",
                item.id, item.milestone
            )
        })?;
    let list = |values: &[String]| {
        if values.is_empty() {
            "—".to_owned()
        } else {
            values.join(", ")
        }
    };
    Ok(NewBead {
        item: item.id.clone(),
        title: format!("{}: {}", item.id, item.title),
        labels: vec![item.milestone.clone(), format!("plan:{}", item.id)],
        epic: epic.id.clone(),
        description: format!(
            "## Context\nImported from {} by ./dev beads import (PLAN.md §12.2 {}; §11.11).\n\n\
             ## Requirements\n{}\n\n## Area\n{}\n\n## Depends on\n{}\n\n## Acceptance\n\
             PLAN.md §12.2 {}, the prose; executable selectors: {}.\n\n\
             PLAN.md wins over this bead (§0.4).\n",
            workgraph::WORKGRAPH,
            item.id,
            list(&item.reqs),
            list(&item.area),
            list(&item.deps),
            item.id,
            if item.accept.is_empty() {
                "none registered yet".to_owned()
            } else {
                item.accept.join("; ")
            },
        ),
    })
}

/// Rewrites the `bead = "…"` line of each item in `recorded`, leaving every other byte
/// of the work graph alone, and proves it by re-parsing.
pub fn record_beads(text: &str, recorded: &BTreeMap<String, String>) -> Result<String, String> {
    for bead in recorded.values() {
        let plain = !bead.is_empty()
            && bead
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !plain {
            return Err(format!("refusing to record the unusual bead ID {bead:?}"));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut current: Option<&str> = None;
    let mut written = BTreeSet::new();
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        if content == "[[item]]" {
            current = None;
        } else if let Some(id) = content
            .strip_prefix("id = \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            current = Some(id);
        } else if content.starts_with("bead = ")
            && let Some(bead) = current.and_then(|id| recorded.get(id))
        {
            let _ = writeln!(out, "bead = \"{bead}\"");
            written.extend(current);
            continue;
        }
        out.push_str(line);
    }
    let missing: Vec<&String> = recorded
        .keys()
        .filter(|id| !written.contains(id.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: no `bead = ` line for {missing:?}",
            workgraph::WORKGRAPH
        ));
    }
    let before = workgraph::parse(text)?;
    let after = workgraph::parse(&out)?;
    let expected: Vec<WorkItem> = before
        .into_iter()
        .map(|mut item| {
            if let Some(bead) = recorded.get(&item.id) {
                item.bead.clone_from(bead);
            }
            item
        })
        .collect();
    if after == expected {
        Ok(out)
    } else {
        Err(format!(
            "{}: recording bead IDs would change more than `bead`; refusing",
            workgraph::WORKGRAPH
        ))
    }
}

/// The `./dev beads import` report (`keyval.dev.beads-import/v1`).
#[derive(Debug, Serialize)]
pub struct ImportReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when the import (or its dry run) found no refusal and nothing failed.
    pub ok: bool,
    /// True for `--dry-run`: nothing was changed.
    pub dry_run: bool,
    /// True when changes were applied to the tracker and the work graph.
    pub applied: bool,
    /// Work items in the work graph.
    pub items: usize,
    /// The plan, as computed before applying.
    pub plan: ImportPlan,
    /// Bead IDs created, by item.
    pub created: BTreeMap<String, String>,
    /// What failed, if anything did after the plan was made.
    pub error: Option<String>,
}

/// Runs an import against the workspace at `root`.
pub fn run(root: &Path, graph: &dyn BeadGraph, dry_run: bool) -> ImportReport {
    let mut report = ImportReport {
        schema: SCHEMA,
        ok: false,
        dry_run,
        applied: false,
        items: 0,
        plan: ImportPlan::default(),
        created: BTreeMap::new(),
        error: None,
    };
    if let Err(why) = import(root, graph, dry_run, &mut report) {
        report.error = Some(why);
    }
    report.ok = report.error.is_none() && report.plan.refusals.is_empty();
    report
}

fn import(
    root: &Path,
    graph: &dyn BeadGraph,
    dry_run: bool,
    report: &mut ImportReport,
) -> Result<(), String> {
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).map_err(|err| format!("cannot read {name}: {err}"))
    };
    let text = read(workgraph::WORKGRAPH)?;
    let drift = workgraph::judge(&read(workgraph::PLAN)?, &text)?;
    if !drift.violations.is_empty() {
        return Err(format!(
            "{} drifts from {} (./dev check WORKGRAPH); fix it before importing: {}",
            workgraph::WORKGRAPH,
            workgraph::PLAN,
            drift.violations.join("; ")
        ));
    }
    let items = workgraph::parse(&text)?;
    report.items = items.len();
    let beads = graph.beads()?;
    let plan_beads: Vec<String> = beads
        .iter()
        .filter(|b| b.labels.iter().any(|l| l.starts_with("plan:")))
        .map(|b| b.id.clone())
        .collect();
    let blocks = graph.blocks(&plan_beads)?;
    report.plan = plan(&items, &beads, &blocks);
    if dry_run || !report.plan.refusals.is_empty() || report.plan.is_noop() {
        return Ok(());
    }
    let mut bead_of = report.plan.adopted.clone();
    for new in &report.plan.create {
        let id = graph.create(new)?;
        graph.add_parent(&id, &new.epic)?;
        report.created.insert(new.item.clone(), id.clone());
        bead_of.insert(new.item.clone(), id);
    }
    let bead = |item: &str| {
        bead_of
            .get(item)
            .cloned()
            .ok_or_else(|| format!("no bead for {item}"))
    };
    for edge in &report.plan.add {
        graph.add_blocks(&bead(&edge.item)?, &bead(&edge.on)?)?;
    }
    for edge in &report.plan.remove {
        graph.remove_blocks(&bead(&edge.item)?, &bead(&edge.on)?)?;
    }
    let recorded: BTreeMap<String, String> = report
        .plan
        .record
        .iter()
        .map(|item| Ok((item.clone(), bead(item)?)))
        .collect::<Result<_, String>>()?;
    if !recorded.is_empty() {
        let updated = record_beads(&text, &recorded)?;
        fs::write(root.join(workgraph::WORKGRAPH), updated)
            .map_err(|err| format!("cannot write {}: {err}", workgraph::WORKGRAPH))?;
    }
    graph.flush()?;
    report.applied = true;
    Ok(())
}

impl Report for ImportReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let plan = &self.plan;
        let mut out = format!(
            "beads import{}: {} work item(s), {} adopted, {} to create\n",
            if self.dry_run { " (dry run)" } else { "" },
            self.items,
            plan.adopted.len(),
            plan.create.len()
        );
        for why in &plan.refusals {
            let _ = writeln!(out, "  ! refused: {why}");
        }
        for new in &plan.create {
            let id = self.created.get(&new.item).map_or("(new)", String::as_str);
            let _ = writeln!(
                out,
                "  + create {id} {:?} labels {} under {}",
                new.title,
                new.labels.join(","),
                new.epic
            );
        }
        for edge in &plan.add {
            let _ = writeln!(out, "  + edge {} depends on {}", edge.item, edge.on);
        }
        for edge in &plan.remove {
            let _ = writeln!(out, "  - edge {} depends on {}", edge.item, edge.on);
        }
        for item in &plan.record {
            let bead = plan
                .adopted
                .get(item)
                .or_else(|| self.created.get(item))
                .map_or("(new)", String::as_str);
            let _ = writeln!(out, "  ~ record {item} bead = {bead}");
        }
        if let Some(why) = &self.error {
            let _ = writeln!(out, "  error: {why}");
        }
        out.push_str(
            match (self.ok, self.dry_run, self.applied, plan.is_noop()) {
                (false, _, _, _) => {
                    "beads import: FAILED (nothing applied unless listed as created)"
                }
                (true, _, _, true) => "beads import: ok (nothing to change)",
                (true, true, _, false) => {
                    "beads import: ok (dry run; run without --dry-run to apply)"
                }
                (true, false, true, false) => "beads import: ok (applied)",
                (true, false, false, false) => "beads import: ok (not applied)",
            },
        );
        out
    }
}

/// The real tracker: `br` run in the workspace root.
pub struct LiveBeadGraph {
    /// The workspace root.
    pub root: PathBuf,
}

impl LiveBeadGraph {
    fn br(&self, args: &[&str]) -> Result<serde_json::Value, String> {
        let mut cmd = Command::new("br");
        // Always this workspace's .beads/, never an inherited BEADS_DB.
        cmd.args(args)
            .current_dir(&self.root)
            .env_remove("BEADS_DB");
        let shown = || format!("br {}", args.join(" "));
        let captured =
            process::run(&mut cmd, BR_LIMIT).map_err(|err| format!("{}: {err}", shown()))?;
        if !captured.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                shown(),
                captured.status,
                captured.stderr.trim()
            ));
        }
        if captured.stdout.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&captured.stdout).map_err(|err| format!("{}: {err}", shown()))
    }
}

fn issues(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    value
        .get("issues")
        .unwrap_or(value)
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_default()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

impl BeadGraph for LiveBeadGraph {
    fn beads(&self) -> Result<Vec<TrackedBead>, String> {
        let value = self.br(&["list", "--all", "--limit", "0", "--json"])?;
        Ok(issues(&value)
            .into_iter()
            .filter_map(|issue| {
                Some(TrackedBead {
                    id: issue["id"].as_str()?.to_owned(),
                    labels: strings(&issue["labels"]),
                })
            })
            .collect())
    }

    fn blocks(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<String>>, String> {
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(64) {
            let mut args = vec!["show"];
            args.extend(chunk.iter().map(String::as_str));
            args.push("--json");
            for issue in issues(&self.br(&args)?) {
                let Some(id) = issue["id"].as_str() else {
                    continue;
                };
                let on: Vec<String> = issue["dependencies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|dep| dep["dependency_type"] == "blocks")
                    .filter_map(|dep| dep["id"].as_str().map(str::to_owned))
                    .collect();
                out.insert(id.to_owned(), on);
            }
        }
        Ok(out)
    }

    fn create(&self, bead: &NewBead) -> Result<String, String> {
        let labels = bead.labels.join(",");
        let value = self.br(&[
            "create",
            "--title",
            &bead.title,
            "--type",
            "task",
            "--priority",
            "2",
            "--labels",
            &labels,
            "--description",
            &bead.description,
            "--json",
        ])?;
        let created = if value.is_array() { &value[0] } else { &value };
        created["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("br create for {} returned no ID: {value}", bead.item))
    }

    fn add_blocks(&self, bead: &str, on: &str) -> Result<(), String> {
        self.br(&["dep", "add", bead, on, "--type", "blocks", "--json"])
            .map(drop)
    }

    fn remove_blocks(&self, bead: &str, on: &str) -> Result<(), String> {
        self.br(&["dep", "remove", bead, on, "--type", "blocks", "--json"])
            .map(drop)
    }

    fn add_parent(&self, bead: &str, epic: &str) -> Result<(), String> {
        self.br(&["dep", "add", bead, epic, "--type", "parent-child", "--json"])
            .map(drop)
    }

    fn flush(&self) -> Result<(), String> {
        let mut cmd = Command::new("br");
        cmd.args(["sync", "--flush-only"])
            .current_dir(&self.root)
            .env_remove("BEADS_DB");
        let captured = process::run(&mut cmd, BR_LIMIT)
            .map_err(|err| format!("br sync --flush-only: {err}"))?;
        if captured.status.success() {
            Ok(())
        } else {
            Err(format!(
                "br sync --flush-only exited with {}: {}",
                captured.status,
                captured.stderr.trim()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-BEADS
    use super::*;
    use crate::test_support::scratch_dir;
    use crate::workgraph::tests::{PLAN_TEXT, workgraph_text};
    use std::cell::RefCell;

    /// An in-memory tracker that records every change.
    #[derive(Default)]
    struct FakeGraph {
        beads: Vec<TrackedBead>,
        blocks: RefCell<BTreeMap<String, Vec<String>>>,
        log: RefCell<Vec<String>>,
        next: RefCell<u32>,
    }

    impl FakeGraph {
        fn with(beads: &[(&str, &[&str])], edges: &[(&str, &str)]) -> Self {
            let graph = Self {
                beads: beads
                    .iter()
                    .map(|(id, labels)| TrackedBead {
                        id: (*id).to_owned(),
                        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                    })
                    .collect(),
                ..Self::default()
            };
            for (bead, on) in edges {
                graph
                    .blocks
                    .borrow_mut()
                    .entry((*bead).to_owned())
                    .or_default()
                    .push((*on).to_owned());
            }
            graph
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl BeadGraph for FakeGraph {
        fn beads(&self) -> Result<Vec<TrackedBead>, String> {
            Ok(self.beads.clone())
        }
        fn blocks(&self, ids: &[String]) -> Result<BTreeMap<String, Vec<String>>, String> {
            let all = self.blocks.borrow();
            Ok(ids
                .iter()
                .map(|id| (id.clone(), all.get(id).cloned().unwrap_or_default()))
                .collect())
        }
        fn create(&self, bead: &NewBead) -> Result<String, String> {
            *self.next.borrow_mut() += 1;
            let id = format!("new-{}", self.next.borrow());
            self.log
                .borrow_mut()
                .push(format!("create {id} {} {:?}", bead.title, bead.labels));
            Ok(id)
        }
        fn add_blocks(&self, bead: &str, on: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("add {bead} -> {on}"));
            Ok(())
        }
        fn remove_blocks(&self, bead: &str, on: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("remove {bead} -> {on}"));
            Ok(())
        }
        fn add_parent(&self, bead: &str, epic: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("parent {bead} -> {epic}"));
            Ok(())
        }
        fn flush(&self) -> Result<(), String> {
            self.log.borrow_mut().push("flush".to_owned());
            Ok(())
        }
    }

    /// A workspace holding [`PLAN_TEXT`] and a work graph recording `beads`.
    fn workspace(beads: [&str; 3]) -> PathBuf {
        let root = scratch_dir("beads-import");
        fs::create_dir_all(root.join("registry")).unwrap();
        fs::write(root.join(workgraph::PLAN), PLAN_TEXT).unwrap();
        fs::write(root.join(workgraph::WORKGRAPH), workgraph_text(beads)).unwrap();
        root
    }

    const EPICS: [(&str, &[&str]); 2] = [
        ("epic-0", &["milestone", "m0"]),
        ("epic-1", &["milestone", "m1"]),
    ];

    /// The three work-item beads with exactly the work graph's edges, a subtask with its
    /// own edges, and the epics.
    fn faithful() -> FakeGraph {
        FakeGraph::with(
            &[
                EPICS[0],
                EPICS[1],
                ("b-1", &["m0", "plan:W01"]),
                ("b-2", &["m0", "plan:W02", "hard"]),
                ("b-3", &["m1", "plan:W03"]),
                ("b-3-1", &["m1", "sub:W03"]),
            ],
            &[
                ("b-2", "b-1"),
                ("b-3", "b-1"),
                ("b-3", "b-2"),
                ("b-3", "b-3-1"),
                ("b-3-1", "b-2"),
            ],
        )
    }

    #[test]
    fn existing_beads_are_adopted_once_and_their_ids_recorded() {
        let root = workspace(["", "", ""]);
        let graph = faithful();
        let report = run(&root, &graph, false);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(
            report.plan.adopted,
            BTreeMap::from([
                ("W01".to_owned(), "b-1".to_owned()),
                ("W02".to_owned(), "b-2".to_owned()),
                ("W03".to_owned(), "b-3".to_owned()),
            ])
        );
        assert_eq!(
            graph.log(),
            ["flush"],
            "rule: adoption edits no bead; subtask edges are untouched"
        );
        let text = fs::read_to_string(root.join(workgraph::WORKGRAPH)).unwrap();
        assert_eq!(
            text,
            workgraph_text(["b-1", "b-2", "b-3"]),
            "rule: only the bead lines change"
        );
    }

    #[test]
    fn a_second_import_changes_nothing() {
        let root = workspace(["b-1", "b-2", "b-3"]);
        let graph = faithful();
        let report = run(&root, &graph, false);
        assert!(report.ok && report.plan.is_noop(), "report={report:#?}");
        assert_eq!(
            graph.log(),
            Vec::<String>::new(),
            "rule: a repeat import is a no-op"
        );
        assert!(!report.applied);
        assert!(report.render_human().ends_with("ok (nothing to change)"));
    }

    #[test]
    fn a_missing_item_is_created_under_its_epic_with_its_edges() {
        let root = workspace(["b-1", "b-2", ""]);
        let graph = FakeGraph::with(
            &[
                EPICS[0],
                EPICS[1],
                ("b-1", &["m0", "plan:W01"]),
                ("b-2", &["m0", "plan:W02"]),
            ],
            &[("b-2", "b-1")],
        );
        let report = run(&root, &graph, false);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(
            graph.log(),
            [
                "create new-1 W03: Contract [\"m1\", \"plan:W03\"]",
                "parent new-1 -> epic-1",
                "add new-1 -> b-1",
                "add new-1 -> b-2",
                "flush",
            ]
        );
        assert_eq!(
            fs::read_to_string(root.join(workgraph::WORKGRAPH)).unwrap(),
            workgraph_text(["b-1", "b-2", "new-1"])
        );
        let created = &report.plan.create[0];
        assert!(
            created.description.contains("Requirements\nF-1, F-2, §8")
                && created.description.contains("Area\napi (spec only)"),
            "description={}",
            created.description
        );
    }

    #[test]
    fn changed_dependencies_are_mirrored_between_work_item_beads_only() {
        let root = workspace(["b-1", "b-2", "b-3"]);
        // b-3 lacks its edge to b-2 and b-2 has a stray edge to b-3. A bug bead depends
        // on b-1 and b-1 on a discovered bead: neither pair is two work items.
        let graph = FakeGraph::with(
            &[
                EPICS[0],
                EPICS[1],
                ("b-1", &["m0", "plan:W01"]),
                ("b-2", &["m0", "plan:W02"]),
                ("b-3", &["m1", "plan:W03"]),
                ("bug-1", &["m1"]),
                ("disc-1", &["m0", "discovered"]),
            ],
            &[
                ("b-2", "b-1"),
                ("b-2", "b-3"),
                ("b-3", "b-1"),
                ("bug-1", "b-1"),
                ("b-1", "disc-1"),
            ],
        );
        let report = run(&root, &graph, true);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(
            report.plan.add,
            [Edge {
                item: "W03".to_owned(),
                on: "W02".to_owned()
            }]
        );
        assert_eq!(
            report.plan.remove,
            [Edge {
                item: "W02".to_owned(),
                on: "W03".to_owned()
            }]
        );
        assert_eq!(
            graph.log(),
            Vec::<String>::new(),
            "rule: --dry-run applies nothing"
        );
        let human = report.render_human();
        assert!(
            human.contains("+ edge W03 depends on W02")
                && human.contains("- edge W02 depends on W03"),
            "rule: --dry-run prints the diff; human={human}"
        );
        let applied = run(&root, &graph, false);
        assert!(applied.applied, "report={applied:#?}");
        assert_eq!(
            graph.log(),
            ["add b-3 -> b-2", "remove b-2 -> b-3", "flush"]
        );
    }

    /// Tracker beads, the work graph's recorded bead IDs, and the expected refusal.
    type RefusalCase<'a> = (&'a [(&'a str, &'a [&'a str])], [&'a str; 3], &'a str);

    #[test]
    fn duplicate_labels_unknown_labels_and_conflicting_ids_are_refused() {
        let cases: [RefusalCase; 5] = [
            (
                &[
                    EPICS[0],
                    EPICS[1],
                    ("b-1", &["plan:W01"]),
                    ("x-1", &["plan:W01"]),
                    ("b-2", &["plan:W02"]),
                    ("b-3", &["plan:W03"]),
                ],
                ["", "", ""],
                "plan:W01 is on 2 beads (b-1, x-1)",
            ),
            (
                &[
                    EPICS[0],
                    EPICS[1],
                    ("b-1", &["plan:W01"]),
                    ("b-2", &["plan:W02"]),
                    ("b-3", &["plan:W03"]),
                    ("b-9", &["plan:W09"]),
                ],
                ["", "", ""],
                "b-9 labelled plan:W09, which the work graph lacks",
            ),
            (
                &[
                    EPICS[0],
                    EPICS[1],
                    ("b-1", &["plan:W01"]),
                    ("b-2", &["plan:W02"]),
                    ("b-3", &["plan:W03"]),
                ],
                ["b-7", "", ""],
                "W01 records bead b-7, but plan:W01 is on b-1",
            ),
            (
                &[("b-1", &["plan:W01"]), ("b-2", &["plan:W02"])],
                ["b-1", "b-2", ""],
                "no epic carries the labels milestone and m1",
            ),
            (
                &[
                    EPICS[0],
                    EPICS[1],
                    ("b-1", &["plan:W01", "plan:W02"]),
                    ("b-3", &["plan:W03"]),
                ],
                ["", "", ""],
                "b-1 carries 2 plan labels (W01, W02)",
            ),
        ];
        for (beads, recorded, expected) in cases {
            let root = workspace(recorded);
            let graph = FakeGraph::with(beads, &[]);
            let report = run(&root, &graph, false);
            assert!(
                !report.ok,
                "expected refusal {expected:?}; report={report:#?}"
            );
            assert!(
                report.plan.refusals.iter().any(|r| r.contains(expected)),
                "expected {expected:?}; refusals={:#?}",
                report.plan.refusals
            );
            assert_eq!(
                graph.log(),
                Vec::<String>::new(),
                "rule: a refusal applies nothing"
            );
            assert_eq!(
                fs::read_to_string(root.join(workgraph::WORKGRAPH)).unwrap(),
                workgraph_text(recorded),
                "rule: a refusal leaves the work graph alone"
            );
        }
    }

    #[test]
    fn a_drifted_or_cyclic_work_graph_is_refused_before_the_tracker_is_read() {
        let root = workspace(["", "", ""]);
        let cyclic = workgraph_text(["", "", ""]).replace(
            "title = \"Bootstrap\"\ndeps = []",
            "title = \"Bootstrap\"\ndeps = [\"W03\"]",
        );
        fs::write(root.join(workgraph::WORKGRAPH), cyclic).unwrap();
        let graph = faithful();
        let report = run(&root, &graph, false);
        assert!(!report.ok, "report={report:#?}");
        let error = report.error.unwrap_or_default();
        assert!(
            error.contains("./dev check WORKGRAPH") && error.contains("cycle: W01 -> W03 -> W01"),
            "rule: plan changes flow plan → work graph → beads; error={error}"
        );
        assert_eq!(graph.log(), Vec::<String>::new());
    }

    #[test]
    fn recording_changes_only_bead_lines() {
        let text = workgraph_text(["", "", ""]);
        let recorded = BTreeMap::from([("W02".to_owned(), "t-2".to_owned())]);
        assert_eq!(
            record_beads(&text, &recorded).unwrap(),
            workgraph_text(["", "t-2", ""])
        );
        let odd = BTreeMap::from([("W02".to_owned(), "t\"2".to_owned())]);
        assert!(
            record_beads(&text, &odd).is_err(),
            "rule: odd IDs are never written"
        );
        let unknown = BTreeMap::from([("W09".to_owned(), "t-9".to_owned())]);
        assert!(record_beads(&text, &unknown).is_err_and(|e| e.contains("W09")));
    }

    /// Runs `br` in a scratch workspace and returns its JSON output.
    fn br(root: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new("br")
            .args(args)
            .current_dir(root)
            .env_remove("BEADS_DB")
            .output()
            .unwrap();
        assert!(out.status.success(), "br {args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
    }

    fn create(root: &Path, title: &str, labels: &str) -> String {
        let value = br(
            root,
            &[
                "create", "--title", title, "-t", "task", "-p", "2", "-l", labels, "--json",
            ],
        );
        let bead = if value.is_array() { &value[0] } else { &value };
        bead["id"].as_str().unwrap().to_owned()
    }

    #[test]
    fn a_real_tracker_import_adopts_creates_mirrors_and_then_rests() {
        let root = workspace(["", "", ""]);
        br(&root, &["init", "--prefix", "t", "--json"]);
        let epic0 = create(&root, "M0: epic", "milestone,m0");
        create(&root, "M1: epic", "milestone,m1");
        let w01 = create(&root, "W01: Bootstrap", "m0,plan:W01");
        let w03 = create(&root, "W03: Contract", "m1,plan:W03");
        let sub = create(&root, "W03.1: part", "m1,sub:W03");
        br(&root, &["dep", "add", &w03, &sub, "--json"]);
        br(&root, &["dep", "add", &sub, &w01, "--json"]);
        let graph = LiveBeadGraph { root: root.clone() };

        let first = run(&root, &graph, false);
        assert!(first.ok && first.applied, "report={first:#?}");
        let w02 = first.created.get("W02").cloned().unwrap();
        assert_eq!(
            fs::read_to_string(root.join(workgraph::WORKGRAPH)).unwrap(),
            workgraph_text([w01.as_str(), w02.as_str(), w03.as_str()]),
            "rule: the plan-ID ↔ bead-ID mapping is recorded"
        );
        let blocks = graph
            .blocks(&[w01.clone(), w02.clone(), w03.clone(), sub.clone()])
            .unwrap();
        let sorted = |id: &str| {
            let mut on = blocks.get(id).cloned().unwrap_or_default();
            on.sort();
            on
        };
        assert_eq!(sorted(&w02), std::slice::from_ref(&w01));
        let mut w03_on = vec![w01.clone(), w02.clone(), sub.clone()];
        w03_on.sort();
        assert_eq!(sorted(&w03), w03_on, "rule: the subtask edge survives");
        assert_eq!(
            sorted(&sub),
            std::slice::from_ref(&w01),
            "rule: subtask edges are untouched"
        );
        let shown = br(&root, &["show", &w02, "--json"]);
        let labels = strings(&shown[0]["labels"]);
        assert!(
            labels.contains(&"plan:W02".to_owned()) && labels.contains(&"m0".to_owned()),
            "labels={labels:?}"
        );
        let parent = shown[0]["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == epic0.as_str() && d["dependency_type"] == "parent-child");
        assert!(
            parent,
            "rule: a created bead joins its milestone epic; shown={shown}"
        );

        let second = run(&root, &graph, false);
        assert!(
            second.ok && second.plan.is_noop() && !second.applied,
            "rule: a second import creates no duplicates and changes nothing; report={second:#?}"
        );
        let all = graph.beads().unwrap();
        let w02_count = all
            .iter()
            .filter(|b| b.labels.contains(&"plan:W02".to_owned()))
            .count();
        assert_eq!(w02_count, 1, "rule: never a duplicate bead");
        assert!(
            root.join(".beads/issues.jsonl").is_file(),
            "rule: the import flushes"
        );
    }
}
