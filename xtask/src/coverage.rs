//! `./dev report coverage`: renders `reports/coverage.md` from coverage records
//! (`keyval.coverage/v1`, PLAN.md §10.4; W41.2) and the required set in
//! `registry/coverage.toml`.
//!
//! A coverage record is the matrix the checker crate counts from a run's observations: every
//! cell of six dimensions (fault, fault pair, topology, crash point, outcome, operation) with
//! how often it was exercised. The report sums the records of each build (coverage is per
//! build), lists every empty cell, and marks the empty cells that are in the required set.
//! T2 reports empty cells; T3 requires every required cell to be non-zero, which this view
//! makes visible and does not itself enforce: it is a report, not a gate.
//!
//! `registry/coverage.toml` has one `[[dimension]]` table per dimension: its `name`, its
//! `owner` (`W41` authored the structure and the fault, pair, topology and crash-point cells;
//! `W43` completes the outcome and operation cells) and its `required` cells. The file is
//! judged against the record schema (`schemas/coverage/coverage.v1.schema.json`), whose
//! cell lists are the universe: an unknown cell, the never-required `other_partition`, a
//! duplicate or missing dimension, an unknown owner, or an empty required list for a
//! dimension that is not owned by W43 is an error (P10).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::output::Report;

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.coverage/v1";

/// The required set, relative to the workspace root.
pub const REQUIRED: &str = "registry/coverage.toml";

/// The schema of a coverage record, relative to the workspace root.
pub const RECORD_SCHEMA: &str = "schemas/coverage/coverage.v1.schema.json";

/// Where coverage records are read from by default, relative to the workspace root.
pub const RECORDS: &str = "artifacts/coverage";

/// Where the report is written, relative to the workspace root (generated, not committed).
pub const REPORT: &str = "reports/coverage.md";

/// The owner that may leave a dimension's required list empty until it completes it.
const PENDING_OWNER: &str = "W43";

/// The owners of required-set cells.
/// The dimensions whose required cells W43 completes (D-73); every other dimension is
/// complete from the start.
const PENDING_DIMENSIONS: [&str; 2] = ["outcome", "operation"];

const OWNERS: [&str; 2] = ["W41", PENDING_OWNER];

/// The cell that is counted but never required.
const NEVER_REQUIRED: &str = "other_partition";

/// The record's dimensions, in report order.
const DIMENSIONS: [&str; 6] = [
    "fault",
    "fault_pair",
    "topology",
    "crash_point",
    "outcome",
    "operation",
];

/// One `[[dimension]]` table of `registry/coverage.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DimensionRow {
    name: String,
    owner: String,
    required: Vec<String>,
}

/// The file `registry/coverage.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequiredFile {
    schema: String,
    dimension: Vec<DimensionRow>,
}

/// The required set: dimension name to its owner and cells, in report order.
#[derive(Debug)]
pub struct Required {
    dimensions: Vec<(String, String, BTreeSet<String>)>,
}

impl Required {
    /// Whether `cell` of `dimension` is required.
    fn contains(&self, dimension: &str, cell: &str) -> bool {
        self.dimensions
            .iter()
            .any(|(name, _, cells)| name == dimension && cells.contains(cell))
    }

    /// The number of required cells over all dimensions.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.dimensions
            .iter()
            .map(|(_, _, cells)| cells.len())
            .sum()
    }
}

/// The cells of each dimension, read from the record schema.
fn universe(schema: &Value) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let mut cells = BTreeMap::new();
    for dimension in DIMENSIONS {
        let listed = schema
            .pointer(&format!(
                "/properties/cells/properties/{dimension}/required"
            ))
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{RECORD_SCHEMA} lists no cells for {dimension}"))?;
        cells.insert(
            dimension.to_owned(),
            listed
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        );
    }
    Ok(cells)
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{} is not JSON: {err}", path.display()))
}

