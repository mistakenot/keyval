//! The T0 registry gate (PLAN.md §10.13): check REGISTRY judges the verification registry
//! (`registry/*.toml`) against the plan, the code, the API spec, the tests and the work
//! graph. Every condition fails the check except 5 and 6, which are *reported* (listed,
//! never failing) until W32 closes. Malformed metadata always fails, whatever a row's
//! status: staging never exempts it.
//!
//! 1. **Plan and registry.** `requirements.toml` lists PLAN.md §6's IDs and levels, in
//!    order, and each status equals its derivation from the work graph and the tracker
//!    export ([`registry::derived_status`]): a hand-edited status fails (§11.5 rule 12).
//! 2. **Coverage.** Every `active` MUST requirement is covered by an `implemented` check.
//! 3. **Registry and code.** Every `implemented` check has exactly one `// check: <ID>`
//!    marker in first-party code; every marker has a row, and a `planned` row at most one.
//! 4. **Registry and execution** (static in T0). Every `implemented` check's `run` is
//!    `./dev check <ID>` and resolves to a catalogue check, and a cargo-test selector
//!    matches at least one `#[test]` in its package's sources. Its `min_cases` is enforced
//!    where the check runs. Every catalogue check has a row.
//! 5. **Spec and code** (reported until W32). The spec's `ErrorCode` enum equals the
//!    domain `ErrorCode` codes, and every operation has a `// api: <operationId>` handler
//!    marker under `crates/server/`.
//! 6. **Spec and tests** (reported until W32). Every operation and error code is named by
//!    a `// api-test: <name>` marker under `conformance/`.
//! 7. **Bug variants.** Every `bug_variant` row names, in `catches`, at least one existing
//!    checker; whether it is caught is T1's business (§10.6).
//! 8. **Work graph.** `workgraph.toml` mirrors §12.2/§12.3 and is acyclic
//!    ([`workgraph::judge`]), and each item's `deps` equal its bead's `blocks` edges to
//!    other work-item beads in the tracker export; other beads' edges are free (§12.2).

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
const REQUIREMENT_FAMILIES: &[&str] = &["DM", "F", "C", "A", "DUR", "O", "T"];
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
    /// Findings of the report-mode conditions (5 and 6 until W32): listed, never failing.
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
    let mut verdict = Judgement::default();
    well_formed(&requirements, &checks, inputs.plan, &spec, &mut verdict);
    plan_and_statuses(&requirements, &items, inputs, &mut verdict);
    coverage(&requirements, &checks, &mut verdict);
    markers(&checks, inputs.sources, &mut verdict);
    execution(&checks, inputs, &mut verdict);
    spec_and_code(&spec, inputs, &mut verdict);
    spec_and_tests(&spec, inputs.sources, &mut verdict);
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
        if !requirement_id(&r.id) || !seen.insert(r.id.as_str()) {
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

/// `DM-1`, `DUR-9`, `F-10`: a §6 family and a number.
fn requirement_id(id: &str) -> bool {
    id.split_once('-').is_some_and(|(family, number)| {
        REQUIREMENT_FAMILIES.contains(&family)
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
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
    items: &[workgraph::WorkItem],
    inputs: &Inputs<'_>,
    verdict: &mut Judgement,
) {
    let plan = registry::plan_requirements(inputs.plan);
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
    let listed: Vec<registry::Item> = items
        .iter()
        .map(|item| registry::Item {
            id: item.id.clone(),
            reqs: item.reqs.clone(),
            accept: item.accept.clone(),
            bead: item.bead.clone(),
        })
        .collect();
    let closed = registry::closed_items(&listed, inputs.export);
    for r in &requirements.requirement {
        let derived = registry::derived_status(&listed, &r.id, &closed);
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
    let dir = if package == "xtask" {
        "xtask".to_owned()
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

/// Condition 5, reported until W32: spec codes against domain codes, and an HTTP handler
/// per operation.
fn spec_and_code(spec: &Spec, inputs: &Inputs<'_>, verdict: &mut Judgement) {
    let domain = domain_codes(inputs.error_codes);
    if domain != spec.codes {
        verdict.reported.push(format!(
            "condition 5 (reported until W32): the spec's ErrorCode enum {:?} differs from \
             the domain ErrorCode {domain:?}",
            spec.codes
        ));
    }
    let handled = named_markers(inputs.sources, SERVER, "// api: ");
    let unhandled: Vec<&String> = spec
        .operations
        .iter()
        .filter(|op| !handled.contains(*op))
        .collect();
    if !unhandled.is_empty() {
        verdict.reported.push(format!(
            "condition 5 (reported until W32): no HTTP handler (`// api: <operationId>` \
             under {SERVER}) for {} operation(s): {unhandled:?}",
            unhandled.len()
        ));
    }
    verdict.cases += spec.codes.len() + spec.operations.len();
}

/// Condition 6, reported until W32: a conformance test per operation and error code.
fn spec_and_tests(spec: &Spec, sources: &[(String, String)], verdict: &mut Judgement) {
    let tested = named_markers(sources, CONFORMANCE, "// api-test: ");
    for (what, names) in [("operation", &spec.operations), ("error code", &spec.codes)] {
        let untested: Vec<&String> = names.iter().filter(|n| !tested.contains(*n)).collect();
        if !untested.is_empty() {
            verdict.reported.push(format!(
                "condition 6 (reported until W32): no conformance test (`// api-test: \
                 <name>` under {CONFORMANCE}) exercises {} {what}(s): {untested:?}",
                untested.len()
            ));
        }
        verdict.cases += names.len();
    }
}

/// Condition 7: every bug variant names an existing checker that should catch it.
fn bug_variants(checks: &ChecksFile, verdict: &mut Judgement) {
    for c in checks.check.iter().filter(|c| c.kind == "bug_variant") {
        verdict.cases += 1;
        let catchers = c.catches.iter().filter(|id| {
            checks
                .check
                .iter()
                .any(|other| &other.id == *id && other.kind == "checker")
        });
        if c.catches.is_empty() || catchers.count() != c.catches.len() {
            verdict.violations.push(format!(
                "condition 7: bug variant {} needs a designated catcher: catches {:?} must \
                 name existing checker rows",
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
    for item in items.iter().filter(|item| !item.bead.is_empty()) {
        verdict.cases += 1;
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

    /// A tracker export line for a work-item bead.
    fn bead(id: &str, status: &str, blocks: &[&str]) -> String {
        let deps: Vec<serde_json::Value> = blocks
            .iter()
            .map(|on| serde_json::json!({"depends_on_id": on, "type": "blocks"}))
            .collect();
        format!(
            "{}\n",
            serde_json::json!({"id": id, "status": status, "labels": [], "dependencies": deps})
        )
    }

    fn export_text(w03: &str) -> String {
        [
            bead("b1", "closed", &[]),
            bead("b2", "closed", &["b1"]),
            bead("b3", w03, &["b1", "b2"]),
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
    fn condition_4_fails_on_an_unresolved_or_empty_selector() {
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

    #[test]
    fn condition_7_fails_on_a_bug_variant_without_a_catcher() {
        let mut texts = Texts::valid();
        texts.checks = [
            texts.checks.clone(),
            row(
                "B-1",
                "bug_variant",
                "\"F-1\"",
                "planned",
                "catches = [\"A-CHECK\"]\n",
            ),
        ]
        .concat();
        fails_only(
            &texts,
            "condition 7",
            "a bug variant's catcher is an existing checker",
        );
    }

    #[test]
    fn condition_8_fails_on_work_graph_drift_and_on_tracker_edges() {
        let mut texts = Texts::valid();
        texts.export = [
            bead("b1", "closed", &[]),
            bead("b2", "closed", &["b1"]),
            bead("b3", "open", &["b1"]),
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
}
