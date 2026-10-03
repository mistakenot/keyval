//! WORKGRAPH: `registry/workgraph.toml` mirrors PLAN.md §12.2 and contains no cycle
//! (§10.13 condition 8, §11.11; W06).
//!
//! The comparison covers what condition 8 names: `id`, `deps`, `reqs` (ranges such as
//! `F-1…F-10` expand mechanically) and `area`. It also holds `deps` to the §12.3 edges,
//! so the plan's two statements of the graph cannot drift apart unnoticed. `title`,
//! `milestone`, `accept` and `bead` are registry-owned detail, checked only for shape.
//! Changes flow plan → this file → beads, never the other way (§11.11): a drift is fixed
//! in the work graph, or in the plan through a `plan-change`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::check::StaticOutcome;

/// The plan, relative to the workspace root.
pub const PLAN: &str = "PLAN.md";

/// The work graph, relative to the workspace root.
pub const WORKGRAPH: &str = "registry/workgraph.toml";

/// The work-graph schema this module reads.
pub const SCHEMA: &str = "keyval.registry.workgraph/v1";

/// One `[[item]]` of the work graph, every field.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    /// The §12.2 ID, e.g. `W06`.
    pub id: String,
    /// The §12.2 title, verbatim.
    pub title: String,
    /// IDs of the items this one depends on.
    pub deps: Vec<String>,
    /// The Reqs tokens, ranges expanded.
    pub reqs: Vec<String>,
    /// The Area tokens, verbatim.
    pub area: Vec<String>,
    /// `m0`…`m5`.
    pub milestone: String,
    /// Executable acceptance selectors, each a `./dev` command line.
    pub accept: Vec<String>,
    /// The tracker's bead ID; empty until `./dev beads import` records it.
    pub bead: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkgraphFile {
    schema: String,
    item: Vec<WorkItem>,
}

/// Parses the work graph, refusing an unknown schema or field.
pub fn parse(text: &str) -> Result<Vec<WorkItem>, String> {
    let file: WorkgraphFile =
        toml::from_str(text).map_err(|err| format!("invalid {WORKGRAPH}: {}", err.message()))?;
    if file.schema != SCHEMA {
        return Err(format!(
            "{WORKGRAPH}: schema {:?}, expected {SCHEMA:?}",
            file.schema
        ));
    }
    Ok(file.item)
}

/// One §12.2 row as the plan states it, ranges expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    /// `W<nn>`.
    pub id: String,
    /// The Depends-on cell (`—` is none).
    pub deps: Vec<String>,
    /// The Reqs cell.
    pub reqs: Vec<String>,
    /// The Area cell.
    pub area: Vec<String>,
}

/// The §12.2 work items, in plan order.
pub fn plan_items(plan: &str) -> Result<Vec<PlanItem>, String> {
    let section = section(plan, "### 12.2 ", "### 12.3 ")?;
    let mut items = Vec::new();
    for line in section.lines().filter(|line| line.starts_with("| **W")) {
        let cells: Vec<&str> = line
            .trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split(" | ")
            .map(str::trim)
            .collect();
        let [id, _title, deps, reqs, area, _acceptance] = cells[..] else {
            return Err(format!(
                "{PLAN} §12.2: a row has {} cells, expected 6: {line:.60}",
                cells.len()
            ));
        };
        let id = id
            .strip_prefix("**")
            .and_then(|id| id.strip_suffix("**"))
            .ok_or_else(|| format!("{PLAN} §12.2: unreadable ID cell {id:?}"))?;
        let mut expanded = Vec::new();
        for token in tokens(reqs) {
            expanded.extend(expand(&token)?);
        }
        items.push(PlanItem {
            id: id.to_owned(),
            deps: tokens(deps),
            reqs: expanded,
            area: tokens(area),
        });
    }
    if items.is_empty() {
        return Err(format!("{PLAN} §12.2 has no work-item rows"));
    }
    Ok(items)
}

