//! The T0 registry gate (PLAN.md §10.13): check REGISTRY judges the verification registry
//! (`registry/*.toml`) against the plan, the code, the API spec, the tests and the work
//! graph. Every condition fails the check except 5 and 6, which are *reported* (listed,
//! never failing) until W32 closes. W32's closure, judged exactly as the activation hook
//! judges an item closed ([`registry::closed_items`]), is the single switch that makes them
//! fail (§10.13, D-49). Malformed metadata always fails, whatever a row's status: staging
//! never exempts it.
//!
//! 1. **Plan and registry.** `requirements.toml` lists PLAN.md §6's IDs and levels, in
//!    order, and each status equals its derivation from the work graph and the tracker
//!    export ([`registry::derived_status`]): a hand-edited status fails (§11.5 rule 12).
//!    A §6 row whose level cell is not MUST, SHOULD or MAY fails too; it is never skipped.
//! 2. **Coverage.** Every `active` MUST requirement is covered by an `implemented` check.
//! 3. **Registry and code.** Every `implemented` check has exactly one `// check: <ID>`
//!    marker in first-party code; every marker has a row, and a `planned` row at most one.
//! 4. **Registry and execution** (static in T0). Every `implemented` check's `run` is
//!    `./dev check <ID>` and resolves to a catalogue check, and a cargo-test selector
//!    matches at least one `#[test]`, and at least its `min_cases`, in its package's
//!    sources (a short selection would fail its run, P10). Every catalogue check has a
//!    row.
//! 5. **Spec and code** (reported until W32 closes). The spec's `ErrorCode` enum equals the
//!    domain `ErrorCode` codes, and every operation has a `// api: <operationId>` handler
//!    marker under `crates/server/`.
//! 6. **Spec and tests** (reported until W32 closes). Every operation and error code is named by
//!    a `// api-test: <name>` marker under `conformance/`.
//! 7. **Bug variants.** Every `bug_variant` row names, in `catches`, at least one catcher,
//!    and each is an existing check of any kind but `bug_variant`: §10.6 designates
//!    checkers, the registry gate (static), the conformance suite and fixture corpora.
//!    Whether it is caught is T1's business (§10.6).
//! 8. **Work graph.** `workgraph.toml` mirrors §12.2/§12.3 and is acyclic
//!    ([`workgraph::judge`]), and each item's `deps` equal its bead's `blocks` edges to
//!    other work-item beads in the tracker export; other beads' edges are free (§12.2).
//!    Every item records its bead, which alone closes it ([`registry::closed_items`]), so
//!    that bead, and no other, carries the item's `plan:<id>` label (§11.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::check::{CATALOGUE, Selector};
use crate::lint;
use crate::registry;
use crate::workgraph;

/// The check registry, relative to the root (W05).
pub const CHECKS: &str = "registry/checks.toml";

const PLAN: &str = "PLAN.md";
const TRACKER_EXPORT: &str = ".beads/issues.jsonl";
const SPEC: &str = "api/openapi.yaml";
const ERROR_CODES: &str = "crates/types/src/error_code.rs";
const SERVER: &str = "crates/server/";
const CONFORMANCE: &str = "conformance/";

const REQUIREMENTS_SCHEMA: &str = "keyval.registry.requirements/v1";
const CHECKS_SCHEMA: &str = "keyval.registry.checks/v1";
const LEVELS: &[&str] = &["MUST", "SHOULD", "MAY"];
const REQUIREMENT_STATUSES: &[&str] = &["planned", "active"];
/// The work item whose closure switches conditions 5 and 6 from reported to failing
/// (§10.13, D-49).
const ENFORCED_BY: &str = "W32";
const KINDS: &[&str] = &[
    "test_group",
    "static",
    "checker",
    "conformance",
    "golden",
    "probe",
    "bug_variant",
];
const CLASSES: &[&str] = &[
    "invariant",
    "simulation_evidenced",
    "measurement",
    "assumption",
    "proposal",
];
const TIERS: &[&str] = &["T0", "T1", "T2", "T3"];
const CHECK_STATUSES: &[&str] = &["planned", "implemented"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementsFile {
    schema: String,
    #[serde(default)]
    requirement: Vec<RequirementRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementRow {
    id: String,
    level: String,
    status: String,
    title: String,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    schema: String,
    #[serde(default)]
    check: Vec<CheckRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRow {
    id: String,
    summary: String,
    kind: String,
    covers: Vec<String>,
    #[serde(default)]
    api: Vec<String>,
    class: String,
    tier: String,
    #[serde(default)]
    run: String,
    min_cases: u32,
    status: String,
    #[serde(default)]
    catches: Vec<String>,
}

impl CheckRow {
    fn implemented(&self) -> bool {
        self.status == "implemented"
    }
}

/// What the gate needs to know about one catalogue check's executable selector.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    /// The check ID.
    pub id: &'a str,
    /// A cargo-test selector's package and name filters; `None` for a static check.
    pub cargo: Option<(&'a str, &'a [&'a str])>,
}

/// Everything the gate reads, so fixtures can drive it.
pub struct Inputs<'a> {
    /// `PLAN.md`.
    pub plan: &'a str,
    /// `registry/requirements.toml`.
    pub requirements: &'a str,
    /// `registry/checks.toml`.
    pub checks: &'a str,
    /// `registry/workgraph.toml`.
    pub workgraph: &'a str,
    /// The committed tracker export, `.beads/issues.jsonl` (empty if absent).
    pub export: &'a str,
    /// `api/openapi.yaml`.
    pub spec: &'a str,
    /// The domain error type's source, `crates/types/src/error_code.rs` (empty if absent).
    pub error_codes: &'a str,
    /// Every first-party Rust source as `(path relative to the root, text)`.
    pub sources: &'a [(String, String)],
    /// The executable catalogue (`check::CATALOGUE`).
    pub catalogue: &'a [Selection<'a>],
}

/// The gate's verdict.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Judgement {
    /// Rows, markers, edges and other items examined; zero is a failure (P10).
    pub cases: usize,
    /// Failures, each naming its condition.
    pub violations: Vec<String>,
    /// Findings of the report-mode conditions (5 and 6 until W32 closes): listed, never
    /// failing.
    pub reported: Vec<String>,
}

// check: REGISTRY
/// Judges the registry of the workspace at `root`.
pub fn check_registry(root: &Path) -> Result<Judgement, String> {
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).map_err(|err| format!("cannot read {name}: {err}"))
    };
    let optional = |name: &str| match fs::read_to_string(root.join(name)) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("cannot read {name}: {err}")),
    };
    let mut sources = Vec::new();
    for path in lint::first_party_sources(root)? {
        let text = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let relative = path.strip_prefix(root).unwrap_or(&path);
        sources.push((relative.to_string_lossy().into_owned(), text));
    }
    let catalogue: Vec<Selection<'_>> = CATALOGUE
        .iter()
        .map(|def| Selection {
            id: def.id,
            cargo: match def.selector {
                Selector::CargoTest { package, filters } => Some((package, filters)),
                Selector::Static(_) => None,
            },
        })
        .collect();
    judge(&Inputs {
        plan: &read(PLAN)?,
        requirements: &read(registry::REQUIREMENTS)?,
        checks: &read(CHECKS)?,
        workgraph: &read(registry::WORKGRAPH)?,
        export: &optional(TRACKER_EXPORT)?,
        spec: &read(SPEC)?,
        error_codes: &optional(ERROR_CODES)?,
        sources: &sources,
        catalogue: &catalogue,
    })
}