/// Parses and judges the text of `registry/coverage.toml` against the record schema.
///
/// # Errors
///
/// A message naming the first rule broken.
pub fn parse_required(text: &str, schema: &Value) -> Result<Required, String> {
    let file: RequiredFile =
        toml::from_str(text).map_err(|err| format!("invalid {REQUIRED}: {}", err.message()))?;
    if file.schema != "keyval.coverage-required/v1" {
        return Err(format!(
            "{REQUIRED}: schema is {:?}, expected \"keyval.coverage-required/v1\"",
            file.schema
        ));
    }
    let universe = universe(schema)?;
    let mut seen = BTreeSet::new();
    let mut dimensions = Vec::new();
    for row in file.dimension {
        let Some(known) = universe.get(&row.name) else {
            return Err(format!("{REQUIRED}: unknown dimension {:?}", row.name));
        };
        if !seen.insert(row.name.clone()) {
            return Err(format!(
                "{REQUIRED}: dimension {:?} appears twice",
                row.name
            ));
        }
        if !OWNERS.contains(&row.owner.as_str()) {
            return Err(format!(
                "{REQUIRED}: dimension {:?} has owner {:?}, expected one of {OWNERS:?}",
                row.name, row.owner
            ));
        }
        let mut cells = BTreeSet::new();
        for cell in &row.required {
            if cell == NEVER_REQUIRED {
                return Err(format!(
                    "{REQUIRED}: {} lists {NEVER_REQUIRED:?}, a catch-all that is never required",
                    row.name
                ));
            }
            if !known.contains(cell) {
                return Err(format!(
                    "{REQUIRED}: {} lists {cell:?}, which is not a cell of the matrix",
                    row.name
                ));
            }
            if !cells.insert(cell.clone()) {
                return Err(format!("{REQUIRED}: {} lists {cell:?} twice", row.name));
            }
        }
        if PENDING_DIMENSIONS.contains(&row.name.as_str()) {
            if row.owner != PENDING_OWNER {
                return Err(format!(
                    "{REQUIRED}: {} is completed by {PENDING_OWNER}, not {:?}",
                    row.name, row.owner
                ));
            }
        } else {
            if row.owner == PENDING_OWNER {
                return Err(format!(
                    "{REQUIRED}: {} is owned by W41; {PENDING_OWNER} completes only \
                     {PENDING_DIMENSIONS:?}",
                    row.name
                ));
            }
            let complete: BTreeSet<String> = known
                .iter()
                .filter(|cell| cell.as_str() != NEVER_REQUIRED)
                .cloned()
                .collect();
            if cells != complete {
                let absent: Vec<&String> = complete.difference(&cells).collect();
                return Err(format!(
                    "{REQUIRED}: {} must require every cell of the matrix but \
                     {NEVER_REQUIRED:?}; absent: {absent:?} (a vacuous required set is \
                     a defect, P10)",
                    row.name
                ));
            }
        }
        dimensions.push((row.name, row.owner, cells));
    }
    let missing: Vec<&str> = DIMENSIONS
        .iter()
        .copied()
        .filter(|name| !seen.contains(*name))
        .collect();
    if !missing.is_empty() {
        return Err(format!("{REQUIRED}: dimensions missing: {missing:?}"));
    }
    dimensions.sort_by_key(|(name, _, _)| DIMENSIONS.iter().position(|d| d == name));
    Ok(Required { dimensions })
}

/// One dimension of the required set, as reported.
#[derive(Clone, Debug, Serialize)]
pub struct RequiredDimension {
    /// The dimension.
    pub dimension: String,
    /// Who completes its cells.
    pub owner: String,
    /// How many cells are required.
    pub cells: usize,
    /// True when the dimension has no required cells yet (owned by W43).
    pub pending: bool,
}

/// One empty cell of a build's matrix.
#[derive(Clone, Debug, Serialize)]
pub struct EmptyCell {
    /// The dimension.
    pub dimension: String,
    /// The cell.
    pub cell: String,
    /// Whether the cell is in the required set.
    pub required: bool,
}

/// One build's summed matrix.
#[derive(Clone, Debug, Serialize)]
pub struct Build {
    /// The build the records ran on.
    #[serde(rename = "build_id")]
    pub id: String,
    /// Runs counted over all its records.
    pub runs: u64,
    /// Cells in the matrix.
    pub cells: usize,
    /// Cells with a zero count.
    pub empty: usize,
    /// Required cells.
    pub required_total: usize,
    /// Required cells with a zero count: what T3 would reject.
    pub required_empty: usize,
    /// Every empty cell, in report order.
    pub empty_cells: Vec<EmptyCell>,
}

