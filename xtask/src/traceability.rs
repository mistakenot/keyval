//! `./dev report traceability`: renders `reports/traceability.md` from the verification
//! registry (PLAN.md §10.13 "Reports"; W05.3).
//!
//! For each §6 requirement the report gives:
//! - its level, title, owner disposition (`notes`), declared status and the status
//!   §10.13 derives from the closed work items that list it;
//! - its covering checks from `registry/checks.toml`, with kind, claim class (§10.15),
//!   tier and status, and each check's latest receipt;
//! - whether it is **passed** on the judged tree: active, and every covering check's
//!   latest receipt on that tree reports `passed`. That is computed here from receipts,
//!   never stored (§10.13). Only a receipt of the check's whole family counts: a full,
//!   unfiltered selection that ran at least the check's `min_cases` (P10). A `--case` run
//!   never stands in for the family (§10.13, br-traceability-passed-unsupported-ykqx).
//!
//! When a tree is judged, the registry, the work graph, the tracker export and the
//! registry gate are all read from that tree, materialised into `.local/traceability/`,
//! never from the working files, which may hold someone's uncommitted edit (the rule in
//! `registry.rs`). Only without git (a git-isolated candidate) are the working files read.
//!
//! It also gives the registry gate's verdict (§10.13 conditions 1–8), including the
//! conditions it only reports (5 and 6 until W32).
//!
//! Receipts are read from `artifacts/receipts/`. Accept receipts (`keyval.dev.accept/v1`)
//! give one line per check run (`<ID> <status> (<N> cases)`). Gate receipts give a step
//! for each static check they ran. The report is a view, not a gate: it renders whatever
//! it finds and leaves the verdict to the checks.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::Git;
use crate::output::Report;
use crate::registry::{self, Item};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.traceability/v2";

/// Where the report is written, relative to the workspace root (generated, not committed).
pub const REPORT: &str = "reports/traceability.md";

/// Where a judged tree is materialised to read its registry, relative to the root.
pub const WORK: &str = ".local/traceability";

/// The check registry, relative to the root (W05.1).
const CHECKS: &str = "registry/checks.toml";

/// The tracker export, relative to the root.
const TRACKER_EXPORT: &str = ".beads/issues.jsonl";

/// Where a report run reads and writes.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Where a judged tree is materialised (`.local/traceability/`), so that its registry
    /// is read instead of the working files.
    pub work: PathBuf,
    /// The artifacts directory (`receipts/` lives under it).
    pub artifacts: PathBuf,
    /// The Markdown file to write.
    pub out: PathBuf,
}

/// One `[[requirement]]` row, with the fields the report shows.
#[derive(Clone, Debug, Deserialize)]
pub struct RequirementRow {
    /// The §6 ID.
    pub id: String,
    /// MUST, SHOULD or MAY.
    pub level: String,
    /// The declared status: `planned` or `active`.
    pub status: String,
    /// Short title.
    pub title: String,
    /// Owner disposition for a qualified, SHOULD or MAY requirement.
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Deserialize)]
struct RequirementsFile {
    #[serde(default)]
    requirement: Vec<RequirementRow>,
}

/// One `[[check]]` row, with the fields the report shows.
#[derive(Clone, Debug, Deserialize)]
pub struct CheckRow {
    /// Registry ID.
    pub id: String,
    /// `test_group`, `static`, ….
    pub kind: String,
    /// Requirement IDs it covers.
    #[serde(default)]
    pub covers: Vec<String>,
    /// Claim class (§10.15).
    pub class: String,
    /// Gate tier.
    pub tier: String,
    /// Declared status: `planned` or `implemented`.
    pub status: String,
    /// The fewest cases a full run must execute (P10).
    pub min_cases: u32,
}

#[derive(Deserialize)]
struct ChecksFile {
    #[serde(default)]
    check: Vec<CheckRow>,
}

/// Where the report read the registry from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    /// The judged tree, materialised.
    Tree,
    /// The working files: no tree was known (no git).
    WorkingTree,
}

/// What kind of receipt recorded a check's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An accept receipt (`keyval.dev.accept/v1`): a selector ran the check.
    Accept,
    /// A gate receipt (`keyval.dev.receipt/v*`): the gate ran the check as a step.
    Gate,
}

/// One check result found in a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// The check.
    pub check: String,
    /// `passed`, `failed`, …, as the receipt states it.
    pub outcome: String,
    /// Cases the run executed (accept: passed cases; gate: examined cases).
    pub cases: Option<u64>,
    /// The tree the check ran on.
    pub tree: String,
    /// When the receipt was finished (UTC, RFC 3339).
    pub at: String,
    /// The receipt file.
    pub receipt: String,
    /// The kind of receipt.
    pub source: Source,
    /// Whether the run selected the check's whole family. A `--case` run did not, and so
    /// never counts toward "passed".
    pub full: bool,
}