/// Every registry gate condition on the given inputs. Unparseable registry files are an
/// error, not a judgement.
pub fn judge(inputs: &Inputs<'_>) -> Result<Judgement, String> {
    let requirements: RequirementsFile = toml::from_str(inputs.requirements)
        .map_err(|err| format!("invalid {}: {}", registry::REQUIREMENTS, err.message()))?;
    let checks: ChecksFile = toml::from_str(inputs.checks)
        .map_err(|err| format!("invalid {CHECKS}: {}", err.message()))?;
    let items = workgraph::parse(inputs.workgraph)?;
    let spec = Spec::parse(inputs.spec);
    let listed: Vec<registry::Item> = items
        .iter()
        .map(|item| registry::Item {
            id: item.id.clone(),
            reqs: item.reqs.clone(),
            accept: item.accept.clone(),
            bead: item.bead.clone(),
        })
        .collect();
    // The activation hook's own derivation, for both condition 1 and the 5/6 switch.
    let closed = registry::closed_items(&listed, inputs.export);
    let stage = if closed.contains(ENFORCED_BY) {
        Stage::Enforced
    } else {
        Stage::Reported
    };
    let mut verdict = Judgement::default();
    well_formed(&requirements, &checks, inputs.plan, &spec, &mut verdict);
    plan_and_statuses(&requirements, &listed, &closed, inputs.plan, &mut verdict);
    coverage(&requirements, &checks, &mut verdict);
    markers(&checks, inputs.sources, &mut verdict);
    execution(&checks, inputs, &mut verdict);
    spec_and_code(&spec, inputs, stage, &mut verdict);
    spec_and_tests(&spec, inputs.sources, stage, &mut verdict);
    bug_variants(&checks, &mut verdict);
    work_graph(&items, inputs, &mut verdict)?;
    Ok(verdict)
}

/// The spec's operations and error codes.
struct Spec {
    operations: Vec<String>,
    codes: Vec<String>,
}

impl Spec {
    fn parse(spec: &str) -> Self {
        let operations = spec
            .lines()
            .filter_map(|line| line.trim().strip_prefix("operationId:"))
            .map(|id| id.trim().to_owned())
            .collect();
        let mut codes = Vec::new();
        let mut in_enum = false;
        for line in spec
            .lines()
            .skip_while(|line| *line != "    ErrorCode:")
            .skip(1)
        {
            if line == "      enum:" {
                in_enum = true;
            } else if in_enum {
                match line.strip_prefix("        - ") {
                    Some(code) => codes.push(code.trim().to_owned()),
                    None => break,
                }
            } else if !line.starts_with("      ") {
                break;
            }
        }
        Self { operations, codes }
    }
}

/// IDs, vocabularies and required fields of every row, planned rows included.
fn well_formed(
    requirements: &RequirementsFile,
    checks: &ChecksFile,
    plan: &str,
    spec: &Spec,
    verdict: &mut Judgement,
) {
    let mut fail = |message: String| verdict.violations.push(format!("malformed: {message}"));
    if requirements.schema != REQUIREMENTS_SCHEMA {
        fail(format!(
            "{} has schema {:?}, expected {REQUIREMENTS_SCHEMA:?}",
            registry::REQUIREMENTS,
            requirements.schema
        ));
    }
    let mut seen = BTreeSet::new();
    for r in &requirements.requirement {
        if !registry::requirement_id(&r.id) || !seen.insert(r.id.as_str()) {
            fail(format!(
                "requirement ID {:?} is malformed or repeated",
                r.id
            ));
        }
        if !LEVELS.contains(&r.level.as_str()) || r.title.trim().is_empty() {
            fail(format!(
                "requirement {} needs a MUST/SHOULD/MAY level and a title",
                r.id
            ));
        }
        if r.level != "MUST" && r.notes.as_deref().is_none_or(|n| n.trim().is_empty()) {
            fail(format!(
                "{} requirement {} records no disposition note (§13)",
                r.level, r.id
            ));
        }
        if !REQUIREMENT_STATUSES.contains(&r.status.as_str()) {
            fail(format!(
                "requirement {} has status {:?}; only planned or active",
                r.id, r.status
            ));
        }
    }
    if checks.schema != CHECKS_SCHEMA {
        fail(format!(
            "{CHECKS} has schema {:?}, expected {CHECKS_SCHEMA:?}",
            checks.schema
        ));
    }
    let goals_and_principles = goals_and_principles(plan);
    let api: BTreeSet<&str> = spec
        .operations
        .iter()
        .chain(&spec.codes)
        .map(String::as_str)
        .collect();
    let mut ids = BTreeSet::new();
    for c in &checks.check {
        if !check_id(&c.id) || !ids.insert(c.id.as_str()) {
            fail(format!("check ID {:?} is malformed or repeated", c.id));
        }
        let vocabulary = [
            ("kind", c.kind.as_str(), KINDS),
            ("class", c.class.as_str(), CLASSES),
            ("tier", c.tier.as_str(), TIERS),
            ("status", c.status.as_str(), CHECK_STATUSES),
        ];
        for (field, value, allowed) in vocabulary {
            if !allowed.contains(&value) {
                fail(format!(
                    "check {} has {field} {value:?}; one of {allowed:?}",
                    c.id
                ));
            }
        }
        if c.summary.trim().is_empty() || c.min_cases == 0 || c.covers.is_empty() {
            fail(format!(
                "check {} needs a summary, a non-zero min_cases and covered IDs",
                c.id
            ));
        }
        let unknown: Vec<&String> = c
            .covers
            .iter()
            .filter(|id| {
                !requirements.requirement.iter().any(|r| &r.id == *id)
                    && !goals_and_principles.contains(id.as_str())
            })
            .collect();
        if !unknown.is_empty() {
            fail(format!("check {} covers unknown IDs {unknown:?}", c.id));
        }
        let off_spec: Vec<&String> = c.api.iter().filter(|a| !api.contains(a.as_str())).collect();
        if !off_spec.is_empty() {
            fail(format!(
                "check {} names {off_spec:?}, which api/openapi.yaml lacks",
                c.id
            ));
        }
        if c.kind != "bug_variant" && !c.catches.is_empty() {
            fail(format!(
                "check {} is not a bug variant but names catches",
                c.id
            ));
        }
    }
    verdict.cases += requirements.requirement.len() + checks.check.len();
}