/// The `./dev report coverage` report (`keyval.dev.coverage/v1`).
#[derive(Clone, Debug, Serialize)]
pub struct CoverageReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when the records were read and the report was rendered and written. Empty
    /// required cells do not make it false: this is a report, T3 is the gate.
    pub ok: bool,
    /// Where the Markdown report was written.
    pub path: String,
    /// The coverage records read, relative to the root.
    pub inputs: Vec<String>,
    /// The required set, per dimension.
    pub required: Vec<RequiredDimension>,
    /// One entry per build, ordered by build ID.
    pub builds: Vec<Build>,
}

impl Report for CoverageReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut text = format!(
            "coverage: {} record(s), {} build(s)\n",
            self.inputs.len(),
            self.builds.len()
        );
        for build in &self.builds {
            let _ = writeln!(
                text,
                "build {}: {} run(s); {} of {} cells empty; {} of {} required cells empty",
                build.id,
                build.runs,
                build.empty,
                build.cells,
                build.required_empty,
                build.required_total
            );
        }
        let pending: Vec<&str> = self
            .required
            .iter()
            .filter(|dimension| dimension.pending)
            .map(|dimension| dimension.dimension.as_str())
            .collect();
        if !pending.is_empty() {
            let _ = writeln!(
                text,
                "required set pending ({PENDING_OWNER}): {}",
                pending.join(", ")
            );
        }
        let _ = write!(text, "written: {}", self.path);
        text
    }
}

/// The coverage records under `<root>/artifacts/coverage/`, sorted.
///
/// # Errors
///
/// A message if the directory exists but cannot be read.
pub fn default_inputs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(RECORDS);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// What a build's records add up to.
#[derive(Default)]
struct Sum {
    runs: u64,
    counts: BTreeMap<(String, String), u64>,
}

/// Renders the report for `inputs` (the default records if empty), writes `REPORT`, and
/// returns it.
///
/// # Errors
///
/// A message if the required set or a record is unreadable or breaks its schema, or the report
/// cannot be written. A malformed record is never skipped (P10).
pub fn run(root: &Path, inputs: &[PathBuf]) -> Result<CoverageReport, String> {
    let schema = read_json(&root.join(RECORD_SCHEMA))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|err| format!("{RECORD_SCHEMA} does not compile: {err}"))?;
    let required_text = fs::read_to_string(root.join(REQUIRED))
        .map_err(|err| format!("cannot read {REQUIRED}: {err}"))?;
    let required = parse_required(&required_text, &schema)?;
    let files = if inputs.is_empty() {
        default_inputs(root)?
    } else {
        inputs.to_vec()
    };
    let mut sums: BTreeMap<String, Sum> = BTreeMap::new();
    for file in &files {
        let record = read_json(file)?;
        let broken: Vec<String> = validator
            .iter_errors(&record)
            .map(|err| format!("{}: {err}", err.instance_path()))
            .collect();
        if !broken.is_empty() {
            return Err(format!(
                "{} is not a keyval.coverage/v1 record: {}",
                file.display(),
                broken.join("; ")
            ));
        }
        let build = record["build_id"].as_str().unwrap_or_default().to_owned();
        let sum = sums.entry(build).or_default();
        sum.runs += record["runs"].as_u64().unwrap_or(0);
        for dimension in DIMENSIONS {
            let row = record["cells"][dimension].as_object();
            for (cell, count) in row.into_iter().flatten() {
                *sum.counts
                    .entry((dimension.to_owned(), cell.clone()))
                    .or_insert(0) += count.as_u64().unwrap_or(0);
            }
        }
    }
    let builds = sums
        .into_iter()
        .map(|(build_id, sum)| build(build_id, &sum, &required))
        .collect();
    let report = CoverageReport {
        schema: SCHEMA,
        ok: true,
        path: REPORT.to_owned(),
        inputs: files
            .iter()
            .map(|file| {
                file.strip_prefix(root)
                    .unwrap_or(file)
                    .display()
                    .to_string()
            })
            .collect(),
        required: required
            .dimensions
            .iter()
            .map(|(name, owner, cells)| RequiredDimension {
                dimension: name.clone(),
                owner: owner.clone(),
                cells: cells.len(),
                pending: cells.is_empty(),
            })
            .collect(),
        builds,
    };
    let out = root.join(REPORT);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    fs::write(&out, markdown(&report))
        .map_err(|err| format!("cannot write {}: {err}", out.display()))?;
    Ok(report)
}