/// Everything the report is computed from, so that the computation is testable without
/// files.
pub struct Inputs {
    /// `registry/requirements.toml` rows, in order.
    pub requirements: Vec<RequirementRow>,
    /// `registry/checks.toml` rows, in order.
    pub checks: Vec<CheckRow>,
    /// `registry/workgraph.toml` items.
    pub items: Vec<Item>,
    /// The IDs of the work items whose bead is closed.
    pub closed: BTreeSet<String>,
    /// Every check result found in the receipts.
    pub evidence: Vec<Evidence>,
    /// The tree "passed" is judged on, if known.
    pub tree: Option<String>,
    /// The registry gate's judgement, or why it could not run.
    pub registry: RegistryVerdict,
    /// Where the registry was read from.
    pub source: InputSource,
}

/// The registry gate's verdict, as the report shows it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RegistryVerdict {
    /// Failed conditions, one line each.
    pub violations: Vec<String>,
    /// Conditions reported but not failed (5 and 6 until W32), one line each.
    pub reported: Vec<String>,
    /// Why the gate could not run, if it could not.
    pub error: Option<String>,
}

/// A work item that lists a requirement, and whether it has closed.
#[derive(Clone, Debug, Serialize)]
pub struct Listing {
    /// The §12.2 item ID.
    pub item: String,
    /// Whether its bead is closed.
    pub closed: bool,
}

/// A covering check, with its latest evidence.
#[derive(Clone, Debug, Serialize)]
pub struct Covering {
    /// Registry ID.
    pub id: String,
    /// Registry kind.
    pub kind: String,
    /// Claim class (§10.15).
    pub class: String,
    /// Gate tier.
    pub tier: String,
    /// Declared status.
    pub status: String,
    /// The fewest cases a full run must execute.
    pub min_cases: u32,
    /// The newest receipt naming this check, on any tree and of any selection.
    pub latest: Option<Evidence>,
    /// The newest full-selection receipt naming this check on the judged tree: the one
    /// "passed" is judged by.
    pub on_tree: Option<Evidence>,
    /// Whether `on_tree` passed with at least `min_cases` cases.
    pub passes: bool,
}

/// One requirement's line in the report.
#[derive(Clone, Debug, Serialize)]
pub struct RequirementTrace {
    /// The §6 ID.
    pub id: String,
    /// MUST, SHOULD or MAY.
    pub level: String,
    /// Short title.
    pub title: String,
    /// Owner disposition, if any.
    pub notes: Option<String>,
    /// The status `registry/requirements.toml` declares.
    pub status: String,
    /// The status §10.13 derives from the work graph and the tracker.
    pub derived: String,
    /// The work items listing it.
    pub listed_by: Vec<Listing>,
    /// Its covering checks.
    pub checks: Vec<Covering>,
    /// Whether it is passed on the judged tree; `None` without a tree.
    pub passed: Option<bool>,
}

/// Counts across the report.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    /// Requirements in the registry.
    pub requirements: usize,
    /// Requirements declared active.
    pub active: usize,
    /// Requirements passed on the judged tree.
    pub passed: usize,
    /// Active MUST requirements without an implemented covering check (§10.13 cond. 2).
    pub uncovered: Vec<String>,
    /// Requirements whose declared status differs from the derived one.
    pub mismatched: Vec<String>,
}

/// The `./dev report traceability` report (`keyval.dev.traceability/v2`). v2 adds the
/// full-selection and `min_cases` evidence rules and reads the registry from the judged
/// tree.
#[derive(Clone, Debug, Serialize)]
pub struct TraceabilityReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when the report was rendered and written.
    pub ok: bool,
    /// Where the Markdown report was written.
    pub path: String,
    /// The tree "passed" is judged on, if known.
    pub tree: Option<String>,
    /// Where the registry was read from.
    pub inputs: InputSource,
    /// Counts.
    pub summary: Summary,
    /// The registry gate's verdict.
    pub registry: RegistryVerdict,
    /// One entry per requirement, in registry order.
    pub requirements: Vec<RequirementTrace>,
}

impl Report for TraceabilityReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let s = &self.summary;
        let tree = self
            .tree
            .as_deref()
            .map_or_else(|| "no tree".to_owned(), short);
        format!(
            "traceability: {} requirement(s), {} active, {} passed on {tree}; {} active MUST \
             uncovered; {} declared status(es) differ from derived\n\
             registry gate: {} violation(s), {} reported\nwritten: {}",
            s.requirements,
            s.active,
            s.passed,
            s.uncovered.len(),
            s.mismatched.len(),
            self.registry.violations.len(),
            self.registry.reported.len(),
            self.path
        )
    }
}