/// The §12.3 edges as `(dependency, dependant)` pairs.
pub fn plan_edges(plan: &str) -> Result<BTreeSet<(String, String)>, String> {
    let section = section(plan, "### 12.3 ", "### 12.4 ")?;
    let mut edges = BTreeSet::new();
    for line in section.lines().map(str::trim) {
        if let Some((from, to)) = line.split_once(" --> ") {
            let edge = (from.trim().to_owned(), to.trim().to_owned());
            if !edges.insert(edge) {
                return Err(format!("{PLAN} §12.3 lists the edge {line:?} twice"));
            }
        }
    }
    if edges.is_empty() {
        return Err(format!("{PLAN} §12.3 has no edges"));
    }
    Ok(edges)
}

/// The text between the line starting with `heading` and the next line starting with
/// `next`.
fn section<'a>(text: &'a str, heading: &str, next: &str) -> Result<&'a str, String> {
    let start = text
        .find(&format!("\n{heading}"))
        .ok_or_else(|| format!("{PLAN} has no section {heading:?}"))?;
    let rest = &text[start + 1..];
    let end = rest
        .find(&format!("\n{next}"))
        .ok_or_else(|| format!("{PLAN} has no section {next:?} after {heading:?}"))?;
    Ok(&rest[..end])
}

/// A cell's comma-separated tokens; `—` is the empty list.
fn tokens(cell: &str) -> Vec<String> {
    if cell.trim() == "—" {
        return Vec::new();
    }
    cell.split(", ").map(|t| t.trim().to_owned()).collect()
}

/// Expands `F-1…F-10` to `F-1`, …, `F-10` (zero padding kept, as in `B-01…B-22`); any
/// other token is itself.
pub fn expand(token: &str) -> Result<Vec<String>, String> {
    let Some((first, last)) = token.split_once('…') else {
        return Ok(vec![token.to_owned()]);
    };
    let (prefix, width, from) = range_end(first, token)?;
    let (last_prefix, _, to) = range_end(last, token)?;
    if prefix != last_prefix || from > to {
        return Err(format!("unreadable range {token:?}"));
    }
    let padded = first.rsplit('-').next().is_some_and(|n| n.starts_with('0'));
    Ok((from..=to)
        .map(|n| {
            if padded {
                format!("{prefix}-{n:0width$}")
            } else {
                format!("{prefix}-{n}")
            }
        })
        .collect())
}

/// One end of a range, `F-10`: its prefix, digit count and value.
fn range_end<'a>(end: &'a str, token: &str) -> Result<(&'a str, usize, u32), String> {
    let (prefix, number) = end
        .rsplit_once('-')
        .ok_or_else(|| format!("unreadable range end {end:?} in {token:?}"))?;
    let value = number
        .parse()
        .map_err(|_| format!("unreadable range end {end:?} in {token:?}"))?;
    Ok((prefix, number.len(), value))
}

/// What a comparison found: the assertions made and the drifts.
#[derive(Debug, Default)]
pub struct Drift {
    /// Assertions made.
    pub cases: usize,
    /// One message per difference.
    pub violations: Vec<String>,
}

/// Compares the work graph with §12.2 (`plan`) and §12.3 (`edges`).
pub fn plan_drift(
    plan: &[PlanItem],
    edges: &BTreeSet<(String, String)>,
    items: &[WorkItem],
) -> Drift {
    let mut drift = Drift::default();
    let plan_ids: Vec<&str> = plan.iter().map(|p| p.id.as_str()).collect();
    let item_ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    drift.cases += 1;
    if plan_ids != item_ids {
        drift.violations.push(format!(
            "ids differ from §12.2: missing {:?}, extra {:?}, or out of plan order",
            difference(&plan_ids, &item_ids),
            difference(&item_ids, &plan_ids)
        ));
    }
    let by_id: BTreeMap<&str, &WorkItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    for row in plan {
        let Some(item) = by_id.get(row.id.as_str()) else {
            continue;
        };
        for (field, planned, recorded) in [
            ("deps", &row.deps, &item.deps),
            ("reqs", &row.reqs, &item.reqs),
            ("area", &row.area, &item.area),
        ] {
            drift.cases += 1;
            let planned: BTreeSet<&String> = planned.iter().collect();
            let recorded_set: BTreeSet<&String> = recorded.iter().collect();
            if planned != recorded_set || recorded_set.len() != recorded.len() {
                drift.violations.push(format!(
                    "{}.{field}: workgraph has {recorded:?}; §12.2 says {:?}",
                    row.id,
                    planned.into_iter().collect::<Vec<_>>()
                ));
            }
        }
    }
    let recorded: BTreeSet<(String, String)> = items
        .iter()
        .flat_map(|i| i.deps.iter().map(|d| (d.clone(), i.id.clone())))
        .collect();
    for edge in edges.union(&recorded) {
        drift.cases += 1;
        match (edges.contains(edge), recorded.contains(edge)) {
            (true, false) => drift.violations.push(format!(
                "§12.3 has {} --> {}, which no work item's deps record",
                edge.0, edge.1
            )),
            (false, true) => drift
                .violations
                .push(format!("{}.deps has {}, which §12.3 lacks", edge.1, edge.0)),
            (true, true) | (false, false) => {}
        }
    }
    drift
}