fn build(build_id: String, sum: &Sum, required: &Required) -> Build {
    let mut empty_cells = Vec::new();
    let mut required_empty = 0;
    for dimension in DIMENSIONS {
        for ((name, cell), count) in &sum.counts {
            if name == dimension && *count == 0 {
                let is_required = required.contains(dimension, cell);
                required_empty += usize::from(is_required);
                empty_cells.push(EmptyCell {
                    dimension: dimension.to_owned(),
                    cell: cell.clone(),
                    required: is_required,
                });
            }
        }
    }
    Build {
        id: build_id,
        runs: sum.runs,
        cells: sum.counts.len(),
        empty: empty_cells.len(),
        required_total: required.cells(),
        required_empty,
        empty_cells,
    }
}

fn markdown(report: &CoverageReport) -> String {
    let mut text = String::from("# Coverage matrix (PLAN.md §10.4)\n\n");
    text.push_str(
        "Generated by `./dev report coverage`; do not edit. Evidence of what the runs \
         exercised, not proof: a seed count is never a coverage argument.\n\n",
    );
    let _ = writeln!(text, "Records read: {}\n", report.inputs.len());
    text.push_str("## Required set\n\n| Dimension | Owner | Required cells |\n|---|---|---|\n");
    for dimension in &report.required {
        let cells = if dimension.pending {
            format!("pending ({})", dimension.owner)
        } else {
            dimension.cells.to_string()
        };
        let _ = writeln!(
            text,
            "| {} | {} | {cells} |",
            dimension.dimension, dimension.owner
        );
    }
    if report.builds.is_empty() {
        text.push_str("\nNo coverage record was read: every cell is empty.\n");
    }
    for build in &report.builds {
        let _ = write!(
            text,
            "\n## Build `{}`\n\n{} run(s); {} of {} cells empty; {} of {} required cells empty.\n\n",
            build.id,
            build.runs,
            build.empty,
            build.cells,
            build.required_empty,
            build.required_total
        );
        if build.empty_cells.is_empty() {
            text.push_str("No empty cell.\n");
            continue;
        }
        text.push_str("| Dimension | Empty cell | Required |\n|---|---|---|\n");
        for cell in &build.empty_cells {
            let _ = writeln!(
                text,
                "| {} | `{}` | {} |",
                cell.dimension,
                cell.cell,
                if cell.required { "**yes**" } else { "no" }
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_support::{scratch_dir, workspace_root};

    fn schema() -> Value {
        read_json(&workspace_root().join(RECORD_SCHEMA)).unwrap()
    }

    fn required_text() -> String {
        fs::read_to_string(workspace_root().join(REQUIRED)).unwrap()
    }

    fn fixture() -> Value {
        read_json(&workspace_root().join("schemas/coverage/fixtures/campaign.v1.json")).unwrap()
    }

    /// A scratch workspace holding the real schema and required set.
    fn workspace(name: &str) -> PathBuf {
        let root = scratch_dir(name);
        for file in [RECORD_SCHEMA, REQUIRED] {
            let to = root.join(file);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(workspace_root().join(file), to).unwrap();
        }
        root
    }

    fn write_record(root: &Path, name: &str, record: &Value) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, serde_json::to_string(record).unwrap()).unwrap();
        path
    }

    // check: COVERAGE-REPORT
    #[test]
    fn the_real_required_set_parses_with_all_six_dimensions() {
        let required = parse_required(&required_text(), &schema()).unwrap();
        let names: Vec<&str> = required.dimensions.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(
            names, DIMENSIONS,
            "rule: the required set names every dimension, in report order"
        );
        for (name, owner, cells) in &required.dimensions {
            assert!(
                !cells.is_empty(),
                "rule: W43 completed the outcome and operation cells, so no dimension is \
                 pending; dimension={name} owner={owner}"
            );
        }
        assert_eq!(
            required.cells(),
            9 + 36 + 4 + 8 + 10 + 6,
            "rule: faults, pairs, the four named topologies, crash points, every outcome and \
             every operation are required"
        );
    }

    #[test]
    fn a_required_set_with_a_defect_is_refused() {
        let good = required_text();
        let cases = [
            (
                good.replacen("\"stall\",", "\"stalls\",", 1),
                "not a cell of the matrix",
            ),
            (
                good.replacen("\"3/2\",", "\"other_partition\",", 1),
                "never required",
            ),
            (
                good.replacen("\"idle\",", "\"idle\", \"idle\",", 1),
                "twice",
            ),
            (
                good.replacen("owner = \"W41\"", "owner = \"W99\"", 1),
                "has owner",
            ),
            (
                good.replace("name = \"operation\"", "name = \"verb\""),
                "unknown dimension",
            ),
            (
                good.replace("keyval.coverage-required/v1", "keyval.coverage-required/v9"),
                "schema is",
            ),
            (
                good.replacen("[[dimension]]", "[[dimension]]\nunexpected = 1", 1),
                "invalid",
            ),
        ];
        for (text, expected) in cases {
            assert_ne!(
                text, good,
                "rule: each case really changes the file; {expected}"
            );
            let err = parse_required(&text, &schema()).unwrap_err();
            assert!(
                err.contains(expected),
                "rule: a defect is named; expected={expected:?} err={err}"
            );
        }
    }

    #[test]
    fn a_duplicate_a_missing_and_a_vacuous_dimension_are_refused() {
        let good = required_text();
        let drop_operation = good
            .rsplit_once("# Completed by W43: every operation")
            .unwrap()
            .0
            .to_owned();
        let err = parse_required(&drop_operation, &schema()).unwrap_err();
        assert!(
            err.contains("missing"),
            "rule: a missing dimension is named; err={err}"
        );
        let duplicate = format!(
            "{good}\n[[dimension]]\nname = \"operation\"\nowner = \"W43\"\nrequired = []\n"
        );
        let err = parse_required(&duplicate, &schema()).unwrap_err();
        assert!(
            err.contains("twice"),
            "rule: a duplicated dimension is named; err={err}"
        );
        let without_stall = good.replacen("\"stall\",", "", 1);
        let err = parse_required(&without_stall, &schema()).unwrap_err();
        assert!(
            err.contains("must require every cell") && err.contains("stall"),
            "rule: a W41 dimension that drops a cell is refused (P10); err={err}"
        );
        let stolen = good.replacen(
            "name = \"topology\"\nowner = \"W41\"",
            "name = \"topology\"\nowner = \"W43\"",
            1,
        );
        let err = parse_required(&stolen, &schema()).unwrap_err();
        assert!(
            err.contains("owned by W41"),
            "rule: W43 completes only outcome and operation (D-73); err={err}"
        );
    }

    #[test]
    fn the_fixture_record_conforms_to_the_record_schema() {
        let validator = jsonschema::validator_for(&schema()).unwrap();
        let broken: Vec<String> = validator
            .iter_errors(&fixture())
            .map(|err| err.to_string())
            .collect();
        assert!(
            broken.is_empty(),
            "rule: the campaign fixture is a keyval.coverage/v1 record; violations={broken:?}"
        );
        let mut missing = fixture();
        missing["cells"]["fault"]
            .as_object_mut()
            .unwrap()
            .remove("stall");
        assert!(
            !validator.is_valid(&missing),
            "rule: a record without a cell is rejected, so an empty cell is never implicit"
        );
    }

    #[test]
    fn a_record_is_rendered_with_its_empty_cells_and_required_flags() {
        let root = workspace("coverage-render");
        let record = write_record(&root, "campaign.json", &fixture());
        let report = run(&root, &[record]).unwrap();
        assert!(report.ok, "rule: a rendered report is ok; {report:?}");
        assert_eq!(report.builds.len(), 1, "rule: one build; {report:?}");
        let build = &report.builds[0];
        assert_eq!(build.id, "fixture-build", "rule: build named");
        assert_eq!(build.runs, 8, "rule: the record's runs");
        assert_eq!(
            build.cells,
            9 + 36 + 5 + 8 + 10 + 6,
            "rule: every cell is in the matrix"
        );
        // Empty required: faults 9-5, pairs 36-3, topologies 4-3, crash points 8-3.
        assert_eq!(
            build.required_empty,
            4 + 33 + 1 + 5 + 5 + 2,
            "rule: the fixture leaves these required cells empty (outcomes conflict, \
             live_capacity, history_full, busy, recovering; operations delete, scan); {build:?}"
        );
        assert_eq!(build.required_total, 73, "rule: required cells total");
        let outcome_required = build
            .empty_cells
            .iter()
            .find(|cell| cell.dimension == "outcome" && cell.cell == "conflict")
            .unwrap();
        assert!(
            outcome_required.required,
            "rule: W43 made every outcome cell required; {outcome_required:?}"
        );
        let text = fs::read_to_string(root.join(REPORT)).unwrap();
        assert!(
            text.contains("`asymmetric_cycle` | **yes**"),
            "rule: the Markdown flags an empty required cell; text={text}"
        );
        assert!(
            !text.contains("pending ("),
            "rule: no dimension is pending once W43 completed the required set; text={text}"
        );
        assert!(
            report.render_human().contains("required cells empty"),
            "rule: the human rendering summarises required cells"
        );
    }

    #[test]
    fn records_of_one_build_are_summed_and_builds_are_kept_apart() {
        let root = workspace("coverage-sum");
        let one = write_record(&root, "one.json", &fixture());
        let mut second = fixture();
        second["runs"] = json!(2);
        second["cells"]["fault"]["all_node_crash"] = json!(3);
        let two = write_record(&root, "two.json", &second);
        let mut elsewhere = fixture();
        elsewhere["build_id"] = json!("another-build");
        let three = write_record(&root, "three.json", &elsewhere);
        let report = run(&root, &[one, two, three]).unwrap();
        assert_eq!(report.builds.len(), 2, "rule: one entry per build");
        let ids: Vec<&str> = report.builds.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["another-build", "fixture-build"],
            "rule: builds are ordered by ID"
        );
        let summed = &report.builds[1];
        assert_eq!(summed.runs, 10, "rule: runs of one build add");
        assert_eq!(
            summed.required_empty,
            3 + 33 + 1 + 5 + 5 + 2,
            "rule: a cell the second record exercised is no longer empty; {summed:?}"
        );
        assert_eq!(
            report.builds[0].required_empty,
            4 + 33 + 1 + 5 + 5 + 2,
            "rule: coverage never carries across builds"
        );
    }

    #[test]
    fn a_malformed_record_is_an_error_and_no_record_is_an_empty_report() {
        let root = workspace("coverage-bad");
        let mut broken = fixture();
        broken["cells"]["operation"]
            .as_object_mut()
            .unwrap()
            .remove("scan");
        let path = write_record(&root, "broken.json", &broken);
        let err = run(&root, &[path]).unwrap_err();
        assert!(
            err.contains("broken.json") && err.contains("keyval.coverage/v1"),
            "rule: a record that breaks the schema is named, never skipped (P10); err={err}"
        );
        let empty = run(&root, &[]).unwrap();
        assert!(
            empty.ok && empty.builds.is_empty() && empty.inputs.is_empty(),
            "rule: with no record the report says so and lists no build; {empty:?}"
        );
        let text = fs::read_to_string(root.join(REPORT)).unwrap();
        assert!(
            text.contains("No coverage record was read"),
            "rule: the Markdown does not pretend coverage; text={text}"
        );
    }

    #[test]
    fn records_are_found_in_the_default_directory_in_order() {
        let root = workspace("coverage-default");
        let dir = root.join(RECORDS);
        fs::create_dir_all(&dir).unwrap();
        write_record(&dir, "b.json", &fixture());
        write_record(&dir, "a.json", &fixture());
        fs::write(dir.join("notes.txt"), "not a record").unwrap();
        let report = run(&root, &[]).unwrap();
        assert_eq!(
            report.inputs,
            ["artifacts/coverage/a.json", "artifacts/coverage/b.json"],
            "rule: every *.json under artifacts/coverage is read, sorted"
        );
        assert_eq!(report.builds[0].runs, 16, "rule: both records are summed");
    }
}