/// Renders the report for `git`'s checkout and `layout`, judging "passed" on `tree`
/// (default: the checkout's `HEAD^{tree}`, when git can tell), and writes the Markdown
/// file. A judged tree's registry is read from the tree; without one, from the working
/// files under `git.root()`.
pub fn run(git: &Git, layout: &Layout, tree: Option<String>) -> Result<TraceabilityReport, String> {
    let tree = tree.or_else(|| git.rev_parse("HEAD^{tree}").ok());
    // Held until the inputs are read, so that two reports never refresh `work` under each
    // other.
    let _lock = tree.as_ref().map(|_| lock(&layout.work)).transpose()?;
    let (dir, source) = match tree.as_deref() {
        Some(id) => {
            crate::candidate::materialise(git, id, &layout.work).map_err(|why| {
                format!(
                    "cannot read the registry of tree {id} (materialising into {}): {why}",
                    layout.work.display()
                )
            })?;
            (layout.work.as_path(), InputSource::Tree)
        }
        None => (git.root(), InputSource::WorkingTree),
    };
    let inputs = load(dir, &layout.artifacts, tree, source)?;
    let mut report = build(&inputs);
    report.path = layout.out.display().to_string();
    if let Some(dir) = layout.out.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }
    fs::write(&layout.out, render_markdown(&report))
        .map_err(|err| format!("cannot write {}: {err}", layout.out.display()))?;
    report.ok = true;
    Ok(report)
}

/// Locks `<work>.lock`, waiting for another report that holds it.
fn lock(work: &Path) -> Result<File, String> {
    let path = work.with_extension("lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    file.lock()
        .map_err(|err| format!("traceability lock {}: {err}", path.display()))?;
    Ok(file)
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    let full = root.join(path);
    fs::read_to_string(&full).map_err(|err| format!("cannot read {}: {err}", full.display()))
}

/// Reads the registry, the work graph and the tracker export from `root`, the receipts from
/// `artifacts`, and runs the registry gate on `root`.
fn load(
    root: &Path,
    artifacts: &Path,
    tree: Option<String>,
    source: InputSource,
) -> Result<Inputs, String> {
    let requirements: RequirementsFile = toml::from_str(&read(root, registry::REQUIREMENTS)?)
        .map_err(|err| format!("invalid {}: {}", registry::REQUIREMENTS, err.message()))?;
    let checks: ChecksFile = toml::from_str(&read(root, CHECKS)?)
        .map_err(|err| format!("invalid {CHECKS}: {}", err.message()))?;
    let items = registry::parse_workgraph(&read(root, registry::WORKGRAPH)?)?;
    let export = read(root, TRACKER_EXPORT).unwrap_or_default();
    let closed = registry::closed_items(&items, &export);
    let registry = match crate::registry_gate::check_registry(root) {
        Ok(judgement) => RegistryVerdict {
            violations: judgement.violations,
            reported: judgement.reported,
            error: None,
        },
        Err(why) => RegistryVerdict {
            violations: Vec::new(),
            reported: Vec::new(),
            error: Some(why),
        },
    };
    Ok(Inputs {
        requirements: requirements.requirement,
        checks: checks.check,
        items,
        closed,
        evidence: receipts_evidence(&artifacts.join("receipts")),
        tree,
        registry,
        source,
    })
}

/// Every check result in the receipts directly under `dir`; unreadable or foreign files
/// are skipped (a view renders what it finds).
fn receipts_evidence(dir: &Path) -> Vec<Evidence> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut evidence = Vec::new();
    for path in entries.filter_map(|entry| entry.ok().map(|e| e.path())) {
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(receipt) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        evidence.extend(evidence_in(&receipt, &path.display().to_string()));
    }
    evidence
}

/// The check results one receipt records.
fn evidence_in(receipt: &Value, path: &str) -> Vec<Evidence> {
    let schema = receipt["schema"].as_str().unwrap_or_default();
    let at = receipt["finished_at"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let found =
        |check: &str, outcome: &str, cases: Option<u64>, tree: &str, source, full| Evidence {
            check: check.to_owned(),
            outcome: outcome.to_owned(),
            cases,
            tree: tree.to_owned(),
            at: at.clone(),
            receipt: path.to_owned(),
            source,
            full,
        };
    if schema == crate::accept::SCHEMA {
        let tree = receipt["tree"].as_str().unwrap_or_default();
        let mut evidence = Vec::new();
        for selector in receipt["selectors"].as_array().into_iter().flatten() {
            // A selector naming `--case` (either clap spelling) ran part of a family; one
            // without a recorded selector is not trusted as whole either.
            let full = selector["selector"].as_str().is_some_and(|s| {
                !s.split_whitespace()
                    .any(|word| word == "--case" || word.starts_with("--case="))
            });
            let lines = selector["checks"].as_array().into_iter().flatten();
            for (check, outcome, cases) in
                lines.filter_map(Value::as_str).filter_map(parse_check_line)
            {
                evidence.push(found(check, outcome, cases, tree, Source::Accept, full));
            }
        }
        return evidence;
    }
    if family(schema) == family(crate::gate::RECEIPT_SCHEMA) {
        let tree = receipt["tree"]["id"].as_str().unwrap_or_default();
        return receipt["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|step| {
                let id = step["id"].as_str()?;
                let outcome = step["status"].as_str()?;
                // A gate step runs its whole static check.
                Some(found(
                    id,
                    outcome,
                    step["cases"].as_u64(),
                    tree,
                    Source::Gate,
                    true,
                ))
            })
            .collect();
    }
    Vec::new()
}