fn difference<'a>(a: &[&'a str], b: &[&str]) -> Vec<&'a str> {
    a.iter().copied().filter(|x| !b.contains(x)).collect()
}

/// Shape rules for the registry-owned fields, and graph rules: unknown dependencies,
/// cycles and duplicate bead IDs.
pub fn shape_drift(items: &[WorkItem]) -> Drift {
    let mut drift = Drift::default();
    let ids: BTreeSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let mut beads: BTreeMap<&str, &str> = BTreeMap::new();
    for item in items {
        drift.cases += 1;
        let milestone_ok = item
            .milestone
            .strip_prefix('m')
            .is_some_and(|n| matches!(n, "0" | "1" | "2" | "3" | "4" | "5"));
        if !milestone_ok {
            drift.violations.push(format!(
                "{}.milestone is {:?}; expected m0…m5",
                item.id, item.milestone
            ));
        }
        for selector in &item.accept {
            drift.cases += 1;
            if !selector.starts_with("./dev ") || selector.contains("  ") {
                drift.violations.push(format!(
                    "{}.accept has {selector:?}; each selector is one `./dev` command line",
                    item.id
                ));
            }
        }
        for dep in &item.deps {
            drift.cases += 1;
            if !ids.contains(dep.as_str()) {
                drift.violations.push(format!(
                    "{} depends on {dep}, which is not a work item",
                    item.id
                ));
            }
        }
        if !item.bead.is_empty() {
            drift.cases += 1;
            if let Some(other) = beads.insert(&item.bead, &item.id) {
                drift.violations.push(format!(
                    "{} and {other} both record bead {}; each item has its own bead",
                    item.id, item.bead
                ));
            }
        }
    }
    drift.cases += 1;
    if let Some(cycle) = cycle(items) {
        drift.violations.push(format!(
            "the work graph has a cycle: {}",
            cycle.join(" -> ")
        ));
    }
    drift
}

/// A dependency cycle, as the IDs along it (first repeated at the end), if there is one.
pub fn cycle(items: &[WorkItem]) -> Option<Vec<String>> {
    let deps: BTreeMap<&str, Vec<&str>> = items
        .iter()
        .map(|i| (i.id.as_str(), i.deps.iter().map(String::as_str).collect()))
        .collect();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for start in deps.keys() {
        let mut path: Vec<&str> = Vec::new();
        if let Some(found) = visit(start, &deps, &mut done, &mut path) {
            return Some(found);
        }
    }
    None
}

fn visit<'a>(
    node: &'a str,
    deps: &BTreeMap<&'a str, Vec<&'a str>>,
    done: &mut BTreeSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    if let Some(at) = path.iter().position(|n| *n == node) {
        let mut found: Vec<String> = path[at..].iter().map(|n| (*n).to_owned()).collect();
        found.push(node.to_owned());
        return Some(found);
    }
    if done.contains(node) {
        return None;
    }
    path.push(node);
    for next in deps.get(node).into_iter().flatten() {
        if let Some(found) = visit(next, deps, done, path) {
            return Some(found);
        }
    }
    path.pop();
    done.insert(node);
    None
}

/// Every WORKGRAPH assertion on the given texts.
pub fn judge(plan: &str, workgraph: &str) -> Result<Drift, String> {
    let items = parse(workgraph)?;
    let mut drift = plan_drift(&plan_items(plan)?, &plan_edges(plan)?, &items);
    let shape = shape_drift(&items);
    drift.cases += shape.cases;
    drift.violations.extend(shape.violations);
    Ok(drift)
}