/// `DEV-CHECK`, `K-5`, `T0`: uppercase alphanumeric words joined by single hyphens.
fn check_id(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_uppercase())
        && id.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
}

/// The goal and principle IDs of PLAN.md (G1… in §2.1, P1… and CD-1… in §5): the IDs a
/// check may cover besides §6 requirements. Each heads a row of one of those tables.
fn goals_and_principles(plan: &str) -> BTreeSet<String> {
    plan.lines()
        .filter_map(|line| line.strip_prefix("| ")?.split(" | ").next())
        .map(|cell| cell.trim_matches('*').to_owned())
        .filter(|id| {
            let number = id
                .strip_prefix("CD-")
                .or_else(|| id.strip_prefix('P'))
                .or_else(|| id.strip_prefix('G'));
            number.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect()
}

/// Condition 1: §6 rows, and statuses derived rather than typed.
fn plan_and_statuses(
    requirements: &RequirementsFile,
    listed: &[registry::Item],
    closed: &BTreeSet<String>,
    plan_text: &str,
    verdict: &mut Judgement,
) {
    let plan = registry::plan_requirements(plan_text);
    for (id, level) in plan
        .iter()
        .filter(|(_, level)| !LEVELS.contains(&level.as_str()))
    {
        verdict.violations.push(format!(
            "condition 1: PLAN.md §6 row {id} has level {level:?}, not one of {LEVELS:?}; a \
             requirement row is never skipped, so fix the plan's row"
        ));
    }
    let rows: Vec<(String, String)> = requirements
        .requirement
        .iter()
        .map(|r| (r.id.clone(), r.level.clone()))
        .collect();
    if rows != plan {
        let ids = |list: &[(String, String)]| -> BTreeSet<String> {
            list.iter().map(|(id, _)| id.clone()).collect()
        };
        let (ours, theirs) = (ids(&rows), ids(&plan));
        let levels: Vec<String> = rows
            .iter()
            .filter(|row| plan.iter().any(|p| p.0 == row.0 && p.1 != row.1))
            .map(|(id, level)| format!("{id} {level}"))
            .collect();
        verdict.violations.push(format!(
            "condition 1: {} differs from PLAN.md §6 (the plan wins, §0.4): missing {:?}, \
             extra {:?}, wrong level {levels:?}, or out of order",
            registry::REQUIREMENTS,
            theirs.difference(&ours).collect::<Vec<_>>(),
            ours.difference(&theirs).collect::<Vec<_>>(),
        ));
    }
    for r in &requirements.requirement {
        let derived = registry::derived_status(listed, &r.id, closed);
        if REQUIREMENT_STATUSES.contains(&r.status.as_str()) && r.status != derived {
            verdict.violations.push(format!(
                "condition 1: requirement {} is {} but its work items derive {derived}; a \
                 status is never typed by hand (§11.5 rule 12), and a closure outside \
                 ./dev commit --close applies the activation itself",
                r.id, r.status
            ));
        }
    }
    verdict.cases += plan.len();
}

/// Condition 2: every active MUST requirement has an implemented covering check.
fn coverage(requirements: &RequirementsFile, checks: &ChecksFile, verdict: &mut Judgement) {
    for r in requirements
        .requirement
        .iter()
        .filter(|r| r.level == "MUST" && r.status == "active")
    {
        verdict.cases += 1;
        if !checks
            .check
            .iter()
            .any(|c| c.implemented() && c.covers.contains(&r.id))
        {
            verdict.violations.push(format!(
                "condition 2: active MUST requirement {} has no implemented covering check",
                r.id
            ));
        }
    }
}

/// The `// check: <ID>` markers in `sources`, as ID → locations.
fn check_markers(sources: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in sources {
        for (number, line) in text.lines().enumerate() {
            if let Some(id) = line.trim_start().strip_prefix("// check: ") {
                found
                    .entry(id.trim().to_owned())
                    .or_default()
                    .push(format!("{path}:{}", number + 1));
            }
        }
    }
    found
}

/// Condition 3: one marker per implemented check, and no marker without a row.
fn markers(checks: &ChecksFile, sources: &[(String, String)], verdict: &mut Judgement) {
    let found = check_markers(sources);
    for c in &checks.check {
        let at = found.get(&c.id).map_or(&[][..], Vec::as_slice);
        let wrong = if c.implemented() {
            at.len() != 1
        } else {
            at.len() > 1
        };
        if wrong {
            verdict.violations.push(format!(
                "condition 3: {} check {} needs exactly one `// check: {}` marker in code, \
                 found {} {at:?}",
                c.status,
                c.id,
                c.id,
                at.len()
            ));
        }
    }
    for (id, at) in &found {
        if !checks.check.iter().any(|c| &c.id == id) {
            verdict.violations.push(format!(
                "condition 3: marker `// check: {id}` at {at:?} has no row in {CHECKS}"
            ));
        }
    }
    verdict.cases += checks.check.len() + found.len();
}

/// Condition 4: implemented selectors resolve and select cases; catalogue checks have rows.
fn execution(checks: &ChecksFile, inputs: &Inputs<'_>, verdict: &mut Judgement) {
    for c in checks.check.iter().filter(|c| c.implemented()) {
        verdict.cases += 1;
        if c.run != format!("./dev check {}", c.id) {
            verdict.violations.push(format!(
                "condition 4: implemented check {} has run {:?}, not `./dev check {}`",
                c.id, c.run, c.id
            ));
            continue;
        }
        let Some(selection) = inputs.catalogue.iter().find(|s| s.id == c.id) else {
            verdict.violations.push(format!(
                "condition 4: `./dev check {}` does not resolve: no catalogue check has \
                 that ID",
                c.id
            ));
            continue;
        };
        if let Some((package, filters)) = selection.cargo {
            let names = package_tests(package, inputs.sources);
            let selected = names
                .iter()
                .filter(|name| filters.is_empty() || filters.iter().any(|f| name.contains(f)))
                .count();
            if selected == 0 {
                verdict.violations.push(format!(
                    "condition 4: check {} selects no #[test] case of package {package} \
                     (filters {filters:?}; {} test(s) found in its sources)",
                    c.id,
                    names.len()
                ));
            } else if usize::try_from(c.min_cases).is_ok_and(|min| selected < min) {
                verdict.violations.push(format!(
                    "condition 4: check {} selects {selected} #[test] case(s) of package \
                     {package} (filters {filters:?}), fewer than its min_cases {}, so its run \
                     would fail (P10)",
                    c.id, c.min_cases
                ));
            }
        }
    }
    for selection in inputs.catalogue {
        if !checks.check.iter().any(|c| c.id == selection.id) {
            verdict.violations.push(format!(
                "condition 4: catalogue check {} has no row in {CHECKS} (§11.5 rule 12)",
                selection.id
            ));
        }
    }
}

/// The libtest names of the `#[test]` functions in `package`'s sources.
fn package_tests(package: &str, sources: &[(String, String)]) -> Vec<String> {
    // `xtask` and the HTTP-only `conformance` package sit outside `crates/` (Appendix A).
    let dir = if package == "xtask" || package == "conformance" {
        package.to_owned()
    } else {
        format!("crates/{package}")
    };
    let (src, tests) = (format!("{dir}/src/"), format!("{dir}/tests/"));
    let mut names = Vec::new();
    for (path, text) in sources {
        if let Some(relative) = path.strip_prefix(&src) {
            names.extend(test_names(&module_path(relative), text));
        } else if path.starts_with(&tests) {
            // Each integration test file is its own crate: its names start at its root.
            names.extend(test_names(&[], text));
        }
    }
    names
}

/// The module path of a source file relative to `src/`: `lib.rs` and `main.rs` are the
/// crate root, `a/mod.rs` and `a.rs` are `a`, `a/b.rs` is `a::b`.
fn module_path(relative: &str) -> Vec<String> {
    let stem = relative.strip_suffix(".rs").unwrap_or(relative);
    let mut parts: Vec<String> = stem.split('/').map(str::to_owned).collect();
    if parts
        .last()
        .is_some_and(|last| matches!(last.as_str(), "mod" | "lib" | "main"))
    {
        parts.pop();
    }
    parts
}

/// The libtest names of the `#[test]` functions in one file whose module path is `base`:
/// inline `mod` blocks extend the path. Comments and string literals are masked first, so
/// test-like text inside them is ignored.
fn test_names(base: &[String], text: &str) -> Vec<String> {
    let masked = lint::mask_comments_and_strings(text);
    let mut stack: Vec<(String, usize)> = Vec::new();
    let (mut depth, mut pending) = (0_usize, false);
    let mut names = Vec::new();
    for line in masked.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[test]") {
            pending = true;
        }
        if pending && let Some(name) = fn_name(trimmed) {
            let path = base
                .iter()
                .cloned()
                .chain(stack.iter().map(|(module, _)| module.clone()))
                .chain(std::iter::once(name));
            names.push(path.collect::<Vec<_>>().join("::"));
            pending = false;
        }
        let mut opening = inline_module(trimmed);
        for c in line.chars() {
            if c == '{' {
                depth += 1;
                if let Some(module) = opening.take() {
                    stack.push((module, depth));
                }
            } else if c == '}' {
                if stack.last().is_some_and(|(_, at)| *at == depth) {
                    stack.pop();
                }
                depth = depth.saturating_sub(1);
            }
        }
    }
    names
}