/// An identifier without its version: `keyval.dev.receipt/v3` becomes `keyval.dev.receipt`,
/// so gate receipts of every version are read.
fn family(id: &str) -> &str {
    id.split_once('/').map_or(id, |(name, _)| name)
}

/// An accept receipt's check line: `<ID> <status> (<N> cases)`, as `accept::LiveRunner`
/// writes it.
fn parse_check_line(line: &str) -> Option<(&str, &str, Option<u64>)> {
    let (id, rest) = line.split_once(' ')?;
    let (outcome, cases) = rest.split_once(' ').unwrap_or((rest, ""));
    let cases = cases
        .strip_prefix('(')
        .and_then(|c| c.strip_suffix(" cases)"))
        .and_then(|n| n.parse().ok());
    let well_formed = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    well_formed.then_some((id, outcome, cases))
}

/// The newest of `evidence` for `check`, optionally only on `tree`, and optionally only
/// from full selections.
fn newest<'a>(
    evidence: &'a [Evidence],
    check: &str,
    tree: Option<&str>,
    full_only: bool,
) -> Option<&'a Evidence> {
    evidence
        .iter()
        .filter(|e| e.check == check && tree.is_none_or(|t| e.tree == t) && (e.full || !full_only))
        .max_by(|a, b| (&a.at, &a.receipt).cmp(&(&b.at, &b.receipt)))
}

/// Whether `evidence` shows the check passed as a family: `passed`, with at least
/// `min_cases` cases (P10).
fn passes(evidence: Option<&Evidence>, min_cases: u32) -> bool {
    evidence.is_some_and(|e| {
        e.full && e.outcome == "passed" && e.cases.is_some_and(|n| n >= u64::from(min_cases))
    })
}

/// Computes the report from `inputs` (without writing anything).
pub fn build(inputs: &Inputs) -> TraceabilityReport {
    let requirements: Vec<RequirementTrace> = inputs
        .requirements
        .iter()
        .map(|row| trace(inputs, row))
        .collect();
    let summary = Summary {
        requirements: requirements.len(),
        active: requirements.iter().filter(|r| r.status == "active").count(),
        passed: requirements
            .iter()
            .filter(|r| r.passed == Some(true))
            .count(),
        uncovered: requirements
            .iter()
            .filter(|r| {
                r.status == "active"
                    && r.level == "MUST"
                    && !r.checks.iter().any(|c| c.status == "implemented")
            })
            .map(|r| r.id.clone())
            .collect(),
        mismatched: requirements
            .iter()
            .filter(|r| r.status != r.derived)
            .map(|r| r.id.clone())
            .collect(),
    };
    TraceabilityReport {
        schema: SCHEMA,
        ok: false,
        path: String::new(),
        tree: inputs.tree.clone(),
        inputs: inputs.source,
        summary,
        registry: inputs.registry.clone(),
        requirements,
    }
}

fn trace(inputs: &Inputs, row: &RequirementRow) -> RequirementTrace {
    let listed_by = inputs
        .items
        .iter()
        .filter(|item| item.reqs.iter().any(|r| r == &row.id))
        .map(|item| Listing {
            item: item.id.clone(),
            closed: inputs.closed.contains(&item.id),
        })
        .collect();
    let checks: Vec<Covering> = inputs
        .checks
        .iter()
        .filter(|check| check.covers.iter().any(|c| c == &row.id))
        .map(|check| {
            let on_tree = inputs
                .tree
                .as_deref()
                .and_then(|tree| newest(&inputs.evidence, &check.id, Some(tree), true))
                .cloned();
            Covering {
                id: check.id.clone(),
                kind: check.kind.clone(),
                class: check.class.clone(),
                tier: check.tier.clone(),
                status: check.status.clone(),
                min_cases: check.min_cases,
                latest: newest(&inputs.evidence, &check.id, None, false).cloned(),
                passes: passes(on_tree.as_ref(), check.min_cases),
                on_tree,
            }
        })
        .collect();
    let passed = inputs
        .tree
        .as_ref()
        .map(|_| row.status == "active" && !checks.is_empty() && checks.iter().all(|c| c.passes));
    RequirementTrace {
        id: row.id.clone(),
        level: row.level.clone(),
        title: row.title.clone(),
        notes: row.notes.clone(),
        status: row.status.clone(),
        derived: registry::derived_status(&inputs.items, &row.id, &inputs.closed).to_owned(),
        listed_by,
        checks,
        passed,
    }
}

// --- Markdown. ---

/// The first 12 characters of a tree or commit ID.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// `text` made safe for one Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn evidence_cell(evidence: Option<&Evidence>) -> String {
    evidence.map_or_else(
        || "none".to_owned(),
        |e| {
            let cases = e.cases.map_or_else(String::new, |n| format!(", {n} cases"));
            let file = Path::new(&e.receipt)
                .file_name()
                .map_or_else(|| e.receipt.clone(), |f| f.to_string_lossy().into_owned());
            let partial = if e.full { "" } else { " (a --case run)" };
            format!(
                "{}{cases}{partial} on `{}` at {} (`{file}`)",
                e.outcome,
                short(&e.tree),
                e.at
            )
        },
    )
}