// check: WORKGRAPH
/// Compares `<root>/registry/workgraph.toml` with `<root>/PLAN.md` §12.2/§12.3.
pub fn check_workgraph(root: &Path) -> Result<StaticOutcome, String> {
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).map_err(|err| format!("cannot read {name}: {err}"))
    };
    let drift = judge(&read(PLAN)?, &read(WORKGRAPH)?)?;
    Ok(StaticOutcome {
        cases: drift.cases,
        violations: drift.violations,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A three-item plan in the §12.2/§12.3 layout.
    pub const PLAN_TEXT: &str = "\
# Plan

### 12.2 Work items

| ID | Title | Depends on | Reqs | Area | Acceptance |
|---|---|---|---|---|---|
| **W01** | Bootstrap | — | P7, CD-1…CD-3 | root, xtask | it works |
| **W02** | Spike | W01 | P4 | vendor, spike | it is deterministic |
| **W03** | Contract | W01, W02 | F-1…F-2, §8 | api (spec only) | it validates |

### 12.3 Dependency graph

```mermaid
flowchart LR
    W01 --> W02
    W01 --> W03
    W02 --> W03
```

### 12.4 Tracks
";

    /// The work graph that mirrors [`PLAN_TEXT`] exactly.
    pub fn workgraph_text(beads: [&str; 3]) -> String {
        format!(
            "schema = \"{SCHEMA}\"\n\n\
             [[item]]\nid = \"W01\"\ntitle = \"Bootstrap\"\ndeps = []\n\
             reqs = [\"P7\", \"CD-1\", \"CD-2\", \"CD-3\"]\narea = [\"root\", \"xtask\"]\n\
             milestone = \"m0\"\naccept = [\"./dev check XTASK\"]\nbead = \"{}\"\n\n\
             [[item]]\nid = \"W02\"\ntitle = \"Spike\"\ndeps = [\"W01\"]\nreqs = [\"P4\"]\n\
             area = [\"vendor\", \"spike\"]\nmilestone = \"m0\"\naccept = []\nbead = \"{}\"\n\n\
             [[item]]\nid = \"W03\"\ntitle = \"Contract\"\ndeps = [\"W01\", \"W02\"]\n\
             reqs = [\"F-1\", \"F-2\", \"§8\"]\narea = [\"api (spec only)\"]\nmilestone = \"m1\"\n\
             accept = []\nbead = \"{}\"\n",
            beads[0], beads[1], beads[2]
        )
    }

    fn violations(plan: &str, workgraph: &str) -> Vec<String> {
        judge(plan, workgraph).unwrap().violations
    }

    fn assert_drift(workgraph: &str, rule: &str, expected: &str) {
        let found = violations(PLAN_TEXT, workgraph);
        assert!(
            found.iter().any(|v| v.contains(expected)),
            "rule: {rule}; expected a violation containing {expected:?}; found={found:#?}"
        );
    }

    #[test]
    fn a_faithful_mirror_passes_and_counts_every_assertion() {
        let drift = judge(PLAN_TEXT, &workgraph_text(["", "", ""])).unwrap();
        assert_eq!(drift.violations, Vec::<String>::new(), "drift={drift:#?}");
        // ids 1 + 3 items × 3 fields + 3 edges, then shape: 3 items + 1 selector + 3 deps
        // + cycle 1
        assert_eq!(drift.cases, 1 + 9 + 3 + 3 + 1 + 3 + 1, "drift={drift:#?}");
    }

    #[test]
    fn ranges_expand_mechanically() {
        assert_eq!(expand("F-1…F-3").unwrap(), ["F-1", "F-2", "F-3"]);
        assert_eq!(expand("B-08…B-10").unwrap(), ["B-08", "B-09", "B-10"]);
        assert_eq!(expand("§11.11").unwrap(), ["§11.11"]);
        assert!(
            expand("F-3…F-1").is_err(),
            "rule: a backwards range is unreadable"
        );
        assert!(
            expand("F-1…CD-3").is_err(),
            "rule: both ends share a prefix"
        );
    }

    #[test]
    fn a_changed_dependency_fails() {
        let changed =
            workgraph_text(["", "", ""]).replace("deps = [\"W01\", \"W02\"]", "deps = [\"W01\"]");
        assert_drift(
            &changed,
            "deps mirror §12.2",
            "W03.deps: workgraph has [\"W01\"]",
        );
        assert_drift(&changed, "deps mirror §12.3", "§12.3 has W02 --> W03");
    }

    #[test]
    fn changed_reqs_and_areas_fail() {
        let reqs = workgraph_text(["", "", ""])
            .replace("\"CD-3\"]", "]")
            .replace(", ]", "]");
        assert_drift(&reqs, "reqs mirror §12.2 with ranges expanded", "W01.reqs");
        let area = workgraph_text(["", "", ""]).replace("\"api (spec only)\"", "\"api\"");
        assert_drift(&area, "areas mirror §12.2 verbatim", "W03.area");
    }

    #[test]
    fn missing_extra_and_reordered_items_fail() {
        let text = workgraph_text(["", "", ""]);
        let without_w02 = {
            let start = text.find("[[item]]\nid = \"W02\"").unwrap();
            let end = text.find("[[item]]\nid = \"W03\"").unwrap();
            format!("{}{}", &text[..start], &text[end..])
        };
        assert_drift(
            &without_w02,
            "every §12.2 item is mirrored",
            "missing [\"W02\"]",
        );
        let extra = format!(
            "{text}\n[[item]]\nid = \"W04\"\ntitle = \"x\"\ndeps = []\nreqs = []\narea = []\n\
             milestone = \"m0\"\naccept = []\nbead = \"\"\n"
        );
        assert_drift(&extra, "no item outside §12.2", "extra [\"W04\"]");
    }

    #[test]
    fn a_plan_whose_two_graphs_disagree_fails() {
        let plan = PLAN_TEXT.replace("    W02 --> W03\n", "");
        let found = violations(&plan, &workgraph_text(["", "", ""]));
        assert!(
            found
                .iter()
                .any(|v| v.contains("W03.deps has W02, which §12.3 lacks")),
            "rule: §12.2 and §12.3 must agree edge for edge; found={found:#?}"
        );
    }

    #[test]
    fn cycles_are_rejected() {
        let cyclic = workgraph_text(["", "", ""]).replace(
            "title = \"Bootstrap\"\ndeps = []",
            "title = \"Bootstrap\"\ndeps = [\"W03\"]",
        );
        assert_drift(
            &cyclic,
            "the work graph is acyclic",
            "cycle: W01 -> W03 -> W01",
        );
        let items = parse(&cyclic).unwrap();
        assert_eq!(cycle(&items).unwrap(), ["W01", "W03", "W01"]);
        assert_eq!(cycle(&parse(&workgraph_text(["", "", ""])).unwrap()), None);
    }

    #[test]
    fn registry_owned_fields_are_shape_checked() {
        let text = workgraph_text(["t-1", "t-1", ""])
            .replace("milestone = \"m1\"", "milestone = \"M1\"")
            .replace("\"./dev check XTASK\"", "\"XTASK\"");
        let found = violations(PLAN_TEXT, &text);
        for expected in [
            "W02 and W01 both record bead t-1",
            "W03.milestone is \"M1\"",
            "W01.accept has \"XTASK\"",
        ] {
            assert!(
                found.iter().any(|v| v.contains(expected)),
                "expected {expected:?}; found={found:#?}"
            );
        }
    }

    #[test]
    fn unknown_fields_and_schemas_are_refused() {
        let unknown =
            workgraph_text(["", "", ""]).replace("bead = \"\"\n", "bead = \"\"\nnote = 1\n");
        assert!(parse(&unknown).is_err_and(|e| e.contains("unknown field")));
        let schema = workgraph_text(["", "", ""]).replace("/v1", "/v2");
        assert!(parse(&schema).is_err_and(|e| e.contains("schema")));
    }

    #[test]
    fn the_real_workgraph_mirrors_the_real_plan() {
        let root = crate::test_support::workspace_root();
        let outcome = check_workgraph(&root).unwrap();
        assert_eq!(
            outcome.violations,
            Vec::<String>::new(),
            "outcome={outcome:#?}"
        );
        let floor = crate::check::CATALOGUE
            .iter()
            .find(|def| def.id == "WORKGRAPH")
            .map(|def| def.min_cases)
            .unwrap();
        assert!(
            outcome.cases >= usize::try_from(floor).unwrap(),
            "rule: the real work graph meets WORKGRAPH's min_cases={floor}; cases={}",
            outcome.cases
        );
    }
}