/// The name of an inline module opened on this line (`mod tests {`), if any.
fn inline_module(trimmed: &str) -> Option<String> {
    let rest = ["pub(crate) ", "pub(super) ", "pub "]
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
        .unwrap_or(trimmed);
    let rest = rest.strip_prefix("mod ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty() && rest.contains('{')).then_some(name)
}

/// The function a line declares (`fn name(` or `fn name<`), if any.
fn fn_name(trimmed: &str) -> Option<String> {
    let (_, after) = trimmed.split_once("fn ")?;
    let name: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The names marked `// <prefix> <name>` in the sources under `dir`.
fn named_markers(sources: &[(String, String)], dir: &str, prefix: &str) -> BTreeSet<String> {
    sources
        .iter()
        .filter(|(path, _)| path.starts_with(dir))
        .flat_map(|(_, text)| text.lines())
        .filter_map(|line| line.trim_start().strip_prefix(prefix))
        .map(|name| name.trim().to_owned())
        .collect()
}

/// The codes the domain `ErrorCode::as_str` spells, in order.
fn domain_codes(error_codes: &str) -> Vec<String> {
    error_codes
        .lines()
        .skip_while(|line| !line.contains("fn as_str"))
        .skip(1)
        .take_while(|line| !line.contains("fn "))
        .filter_map(|line| {
            let (_, after) = line.split_once("=> \"")?;
            after.split('"').next().map(str::to_owned)
        })
        .collect()
}

/// Whether conditions 5 and 6 fail yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// W32 is open: findings are listed, never failing.
    Reported,
    /// W32 has closed: findings are violations.
    Enforced,
}

impl Stage {
    /// Records a finding of `condition` (5 or 6) as this stage dictates.
    fn record(self, condition: u8, finding: &str, verdict: &mut Judgement) {
        match self {
            Self::Reported => verdict.reported.push(format!(
                "condition {condition} (reported until {ENFORCED_BY}): {finding}"
            )),
            Self::Enforced => verdict
                .violations
                .push(format!("condition {condition}: {finding}")),
        }
    }
}

/// Condition 5, reported until W32 closes: spec codes against domain codes, and an HTTP
/// handler per operation.
fn spec_and_code(spec: &Spec, inputs: &Inputs<'_>, stage: Stage, verdict: &mut Judgement) {
    let domain = domain_codes(inputs.error_codes);
    if domain != spec.codes {
        stage.record(
            5,
            &format!(
                "the spec's ErrorCode enum {:?} differs from the domain ErrorCode {domain:?}",
                spec.codes
            ),
            verdict,
        );
    }
    let handled = named_markers(inputs.sources, SERVER, "// api: ");
    let unhandled: Vec<&String> = spec
        .operations
        .iter()
        .filter(|op| !handled.contains(*op))
        .collect();
    if !unhandled.is_empty() {
        stage.record(
            5,
            &format!(
                "no HTTP handler (`// api: <operationId>` under {SERVER}) for {} \
                 operation(s): {unhandled:?}",
                unhandled.len()
            ),
            verdict,
        );
    }
    verdict.cases += spec.codes.len() + spec.operations.len();
}

/// Condition 6, reported until W32 closes: a conformance test per operation and error
/// code.
fn spec_and_tests(
    spec: &Spec,
    sources: &[(String, String)],
    stage: Stage,
    verdict: &mut Judgement,
) {
    let tested = named_markers(sources, CONFORMANCE, "// api-test: ");
    for (what, names) in [("operation", &spec.operations), ("error code", &spec.codes)] {
        let untested: Vec<&String> = names.iter().filter(|n| !tested.contains(*n)).collect();
        if !untested.is_empty() {
            stage.record(
                6,
                &format!(
                    "no conformance test (`// api-test: <name>` under {CONFORMANCE}) \
                     exercises {} {what}(s): {untested:?}",
                    untested.len()
                ),
                verdict,
            );
        }
        verdict.cases += names.len();
    }
}

/// Condition 7: every bug variant names existing checks that should catch it. A catcher
/// may be of any kind that runs against the code (§10.6 designates static and conformance
/// catchers too), never another bug variant.
fn bug_variants(checks: &ChecksFile, verdict: &mut Judgement) {
    for c in checks.check.iter().filter(|c| c.kind == "bug_variant") {
        verdict.cases += 1;
        let catchers = c.catches.iter().filter(|id| {
            checks
                .check
                .iter()
                .any(|other| &other.id == *id && other.kind != "bug_variant")
        });
        if c.catches.is_empty() || catchers.count() != c.catches.len() {
            verdict.violations.push(format!(
                "condition 7: bug variant {} needs a designated catcher: catches {:?} must \
                 name existing check rows that are not bug variants",
                c.id, c.catches
            ));
        }
    }
}

/// Condition 8: the work graph mirrors the plan, and the tracker's work-item edges mirror
/// the work graph.
fn work_graph(
    items: &[workgraph::WorkItem],
    inputs: &Inputs<'_>,
    verdict: &mut Judgement,
) -> Result<(), String> {
    let drift = workgraph::judge(inputs.plan, inputs.workgraph)?;
    verdict.cases += drift.cases;
    verdict.violations.extend(
        drift
            .violations
            .into_iter()
            .map(|v| format!("condition 8: {v}")),
    );
    let beads: BTreeMap<String, serde_json::Value> = inputs
        .export
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|bead| Some((bead["id"].as_str()?.to_owned(), bead)))
        .collect();
    let item_of: BTreeMap<&str, &str> = items
        .iter()
        .filter(|item| !item.bead.is_empty())
        .map(|item| (item.bead.as_str(), item.id.as_str()))
        .collect();
    // The beads carrying each `plan:<id>` label.
    let mut labelled: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, bead) in &beads {
        let labels = bead["labels"].as_array().map_or(&[][..], Vec::as_slice);
        for item in labels
            .iter()
            .filter_map(|label| label.as_str()?.strip_prefix("plan:"))
        {
            labelled.entry(item).or_default().push(id);
        }
    }
    for item in items {
        verdict.cases += 1;
        if item.bead.is_empty() {
            verdict.violations.push(format!(
                "condition 8: {} records no bead; an item is closed only by its recorded \
                 bead, so every item names its one bead (§11.4)",
                item.id
            ));
            continue;
        }
        let carriers = labelled
            .get(item.id.as_str())
            .map_or(&[][..], Vec::as_slice);
        if carriers != [item.bead.as_str()] {
            verdict.violations.push(format!(
                "condition 8: plan:{} is on beads {carriers:?}; it belongs on {}, the item's \
                 recorded bead, and on no other (§11.4)",
                item.id, item.bead
            ));
        }
        let Some(bead) = beads.get(&item.bead) else {
            verdict.violations.push(format!(
                "condition 8: {} records bead {}, which the tracker export lacks",
                item.id, item.bead
            ));
            continue;
        };
        let tracked: BTreeSet<&str> = bead["dependencies"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter(|dep| dep["type"] == "blocks")
            .filter_map(|dep| item_of.get(dep["depends_on_id"].as_str()?).copied())
            .collect();
        let planned: BTreeSet<&str> = item.deps.iter().map(String::as_str).collect();
        if tracked != planned {
            verdict.violations.push(format!(
                "condition 8: {}'s bead {} blocks on work items {tracked:?}, but \
                 workgraph.toml says {planned:?}",
                item.id, item.bead
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // check: REGISTRY-FIXTURES
    use super::*;
    use crate::workgraph::tests::{PLAN_TEXT, workgraph_text};

    /// §5 and §6 for the fixtures, in PLAN.md's table layout.
    const PLAN_HEAD: &str = "\
## 5. Principles

### 5.1 Project principles

| ID | Principle |
|---|---|
| **P4** | Determinism |

### 5.2 Code design principles

| ID | Principle |
|---|---|
| **CD-1** | Hexagonal |

## 6. Requirements

| ID | Level | Requirement |
|---|---|---|
| F-1 | MUST | PUT |
| F-2 | SHOULD | GET |

## 7. Architecture
";

    const REQUIREMENTS_TEXT: &str = "\
schema = \"keyval.registry.requirements/v1\"

[[requirement]]
id = \"F-1\"
level = \"MUST\"
status = \"planned\"
title = \"PUT\"

[[requirement]]
id = \"F-2\"
level = \"SHOULD\"
status = \"planned\"
title = \"GET\"
notes = \"disposition pending\"
";

    const SPEC_TEXT: &str = "\
paths:
  /v1/keys/{key}:
    get:
      operationId: getKey
components:
  schemas:
    ErrorCode:
      type: string
      enum:
        - not_found
        - busy
";

    const ERROR_CODES_TEXT: &str = "\
impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => \"not_found\",
            Self::Busy => \"busy\",
        }
    }
    pub const fn other(self) -> u16 { 0 }
}
";

    /// One checks.toml row.
    fn row(id: &str, kind: &str, covers: &str, status: &str, extra: &str) -> String {
        let run = if status == "implemented" {
            format!("./dev check {id}")
        } else {
            String::new()
        };
        format!(
            "\n[[check]]\nid = \"{id}\"\nsummary = \"{id} works\"\nkind = \"{kind}\"\n\
             covers = [{covers}]\nclass = \"invariant\"\ntier = \"T0\"\nrun = \"{run}\"\n\
             min_cases = 1\nstatus = \"{status}\"\n{extra}"
        )
    }

    fn checks_text() -> String {
        [
            "schema = \"keyval.registry.checks/v1\"\n".to_owned(),
            row("A-CHECK", "test_group", "\"F-1\"", "implemented", ""),
            row("B-CHECK", "static", "\"P4\"", "implemented", ""),
            row("C-CHECK", "test_group", "\"F-2\"", "planned", ""),
        ]
        .concat()
    }

    /// A tracker export line for a bead with `labels`.
    fn labelled_bead(id: &str, labels: &[&str], status: &str, blocks: &[&str]) -> String {
        let deps: Vec<serde_json::Value> = blocks
            .iter()
            .map(|on| serde_json::json!({"depends_on_id": on, "type": "blocks"}))
            .collect();
        format!(
            "{}\n",
            serde_json::json!({"id": id, "status": status, "labels": labels, "dependencies": deps})
        )
    }

    /// A tracker export line for a bead that is no work item's (a subtask).
    fn bead(id: &str, status: &str, blocks: &[&str]) -> String {
        labelled_bead(id, &[], status, blocks)
    }

    /// A tracker export line for `item`'s recorded bead, labelled `plan:<item>`.
    fn item_bead(id: &str, item: &str, status: &str, blocks: &[&str]) -> String {
        labelled_bead(id, &[&format!("plan:{item}")], status, blocks)
    }

    fn export_text(w03: &str) -> String {
        [
            item_bead("b1", "W01", "closed", &[]),
            item_bead("b2", "W02", "closed", &["b1"]),
            item_bead("b3", "W03", w03, &["b1", "b2"]),
        ]
        .concat()
    }

    /// The fixtures' first-party sources: one test for A-CHECK and both markers.
    fn sources() -> Vec<(String, String)> {
        let marker = |id: &str| format!("    // check: {id}\n");
        vec![
            (
                "xtask/src/alpha.rs".to_owned(),
                format!(
                    "#[cfg(test)]\nmod tests {{\n{}    #[test]\n    fn works() {{}}\n}}\n",
                    marker("A-CHECK")
                ),
            ),
            ("xtask/src/beta.rs".to_owned(), marker("B-CHECK")),
        ]
    }

    const CATALOGUE_FIXTURE: &[Selection<'static>] = &[
        Selection {
            id: "A-CHECK",
            cargo: Some(("xtask", &["alpha::"])),
        },
        Selection {
            id: "B-CHECK",
            cargo: None,
        },
    ];

    /// The fixture texts; each test changes one of them.
    struct Texts {
        plan: String,
        requirements: String,
        checks: String,
        workgraph: String,
        export: String,
        error_codes: String,
        sources: Vec<(String, String)>,
    }

    impl Texts {
        fn valid() -> Self {
            Self {
                plan: format!("{PLAN_HEAD}{PLAN_TEXT}"),
                requirements: REQUIREMENTS_TEXT.to_owned(),
                checks: checks_text(),
                workgraph: workgraph_text(["b1", "b2", "b3"]),
                export: export_text("open"),
                error_codes: ERROR_CODES_TEXT.to_owned(),
                sources: sources(),
            }
        }

        fn judge(&self) -> Judgement {
            judge(&Inputs {
                plan: &self.plan,
                requirements: &self.requirements,
                checks: &self.checks,
                workgraph: &self.workgraph,
                export: &self.export,
                spec: SPEC_TEXT,
                error_codes: &self.error_codes,
                sources: &self.sources,
                catalogue: CATALOGUE_FIXTURE,
            })
            .unwrap()
        }
    }

    /// Asserts exactly the violations naming `needle`, and no others.
    fn fails_only(texts: &Texts, needle: &str, rule: &str) {
        let verdict = texts.judge();
        assert!(
            !verdict.violations.is_empty() && verdict.violations.iter().all(|v| v.contains(needle)),
            "rule: {rule}; expected violations naming {needle:?}; verdict={verdict:#?}"
        );
    }

    #[test]
    fn a_consistent_registry_passes_and_reports_conditions_5_and_6() {
        let verdict = Texts::valid().judge();
        assert!(verdict.violations.is_empty(), "verdict={verdict:#?}");
        assert!(verdict.cases > 10, "verdict={verdict:#?}");
        assert!(
            verdict.reported.iter().any(|r| r.contains("condition 5"))
                && verdict.reported.iter().any(|r| r.contains("condition 6")),
            "rule: without a server and conformance tests, 5 and 6 are reported, never \
             failed, until W32; verdict={verdict:#?}"
        );
    }

    #[test]
    fn condition_1_fails_on_plan_drift_and_on_a_hand_edited_status() {
        let mut texts = Texts::valid();
        texts.requirements = REQUIREMENTS_TEXT.replace("level = \"SHOULD\"", "level = \"MUST\"");
        texts.requirements = texts
            .requirements
            .replace("notes = \"disposition pending\"\n", "");
        fails_only(&texts, "condition 1", "levels equal §6 (the plan wins)");
        let mut texts = Texts::valid();
        texts.requirements =
            REQUIREMENTS_TEXT.replacen("status = \"planned\"", "status = \"active\"", 1);
        fails_only(
            &texts,
            "typed by hand",
            "F-1 is listed by W03, which is open, so it derives planned (§11.5 rule 12)",
        );
    }

    #[test]
    fn condition_2_fails_when_an_active_must_requirement_has_no_implemented_check() {
        let mut texts = Texts::valid();
        texts.export = export_text("closed");
        texts.requirements =
            REQUIREMENTS_TEXT.replace("status = \"planned\"", "status = \"active\"");
        assert!(
            texts.judge().violations.is_empty(),
            "control: F-1 is covered by A-CHECK"
        );
        texts.checks = texts
            .checks
            .replace("covers = [\"F-1\"]", "covers = [\"P4\"]");
        fails_only(
            &texts,
            "condition 2",
            "an active MUST requirement is covered",
        );
    }

    #[test]
    fn condition_3_fails_on_a_missing_a_repeated_or_an_unregistered_marker() {
        let mut texts = Texts::valid();
        texts
            .sources
            .retain(|(path, _)| path != "xtask/src/beta.rs");
        fails_only(&texts, "condition 3", "an implemented check has its marker");
        let mut texts = Texts::valid();
        texts.sources.push((
            "xtask/src/gamma.rs".to_owned(),
            "// check: B-CHECK\n".to_owned(),
        ));
        fails_only(&texts, "condition 3", "exactly one marker");
        let mut texts = Texts::valid();
        texts.sources.push((
            "xtask/src/gamma.rs".to_owned(),
            "// check: Z-CHECK\n".to_owned(),
        ));
        fails_only(&texts, "condition 3", "a marker has a row");
    }

    #[test]
    fn condition_4_fails_on_an_unresolved_an_empty_or_a_short_selector() {
        let mut texts = Texts::valid();
        texts.checks = texts
            .checks
            .replace("run = \"./dev check A-CHECK\"", "run = \"cargo test\"");
        fails_only(
            &texts,
            "condition 4",
            "run is the exact ./dev check selector",
        );
        let mut texts = Texts::valid();
        texts.sources[0].1 = texts.sources[0].1.replace("#[test]", "#[allow(dead_code)]");
        fails_only(&texts, "condition 4", "a cargo-test selector lists a case");
        let mut texts = Texts::valid();
        assert!(
            texts.checks.starts_with(
                "schema = \"keyval.registry.checks/v1\"\n\n[[check]]\nid = \"A-CHECK\""
            )
        );
        texts.checks = texts.checks.replacen("min_cases = 1", "min_cases = 2", 1);
        fails_only(
            &texts,
            "fewer than its min_cases 2",
            "a selection short of min_cases fails before it runs (P10)",
        );
        let mut texts = Texts::valid();
        texts.checks = [
            texts.checks.clone(),
            row("D-CHECK", "static", "\"P4\"", "implemented", ""),
        ]
        .concat();
        texts.sources.push((
            "xtask/src/delta.rs".to_owned(),
            "// check: D-CHECK\n".to_owned(),
        ));
        fails_only(
            &texts,
            "does not resolve",
            "the selector resolves to a catalogue check",
        );
    }

    #[test]
    fn conditions_5_and_6_are_reported_never_failed() {
        let mut texts = Texts::valid();
        texts.error_codes = ERROR_CODES_TEXT.replace("Self::Busy => \"busy\",\n", "");
        let verdict = texts.judge();
        assert!(
            verdict.violations.is_empty(),
            "rule: report mode until W32; {verdict:#?}"
        );
        assert!(
            verdict
                .reported
                .iter()
                .any(|r| r.contains("condition 5") && r.contains("differs")),
            "verdict={verdict:#?}"
        );
        let mut texts = Texts::valid();
        texts.sources.push((
            "crates/server/src/http.rs".to_owned(),
            "// api: getKey\n".to_owned(),
        ));
        texts.sources.push((
            "conformance/tests/api.rs".to_owned(),
            "// api-test: getKey\n// api-test: not_found\n// api-test: busy\n".to_owned(),
        ));
        let verdict = texts.judge();
        assert!(
            verdict.reported.is_empty() && verdict.violations.is_empty(),
            "rule: a handler and a conformance test per operation and code clear 5 and 6; \
             {verdict:#?}"
        );
    }

    /// The fixture registry plus a planned checker K-2, a conformance check CONF-1 and the
    /// bug variant B-1 with `catches`.
    fn with_bug_variant(catches: &str) -> Texts {
        let mut texts = Texts::valid();
        texts.checks = [
            texts.checks.clone(),
            row("K-2", "checker", "\"F-1\"", "planned", ""),
            row("CONF-1", "conformance", "\"F-1\"", "planned", ""),
            row(
                "B-1",
                "bug_variant",
                "\"F-1\"",
                "planned",
                &format!("catches = [{catches}]\n"),
            ),
        ]
        .concat();
        texts
    }

    /// The fixtures plus a fourth work item, W32, whose bead `b32` is `status`: the
    /// switch for conditions 5 and 6.
    fn with_w32(status: &str) -> Texts {
        let mut texts = Texts::valid();
        let w03 =
            "| **W03** | Contract | W01, W02 | F-1…F-2, §8 | api (spec only) | it validates |\n";
        let edge = "    W02 --> W03\n";
        assert!(PLAN_TEXT.contains(w03) && PLAN_TEXT.contains(edge));
        let plan = PLAN_TEXT
            .replace(
                w03,
                &format!("{w03}| **W32** | Conformance | W03 | P4 | conformance | it conforms |\n"),
            )
            .replace(edge, &format!("{edge}    W03 --> W32\n"));
        texts.plan = format!("{PLAN_HEAD}{plan}");
        texts.workgraph = format!(
            "{}\n[[item]]\nid = \"W32\"\ntitle = \"Conformance\"\ndeps = [\"W03\"]\n\
             reqs = [\"P4\"]\narea = [\"conformance\"]\nmilestone = \"m1\"\naccept = []\n\
             bead = \"b32\"\n",
            workgraph_text(["b1", "b2", "b3"])
        );
        texts.export = [
            export_text("open"),
            item_bead("b32", "W32", status, &["b3"]),
        ]
        .concat();
        texts
    }

    #[test]
    fn conditions_5_and_6_are_reported_while_w32_is_open_and_fail_once_it_closes() {
        // The same spec/code and spec/test gaps in both: no handler, no conformance test.
        let open = with_w32("open").judge();
        assert!(
            open.violations.is_empty()
                && open
                    .reported
                    .iter()
                    .any(|r| r.starts_with("condition 5 (reported"))
                && open
                    .reported
                    .iter()
                    .any(|r| r.starts_with("condition 6 (reported")),
            "rule: while W32 is open, 5 and 6 are listed, never failed (§10.13); \
             verdict={open:#?}"
        );
        let closed = with_w32("closed").judge();
        assert!(
            closed.reported.is_empty(),
            "rule: nothing is merely reported once W32 has closed; verdict={closed:#?}"
        );
        for condition in ["condition 5: ", "condition 6: "] {
            assert!(
                closed.violations.iter().any(|v| v.starts_with(condition)),
                "rule: W32's closure is the single switch to enforcing (§10.13, D-49): \
                 {condition:?} fails; verdict={closed:#?}"
            );
        }
        assert!(
            closed
                .violations
                .iter()
                .all(|v| v.starts_with("condition 5: ") || v.starts_with("condition 6: ")),
            "rule: W32's closure changes only conditions 5 and 6; verdict={closed:#?}"
        );
    }

    #[test]
    fn condition_7_fails_on_a_bug_variant_without_a_catcher() {
        for (catches, rule) in [
            ("", "a bug variant names a catcher"),
            ("\"NO-SUCH-CHECK\"", "a catcher is an existing check"),
            (
                "\"K-2\", \"NO-SUCH-CHECK\"",
                "every catcher is an existing check, not just one",
            ),
            ("\"B-1\"", "a bug variant never catches a bug variant"),
        ] {
            fails_only(&with_bug_variant(catches), "condition 7", rule);
        }
    }

    #[test]
    fn condition_7_accepts_every_catcher_kind_section_10_6_designates() {
        for (catches, designated) in [
            ("\"K-2\"", "a checker (B-10 → K-2)"),
            (
                "\"B-CHECK\"",
                "the registry gate, a static check (B-21 → §10.13)",
            ),
            (
                "\"K-2\", \"CONF-1\"",
                "a checker and the conformance suite (B-13 → K-2 / conformance)",
            ),
            (
                "\"A-CHECK\"",
                "checker unit fixtures, a test group (B-22 → fixtures and corpus)",
            ),
        ] {
            let verdict = with_bug_variant(catches).judge();
            assert!(
                verdict.violations.is_empty(),
                "rule: §10.6 designates {designated} as a catcher (br-cond7-checker-only-\
                 catchers-ur1r); verdict={verdict:#?}"
            );
        }
    }

    #[test]
    fn condition_8_fails_unless_each_item_records_the_one_bead_labelled_for_it() {
        let mut texts = Texts::valid();
        texts.workgraph = workgraph_text(["b1", "", "b3"]);
        fails_only(
            &texts,
            "condition 8",
            "every item records its bead; an empty `bead` is never skipped",
        );
        let mut texts = Texts::valid();
        texts.export = [
            item_bead("b1", "W01", "closed", &[]),
            bead("b2", "closed", &["b1"]),
            item_bead("b3", "W03", "open", &["b1", "b2"]),
        ]
        .concat();
        fails_only(
            &texts,
            "condition 8",
            "the recorded bead carries its item's plan: label",
        );
        // A discovered bead mislabelled plan:W03 closes while W03's own bead stays open.
        let mut texts = Texts::valid();
        texts.export = [export_text("open"), item_bead("x3", "W03", "closed", &[])].concat();
        fails_only(
            &texts,
            "condition 8",
            "a plan: label is on the recorded bead alone; and the mislabelled closure \
             closes nothing, so F-1 still derives planned and condition 1 holds \
             (br-item-closure-identity-unverified-xp14)",
        );
    }

    #[test]
    fn condition_1_fails_on_a_section_6_row_with_a_malformed_level() {
        let mut texts = Texts::valid();
        assert_eq!(texts.plan.matches("| F-2 | SHOULD | GET |").count(), 1);
        texts.plan = texts
            .plan
            .replace("| F-2 | SHOULD | GET |", "| F-2 | **SHOULD** | GET |");
        fails_only(
            &texts,
            "condition 1",
            "a §6 row whose level does not parse fails; it is never silently dropped",
        );
    }

    #[test]
    fn condition_8_fails_on_work_graph_drift_and_on_tracker_edges() {
        let mut texts = Texts::valid();
        texts.export = [
            item_bead("b1", "W01", "closed", &[]),
            item_bead("b2", "W02", "closed", &["b1"]),
            bead("s1", "open", &["b2", "b3"]),
            item_bead("b3", "W03", "open", &["b1", "b2", "s1"]),
        ]
        .concat();
        let verdict = texts.judge();
        assert!(
            verdict.violations.is_empty(),
            "rule: only work-item beads' edges are compared; a subtask's edges are free \
             (§12.2); {verdict:#?}"
        );
        let mut texts = Texts::valid();
        texts.export = [
            item_bead("b1", "W01", "closed", &[]),
            item_bead("b2", "W02", "closed", &["b1"]),
            item_bead("b3", "W03", "open", &["b1"]),
        ]
        .concat();
        fails_only(
            &texts,
            "condition 8",
            "work-item beads' edges mirror the work graph",
        );
        let mut texts = Texts::valid();
        assert_eq!(
            texts
                .workgraph
                .matches("area = [\"vendor\", \"spike\"]")
                .count(),
            1
        );
        texts.workgraph = texts
            .workgraph
            .replace("area = [\"vendor\", \"spike\"]", "area = [\"vendor\"]");
        fails_only(&texts, "condition 8", "workgraph.toml mirrors §12.2");
        let mut texts = Texts::valid();
        let listed = "reqs = [\"F-1\", \"F-2\", \"§8\"]";
        assert_eq!(texts.workgraph.matches(listed).count(), 1);
        texts.workgraph = texts.workgraph.replace(listed, "reqs = [\"F-1\", \"§8\"]");
        fails_only(
            &texts,
            "condition 8",
            "the plan's range F-1…F-2 expands to both IDs, so a work item listing only F-1 \
             has drifted",
        );
    }

    #[test]
    fn malformed_rows_fail_even_when_planned() {
        let broken = [
            ("id = \"C-CHECK\"", "id = \"c-check\""),
            (
                "kind = \"test_group\"\ncovers = [\"F-2\"]",
                "kind = \"unit\"\ncovers = [\"F-2\"]",
            ),
            ("covers = [\"F-2\"]", "covers = [\"F-99\"]"),
            (
                "covers = [\"F-2\"]",
                "covers = [\"F-2\"]\napi = [\"putKeyz\"]",
            ),
        ];
        for (from, to) in broken {
            let mut texts = Texts::valid();
            assert_eq!(texts.checks.matches(from).count(), 1, "fixture: {from}");
            texts.checks = texts.checks.replace(from, to);
            texts
                .sources
                .push(("xtask/src/c.rs".to_owned(), String::new()));
            fails_only(
                &texts,
                "malformed",
                "staging never exempts malformed metadata",
            );
        }
        let mut texts = Texts::valid();
        texts.requirements = REQUIREMENTS_TEXT.replace("notes = \"disposition pending\"\n", "");
        fails_only(
            &texts,
            "malformed",
            "a SHOULD requirement records its disposition (§13)",
        );
    }

    #[test]
    fn test_names_follow_files_inline_modules_and_ignore_strings() {
        let text = "\
#[cfg(test)]
mod probes {
    const S: &str = \"#[test] fn fake() {}\";
    #[test]
    fn outer() {}
    mod compile_fail {
        #[test]
        #[ignore]
        fn inner() {}
    }
    #[test]
    fn after() {}
}
";
        assert_eq!(
            test_names(&module_path("lint.rs"), text),
            [
                "lint::probes::outer",
                "lint::probes::compile_fail::inner",
                "lint::probes::after"
            ]
        );
        assert_eq!(module_path("a/mod.rs"), ["a"]);
        assert_eq!(module_path("main.rs"), Vec::<String>::new());
    }

    #[test]
    fn the_real_registry_passes_the_gate() {
        let verdict = check_registry(&crate::test_support::workspace_root()).unwrap();
        assert!(
            verdict.violations.is_empty(),
            "rule: the committed registry is consistent with the plan, the code, the spec \
             and the work graph (§10.13); violations={:#?}",
            verdict.violations
        );
        assert!(verdict.cases > 100, "cases={}", verdict.cases);
    }

    #[test]
    fn conformance_tests_are_found_outside_crates() {
        let sources = [(
            "conformance/src/poll.rs".to_owned(),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn waits() {}\n}\n".to_owned(),
        )];
        assert_eq!(
            package_tests("conformance", &sources),
            ["poll::tests::waits"],
            "rule: the conformance package lives at conformance/, not crates/conformance"
        );
    }
}