/// Whether a covering check counts toward "passed" on the judged tree, and if not, why.
fn counts_cell(c: &Covering) -> String {
    match &c.on_tree {
        _ if c.passes => "yes".to_owned(),
        None => "no: no full run on the tree".to_owned(),
        Some(e) if e.outcome != "passed" => format!("no: {}", e.outcome),
        Some(e) => format!(
            "no: {} < min_cases {}",
            e.cases
                .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
            c.min_cases
        ),
    }
}

/// The Markdown report.
pub fn render_markdown(report: &TraceabilityReport) -> String {
    let mut md = String::new();
    let tree = report
        .tree
        .as_deref()
        .map_or_else(|| "unknown (no git)".to_owned(), |t| format!("`{t}`"));
    let s = &report.summary;
    let _ = writeln!(md, "# Traceability (PLAN.md §10.13)\n");
    let _ = writeln!(
        md,
        "Generated by `./dev report traceability` from `registry/requirements.toml`, \
         `registry/checks.toml`, `registry/workgraph.toml`, the tracker export and \
         `artifacts/receipts/`. Do not edit; regenerate. Judged tree: {tree}. Registry \
         read from: {}.\n",
        match report.inputs {
            InputSource::Tree => "the judged tree",
            InputSource::WorkingTree => "the working files (no tree known)",
        }
    );
    let _ = writeln!(md, "## Summary\n\n| | |\n|---|---|");
    let _ = writeln!(md, "| Requirements | {} |", s.requirements);
    let _ = writeln!(md, "| Active | {} |", s.active);
    let _ = writeln!(md, "| Passed on the judged tree | {} |", s.passed);
    let _ = writeln!(
        md,
        "| Active MUST without an implemented covering check | {} |",
        list_or_none(&s.uncovered)
    );
    let _ = writeln!(
        md,
        "| Declared status differs from derived | {} |\n",
        list_or_none(&s.mismatched)
    );
    render_registry(&mut md, &report.registry);
    let _ = writeln!(md, "## Requirements\n");
    for r in &report.requirements {
        render_requirement(&mut md, r);
    }
    md
}

fn list_or_none(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".to_owned()
    } else {
        ids.join(", ")
    }
}

fn render_registry(md: &mut String, verdict: &RegistryVerdict) {
    let _ = writeln!(md, "## Registry gate (§10.13 conditions 1–8)\n");
    if let Some(error) = &verdict.error {
        let _ = writeln!(md, "The registry gate could not run: {}\n", cell(error));
        return;
    }
    let mut section = |title: &str, lines: &[String]| {
        let _ = writeln!(
            md,
            "**{title}:** {}\n",
            if lines.is_empty() { "none" } else { "" }
        );
        for line in lines {
            let _ = writeln!(md, "- {}", cell(line));
        }
        if !lines.is_empty() {
            md.push('\n');
        }
    };
    section("Violations", &verdict.violations);
    section(
        "Reported, not failed (conditions 5 and 6 until W32)",
        &verdict.reported,
    );
}

fn render_requirement(md: &mut String, r: &RequirementTrace) {
    let status = if r.status == r.derived {
        r.status.clone()
    } else {
        format!("{} (derives {})", r.status, r.derived)
    };
    let passed = match r.passed {
        Some(true) => "passed",
        Some(false) => "not passed",
        None => "not judged",
    };
    let _ = writeln!(md, "### {} — {} — {status} — {passed}\n", r.id, r.level);
    let _ = writeln!(md, "{}\n", cell(&r.title));
    if let Some(notes) = &r.notes {
        let _ = writeln!(md, "- Disposition: {}", cell(notes));
    }
    let listed: Vec<String> = r
        .listed_by
        .iter()
        .map(|l| format!("{} ({})", l.item, if l.closed { "closed" } else { "open" }))
        .collect();
    let _ = writeln!(md, "- Listed by: {}\n", list_or_none(&listed));
    if r.checks.is_empty() {
        let _ = writeln!(md, "No covering check.\n");
        return;
    }
    let _ = writeln!(
        md,
        "| Check | Kind | Class | Tier | Status | Min cases | Latest receipt | Newest full run \
         on the judged tree | Counts |\n|---|---|---|---|---|---|---|---|---|"
    );
    for c in &r.checks {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            c.id,
            c.kind,
            c.class,
            c.tier,
            c.status,
            c.min_cases,
            cell(&evidence_cell(c.latest.as_ref())),
            cell(&evidence_cell(c.on_tree.as_ref())),
            counts_cell(c)
        );
    }
    md.push('\n');
}

#[cfg(test)]
mod tests {
    // check: TRACEABILITY
    use super::*;
    use crate::test_support::{scratch_dir, workspace_root};

    const TREE: &str = "1111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222";

    fn requirement(id: &str, level: &str, status: &str) -> RequirementRow {
        RequirementRow {
            id: id.to_owned(),
            level: level.to_owned(),
            status: status.to_owned(),
            title: format!("{id} | title"),
            notes: None,
        }
    }

    fn check(id: &str, covers: &[&str], status: &str) -> CheckRow {
        CheckRow {
            id: id.to_owned(),
            kind: "test_group".to_owned(),
            covers: covers.iter().map(|&c| c.to_owned()).collect(),
            class: "invariant".to_owned(),
            tier: "T0".to_owned(),
            status: status.to_owned(),
            min_cases: 3,
        }
    }

    fn evidence(check: &str, outcome: &str, tree: &str, at: &str) -> Evidence {
        Evidence {
            check: check.to_owned(),
            outcome: outcome.to_owned(),
            cases: Some(3),
            tree: tree.to_owned(),
            at: at.to_owned(),
            receipt: format!("artifacts/receipts/accept-{check}-{at}.json"),
            source: Source::Accept,
            full: true,
        }
    }

    fn item(id: &str, reqs: &[&str]) -> Item {
        Item {
            id: id.to_owned(),
            reqs: reqs.iter().map(|&r| r.to_owned()).collect(),
            accept: Vec::new(),
            bead: String::new(),
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            requirements: vec![
                requirement("O-6", "MUST", "active"),
                requirement("O-8", "MUST", "active"),
                requirement("F-1", "MUST", "planned"),
                requirement("C-7", "SHOULD", "active"),
            ],
            checks: vec![
                check("DEV-SCHEMAS", &["O-6"], "implemented"),
                check("XTASK", &["O-6"], "implemented"),
                check("DEV-DOCTOR", &["O-8"], "implemented"),
                check("SPEC", &["F-1"], "planned"),
            ],
            items: vec![
                item("W01", &["O-8", "O-6"]),
                item("W03", &["F-1"]),
                item("W49", &["O-8"]),
            ],
            closed: ["W01".to_owned(), "W03".to_owned()].into(),
            evidence: vec![
                evidence("DEV-SCHEMAS", "failed", TREE, "2026-10-03T20:00:00Z"),
                evidence("DEV-SCHEMAS", "passed", TREE, "2026-10-03T21:00:00Z"),
                evidence("XTASK", "passed", TREE, "2026-10-03T20:30:00Z"),
                evidence("XTASK", "failed", OTHER, "2026-10-03T22:00:00Z"),
                evidence("DEV-DOCTOR", "passed", OTHER, "2026-10-03T21:00:00Z"),
            ],
            tree: Some(TREE.to_owned()),
            registry: RegistryVerdict {
                violations: Vec::new(),
                reported: vec![
                    "condition 6 (reported until W32): put has no conformance test".to_owned(),
                ],
                error: None,
            },
            source: InputSource::Tree,
        }
    }

    fn find<'a>(report: &'a TraceabilityReport, id: &str) -> &'a RequirementTrace {
        report.requirements.iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn accept_check_lines_parse_and_garbage_does_not() {
        assert_eq!(
            parse_check_line("DEV-COMMIT passed (14 cases)"),
            Some(("DEV-COMMIT", "passed", Some(14)))
        );
        assert_eq!(
            parse_check_line("SPEC failed (0 cases)"),
            Some(("SPEC", "failed", Some(0)))
        );
        assert_eq!(
            parse_check_line("NOPE unknown"),
            Some(("NOPE", "unknown", None))
        );
        assert_eq!(
            parse_check_line("\"SPEC\" \"passed\" 3"),
            None,
            "the legacy quoted form names no ID"
        );
        assert_eq!(parse_check_line("garbage"), None);
    }

    #[test]
    fn receipts_yield_accept_lines_and_gate_steps() {
        let accept = serde_json::json!({
            "schema": crate::accept::SCHEMA, "tree": TREE, "finished_at": "2026-10-03T21:00:00Z",
            "selectors": [
                { "selector": "./dev check RAFT-AUDIT XTASK",
                  "checks": ["RAFT-AUDIT passed (32 cases)", "XTASK failed (40 cases)"] },
                { "selector": "./dev check DEV-GATE --case isolation",
                  "checks": ["DEV-GATE passed (1 cases)"] },
                { "selector": "./dev check DEV-GATE --case=isolation",
                  "checks": ["DEV-GATE passed (1 cases)"] },
                { "checks": ["SPEC passed (174 cases)"] },
            ],
        });
        let found = evidence_in(&accept, "accept.json");
        assert_eq!(found.len(), 5, "found={found:#?}");
        let full: Vec<bool> = found.iter().map(|e| e.full).collect();
        assert_eq!(
            full,
            [true, true, false, false, false],
            "rule: a `--case` selection, in either spelling, is no full run, and neither is \
             one whose selector was not recorded"
        );
        assert_eq!(
            (found[1].check.as_str(), found[1].outcome.as_str()),
            ("XTASK", "failed")
        );
        let gate = serde_json::json!({
            "schema": crate::gate::RECEIPT_SCHEMA, "tree": { "id": OTHER },
            "finished_at": "2026-10-03T21:05:00Z",
            "steps": [{ "id": "LINT-ALLOWS", "status": "passed", "cases": 52 }],
        });
        let found = evidence_in(&gate, "gate.json");
        assert_eq!(
            found,
            [Evidence {
                check: "LINT-ALLOWS".to_owned(),
                outcome: "passed".to_owned(),
                cases: Some(52),
                tree: OTHER.to_owned(),
                at: "2026-10-03T21:05:00Z".to_owned(),
                receipt: "gate.json".to_owned(),
                source: Source::Gate,
                full: true,
            }]
        );
        let foreign = serde_json::json!({ "schema": "keyval.dev.commit/v1", "steps": [] });
        assert!(
            evidence_in(&foreign, "commit.json").is_empty(),
            "only accept and gate receipts carry check results"
        );
    }

    #[test]
    fn a_requirement_passes_only_when_active_and_every_covering_check_passed_on_the_tree() {
        let report = build(&inputs());
        let o6 = find(&report, "O-6");
        assert_eq!(
            o6.passed,
            Some(true),
            "rule: active, and the newest receipt on the tree of each covering check passed \
             (§10.13), even though an older one failed and another tree failed; {o6:#?}"
        );
        let o8 = find(&report, "O-8");
        assert_eq!(
            o8.passed,
            Some(false),
            "rule: a pass on another tree does not count; {o8:#?}"
        );
        assert_eq!(
            o8.checks[0].latest.as_ref().map(|e| e.tree.as_str()),
            Some(OTHER),
            "the latest receipt on any tree is still shown"
        );
        assert_eq!(
            find(&report, "F-1").passed,
            Some(false),
            "rule: a planned requirement is never passed"
        );
        assert_eq!(
            find(&report, "C-7").passed,
            Some(false),
            "rule: no covering check, no pass (P10)"
        );
        let mut untreed = inputs();
        untreed.tree = None;
        assert!(
            build(&untreed)
                .requirements
                .iter()
                .all(|r| r.passed.is_none()),
            "no tree, no judgement"
        );
        assert_eq!(report.summary.passed, 1);
    }

    #[test]
    fn derived_statuses_and_coverage_gaps_are_reported() {
        let report = build(&inputs());
        assert_eq!(
            find(&report, "O-8").derived,
            "planned",
            "W49 lists O-8 and is open"
        );
        assert_eq!(
            find(&report, "F-1").derived,
            "active",
            "W03, its only listing item, is closed"
        );
        assert_eq!(
            report.summary.mismatched,
            ["O-8", "F-1", "C-7"],
            "rule: every declared status that differs from the derived one is listed"
        );
        assert_eq!(report.summary.uncovered, Vec::<String>::new());
        let mut no_check = inputs();
        no_check.checks.retain(|c| c.id != "DEV-DOCTOR");
        assert_eq!(
            build(&no_check).summary.uncovered,
            ["O-8"],
            "rule: an active MUST needs an implemented check (cond. 2)"
        );
    }

    #[test]
    fn the_markdown_lists_every_requirement_its_checks_and_the_registry_verdict() {
        let md = render_markdown(&build(&inputs()));
        for needle in [
            "### O-6 — MUST — active — passed",
            "### O-8 — MUST — active (derives planned) — not passed",
            "| DEV-SCHEMAS | test_group | invariant | T0 | implemented | 3 | passed, 3 cases on `111111111111` at 2026-10-03T21:00:00Z",
            "Registry read from: the judged tree.",
            "| XTASK | test_group | invariant | T0 | implemented | 3 |",
            "| yes |",
            "- Listed by: W01 (closed), W49 (open)",
            "No covering check.",
            "**Violations:** none",
            "- condition 6 (reported until W32): put has no conformance test",
        ] {
            assert!(
                md.contains(needle),
                "rule: the report shows {needle:?}; md=\n{md}"
            );
        }
        assert!(
            md.contains("O-6 \\| title"),
            "rule: a `|` in a title is escaped in tables and text"
        );
    }

    #[test]
    fn evidence_below_min_cases_or_from_a_case_run_never_passes() {
        let mut short = inputs();
        short.checks[0].min_cases = 4;
        let report = build(&short);
        let o6 = find(&report, "O-6");
        assert_eq!(
            o6.passed,
            Some(false),
            "rule: 3 cases are below DEV-SCHEMAS's min_cases 4, so the family did not pass \
             (P10); {o6:#?}"
        );
        assert!(render_markdown(&report).contains("no: 3 < min_cases 4"));

        let mut partial = inputs();
        let mut case_run = evidence("XTASK", "passed", TREE, "2026-10-03T23:00:00Z");
        case_run.full = false;
        partial.evidence.push(case_run);
        partial.evidence.push(evidence(
            "DEV-SCHEMAS",
            "failed",
            TREE,
            "2026-10-03T21:30:00Z",
        ));
        let report = build(&partial);
        let o6 = find(&report, "O-6");
        assert_eq!(
            o6.checks[0].on_tree.as_ref().map(|e| e.outcome.as_str()),
            Some("failed"),
            "the newest full run decides"
        );
        assert_eq!(
            o6.checks[1].on_tree.as_ref().map(|e| e.at.as_str()),
            Some("2026-10-03T20:30:00Z"),
            "rule: a newer `--case` run never stands in for the family (§10.13); {o6:#?}"
        );
        assert_eq!(o6.checks[1].latest.as_ref().map(|e| e.full), Some(false));
        assert_eq!(o6.passed, Some(false));
    }

    #[test]
    fn the_registry_is_read_from_the_judged_tree_not_the_working_files() {
        let requirements = "[[requirement]]\nid = \"R-1\"\nlevel = \"MUST\"\n\
                            status = \"active\"\ntitle = \"A requirement\"\n";
        let check = |id: &str, covers: &str| {
            format!(
                "[[check]]\nid = \"{id}\"\nkind = \"static\"\ncovers = [{covers}]\n\
                 class = \"invariant\"\ntier = \"T0\"\nstatus = \"implemented\"\nmin_cases = 1\n\n"
            )
        };
        let committed = format!("{}{}", check("CHK-A", "\"R-1\""), check("CHK-B", "\"R-1\""));
        let git = crate::git::fixture::repo(
            "traceability-tree",
            &[
                ("registry/requirements.toml", requirements),
                ("registry/checks.toml", &committed),
                (
                    "registry/workgraph.toml",
                    "[[item]]\nid = \"W01\"\nreqs = [\"R-1\"]\nbead = \"br-w01\"\n",
                ),
                (
                    ".beads/issues.jsonl",
                    "{\"id\":\"br-w01\",\"status\":\"closed\",\"labels\":[\"plan:W01\"]}\n",
                ),
            ],
        );
        let judged = git.rev_parse("HEAD^{tree}").unwrap();
        // An uncommitted edit stops CHK-B covering R-1; at the judged tree it still does.
        let edited = format!("{}{}", check("CHK-A", "\"R-1\""), check("CHK-B", ""));
        crate::git::fixture::write(git.root(), "registry/checks.toml", &edited);
        let artifacts = scratch_dir("traceability-tree-artifacts");
        fs::create_dir_all(artifacts.join("receipts")).unwrap();
        let receipt = serde_json::json!({
            "schema": crate::accept::SCHEMA, "tree": judged, "finished_at": "2026-10-04T00:00:00Z",
            "selectors": [{ "selector": "./dev check CHK-A CHK-B",
                            "checks": ["CHK-A passed (1 cases)", "CHK-B failed (1 cases)"] }],
        });
        fs::write(artifacts.join("receipts/accept.json"), receipt.to_string()).unwrap();
        let layout = Layout {
            work: scratch_dir("traceability-tree-work"),
            artifacts: artifacts.clone(),
            out: scratch_dir("traceability-tree-out").join("traceability.md"),
        };
        let report = run(&git, &layout, Some(judged.clone())).unwrap();
        assert_eq!(report.inputs, InputSource::Tree);
        let r1 = find(&report, "R-1");
        assert_eq!(
            (r1.passed, r1.checks.len()),
            (Some(false), 2),
            "rule: at the judged tree CHK-B covers R-1 and failed there, whatever the working \
             files say (registry.rs: read the registry from committed trees); {r1:#?}"
        );
        let working = build(
            &load(
                git.root(),
                &artifacts,
                Some(judged),
                InputSource::WorkingTree,
            )
            .unwrap(),
        );
        assert_eq!(
            find(&working, "R-1").passed,
            Some(true),
            "the working files alone would have reported it passed: the defect this guards"
        );
    }

    #[test]
    fn the_report_renders_the_real_registry() {
        let out = scratch_dir("traceability-real").join("reports/traceability.md");
        let layout = Layout {
            work: scratch_dir("traceability-real-work"),
            artifacts: scratch_dir("traceability-no-receipts"),
            out: out.clone(),
        };
        // HEAD's tree where git works, the working files in a git-isolated candidate.
        let report = run(&Git::new(&workspace_root()), &layout, None).unwrap();
        assert!(report.ok, "report={report:#?}");
        assert!(
            report.summary.requirements >= 60,
            "rule: every §6 requirement is traced; requirements={}",
            report.summary.requirements
        );
        let md = fs::read_to_string(&out).unwrap();
        for r in &report.requirements {
            assert!(
                md.contains(&format!("### {} — ", r.id)),
                "rule: {} has a section",
                r.id
            );
        }
        assert!(
            report.requirements.iter().all(|r| r.passed != Some(true)),
            "rule: with no receipts, nothing is passed (P10)"
        );
        assert_eq!(
            report.inputs == InputSource::Tree,
            report.tree.is_some(),
            "rule: a known tree is read from the tree, an unknown one from the working files"
        );
    }
}
