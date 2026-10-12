//! `./dev sim corpus [--fast]`: the append-only regression corpus runner (PLAN.md §10.9,
//! §12.2 W42.3; T-4, T-8, O-6).
//!
//! `sim/corpus/` holds one TOML file per case, with provenance (bead, checker, date, build,
//! the failure it once showed), and any scenario files a case replays. A case is one of
//! three kinds, chosen by its `kind` field (absent means `check`, which is what the cases
//! committed before this runner are):
//!
//! - `check`: `rerun = "./dev check <ID> --case <NAME>"`, the test that pins the fix;
//! - `seeds`: `profile = "swarm"` and `seeds = [..]`, each a `./dev sim run --seed N` run;
//! - `scenario`: `scenario_file = "<name>.json"`, a scenario file in the corpus directory,
//!   replayed with `./dev sim run --scenario`.
//!
//! The corpus is a protected artefact (P9). `sim/corpus/INDEX.toml` lists every other file
//! of the directory with its SHA-256 and names the **fast subset**, the fixed reviewed list
//! of cases T0 runs once the whole corpus no longer fits its budget. [`load`] fails on a
//! file the index does not list, a listed file that is missing or changed, a fast entry that
//! is not a case, and a case it cannot parse into one of the three kinds. [`append_only`]
//! judges a tree's index against its parent's: nothing may be removed or changed, only
//! added. [`protected_hash`] is what every gate receipt records.
//!
//! A run is ok only when at least one case ran and every one passed: an incomplete run is
//! not a pass, and neither is a case whose process could not report (P10).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cargo::Cargo;
use crate::output::{ErrorKind, Report};
use crate::process;
use crate::sim::{self, Refusal, Verdict, tail};
use crate::sim_campaign::{self, Counts, Source};

/// Schema identifier of the run report (`schemas/dev/corpus-run.v1.schema.json`).
pub const SCHEMA: &str = "keyval.dev.corpus-run/v1";

/// The corpus directory, relative to the workspace root.
pub const DIR: &str = "sim/corpus";

/// The protected index inside [`DIR`].
pub const INDEX: &str = "INDEX.toml";

/// The most files the corpus directory may hold (CD-9).
pub const MAX_FILES: usize = 4096;

/// The most bytes one corpus file may hold (CD-9).
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// The most seeds one `seeds` case may name (CD-9).
pub const MAX_SEEDS: usize = 64;

/// The claim class every run is reported in (§10.15).
pub const LABEL: &str = "simulated regression corpus: each case re-runs a failure that was \
                         once found and fixed; simulation and test evidence, not proof, and \
                         not a real cluster (P6)";

/// What a run covers.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// Every case.
    All,
    /// Only the index's fast subset.
    Fast,
}

/// How one case is run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    /// A registered check restricted to one test.
    Check {
        /// The check ID.
        id: String,
        /// The test name.
        case: String,
    },
    /// Swarm-profile runs of these seeds.
    Seeds(Vec<u64>),
    /// A scenario file in the corpus directory.
    Scenario(String),
}

impl Run {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Check { .. } => "check",
            Self::Seeds(_) => "seeds",
            Self::Scenario(_) => "scenario",
        }
    }
}

/// One regression case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    /// The case file's name in the corpus directory.
    pub name: String,
    /// How to run it.
    pub run: Run,
    /// The command that re-runs it by hand.
    pub rerun: String,
}

/// A verified corpus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corpus {
    /// Every case, in file-name order.
    pub cases: Vec<Case>,
    /// The fast subset, in the index's order.
    pub fast: Vec<String>,
    /// [`protected_hash`] of the directory.
    pub hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexFile {
    fast: Vec<String>,
    #[serde(default)]
    file: Vec<IndexEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    name: String,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Check,
    Seeds,
    Scenario,
}

/// A case file. Provenance fields are required; unknown fields are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "provenance fields are parsed for validation only (br-w42-3-corpus-runner-0qq)"
)]
struct CaseFile {
    bead: String,
    checker: String,
    date: String,
    build: String,
    failure: String,
    scenario: Option<String>,
    seeds: Option<Vec<u64>>,
    rerun: Option<String>,
    shrunk: bool,
    non_vacuity: Option<String>,
    claim_class: String,
    kind: Option<Kind>,
    profile: Option<String>,
    scenario_file: Option<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Every file of the corpus directory (the index included) by name, with its bytes.
fn read_dir(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    let mut files = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
        let name = entry.file_name().into_string().map_err(|name| {
            format!("{}: {} is not UTF-8", dir.display(), name.to_string_lossy())
        })?;
        let path = entry.path();
        if !path.is_file() {
            return Err(format!(
                "{}: {name} is not a regular file; the corpus is flat",
                dir.display()
            ));
        }
        if files.len() >= MAX_FILES {
            return Err(format!("{}: more than {MAX_FILES} files", dir.display()));
        }
        let bytes =
            fs::read(&path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{}: {} bytes exceeds the {MAX_FILE_BYTES}-byte limit",
                path.display(),
                bytes.len()
            ));
        }
        files.insert(name, bytes);
    }
    Ok(files)
}

fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut canonical = String::new();
    for (name, bytes) in files {
        let _ = writeln!(canonical, "{name}\t{}", sha256_hex(bytes));
    }
    format!("sha256:{}", sha256_hex(canonical.as_bytes()))
}

/// The hash a gate receipt records for the corpus: SHA-256 over each file's name and
/// SHA-256 in name order, the index included. `None` when the directory cannot be read.
#[must_use]
pub fn protected_hash(root: &Path) -> Option<String> {
    read_dir(&root.join(DIR)).ok().map(|files| digest(&files))
}

/// The name-to-SHA-256 map of an index's text.
///
/// # Errors
///
/// The parse error, or a repeated name.
pub fn index_hashes(text: &str) -> Result<BTreeMap<String, String>, String> {
    let index: IndexFile = toml::from_str(text).map_err(|err| format!("{INDEX}: {err}"))?;
    let mut hashes = BTreeMap::new();
    for entry in index.file {
        if hashes.insert(entry.name.clone(), entry.sha256).is_some() {
            return Err(format!("{INDEX}: {} is listed twice", entry.name));
        }
    }
    Ok(hashes)
}

/// What an index may not do relative to its parent's (P9): drop a file, or change one.
/// Adding is the only edit.
#[must_use]
pub fn append_only(
    parent: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> Vec<String> {
    parent
        .iter()
        .filter_map(|(name, before)| match now.get(name) {
            None => Some(format!(
                "{name} was removed; the corpus is append-only (P9)"
            )),
            Some(after) if after != before => Some(format!(
                "{name} changed from sha256 {before} to {after}; the corpus is append-only (P9)"
            )),
            Some(_) => None,
        })
        .collect()
}

fn word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `./dev check <ID> --case <NAME>` as the pair it names; nothing else is a rerun.
fn parse_check(rerun: &str) -> Result<(String, String), String> {
    match rerun.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["./dev", "check", id, "--case", case] if word(id) && word(case) => {
            Ok(((*id).to_owned(), (*case).to_owned()))
        }
        _ => Err(format!(
            "rerun {rerun:?} is not `./dev check <ID> --case <NAME>`"
        )),
    }
}

fn case_from(name: &str, text: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<Case, String> {
    let file: CaseFile = toml::from_str(text).map_err(|err| format!("{name}: {err}"))?;
    let kind = file.kind.unwrap_or(Kind::Check);
    let stray = |what: &str| format!("{name}: {what} does not belong to a {kind:?} case");
    match kind {
        Kind::Check => {
            if file.profile.is_some() {
                return Err(stray("profile"));
            }
            if file.scenario_file.is_some() {
                return Err(stray("scenario_file"));
            }
            let rerun = file
                .rerun
                .ok_or_else(|| format!("{name}: a check case needs a rerun"))?;
            let (id, case) = parse_check(&rerun).map_err(|why| format!("{name}: {why}"))?;
            Ok(Case {
                name: name.to_owned(),
                run: Run::Check { id, case },
                rerun,
            })
        }
        Kind::Seeds => {
            if file.scenario_file.is_some() {
                return Err(stray("scenario_file"));
            }
            if file.profile.as_deref() != Some(sim_campaign::SWARM) {
                return Err(format!("{name}: a seeds case runs profile \"swarm\""));
            }
            let seeds = file.seeds.unwrap_or_default();
            if seeds.is_empty() || seeds.len() > MAX_SEEDS {
                return Err(format!(
                    "{name}: a seeds case names 1..={MAX_SEEDS} seeds, not {}",
                    seeds.len()
                ));
            }
            let rerun = seeds
                .iter()
                .map(|seed| format!("./dev sim run --seed {seed} --profile swarm"))
                .collect::<Vec<_>>()
                .join(" && ");
            Ok(Case {
                name: name.to_owned(),
                run: Run::Seeds(seeds),
                rerun,
            })
        }
        Kind::Scenario => {
            if file.profile.is_some() {
                return Err(stray("profile"));
            }
            let scenario = file
                .scenario_file
                .ok_or_else(|| format!("{name}: a scenario case needs scenario_file"))?;
            if !std::path::Path::new(&scenario)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                || !files.contains_key(&scenario)
            {
                return Err(format!(
                    "{name}: scenario_file {scenario:?} is not a .json file of {DIR}/"
                ));
            }
            let rerun = format!("./dev sim run --scenario {DIR}/{scenario}");
            Ok(Case {
                name: name.to_owned(),
                run: Run::Scenario(scenario),
                rerun,
            })
        }
    }
}

/// Verifies `root`'s corpus against its index and parses its cases.
///
/// # Errors
///
/// Every problem found, one line each: an unreadable directory or index, a file the index
/// does not list, a listed file that is missing or whose SHA-256 changed, a fast entry
/// that is not a case (or is repeated), a case that is none of the three kinds, and a
/// corpus with no case at all.
pub fn load(root: &Path) -> Result<Corpus, Vec<String>> {
    let dir = root.join(DIR);
    let files = read_dir(&dir).map_err(|why| vec![why])?;
    let Some(index_bytes) = files.get(INDEX) else {
        return Err(vec![format!("{DIR}/{INDEX} is missing")]);
    };
    let index_text = String::from_utf8_lossy(index_bytes);
    let index: IndexFile =
        toml::from_str(&index_text).map_err(|err| vec![format!("{INDEX}: {err}")])?;
    let listed = index_hashes(&index_text).map_err(|why| vec![why])?;
    let mut problems = Vec::new();
    for (name, bytes) in files.iter().filter(|(name, _)| name.as_str() != INDEX) {
        match listed.get(name) {
            None => problems.push(format!(
                "{name} is not listed in {INDEX}; list it (appending is the only edit)"
            )),
            Some(expected) if *expected != sha256_hex(bytes) => problems.push(format!(
                "{name} changed: sha256 {} but {INDEX} lists {expected}; a corpus file never changes (P9)",
                sha256_hex(bytes)
            )),
            Some(_) => {}
        }
    }
    for name in listed.keys().filter(|name| !files.contains_key(*name)) {
        problems.push(format!("{name} is listed in {INDEX} but missing (P9)"));
    }
    let mut cases = Vec::new();
    for (name, bytes) in &files {
        let is_case = std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
            && name != INDEX;
        let is_scenario = std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if is_case {
            match case_from(name, &String::from_utf8_lossy(bytes), &files) {
                Ok(case) => cases.push(case),
                Err(why) => problems.push(why),
            }
        } else if !is_scenario && name != INDEX {
            problems.push(format!(
                "{name}: the corpus holds .toml cases and .json scenarios only"
            ));
        }
    }
    let names: BTreeSet<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    let mut seen = BTreeSet::new();
    for entry in &index.fast {
        if !names.contains(entry.as_str()) {
            problems.push(format!(
                "fast subset entry {entry} is not a case of the corpus"
            ));
        }
        if !seen.insert(entry.as_str()) {
            problems.push(format!("fast subset entry {entry} is repeated"));
        }
    }
    if cases.is_empty() {
        problems.push("the corpus holds no case (P10)".to_owned());
    }
    if problems.is_empty() {
        Ok(Corpus {
            cases,
            fast: index.fast,
            hash: digest(&files),
        })
    } else {
        Err(problems)
    }
}

/// How a case ended.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseVerdict {
    /// Every run of the case passed.
    Pass,
    /// A run failed.
    Fail,
    /// A run did not reach a verdict.
    Incomplete,
    /// A run produced no report.
    Error,
}

impl CaseVerdict {
    const fn severity(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Incomplete => 1,
            Self::Fail => 2,
            Self::Error => 3,
        }
    }
}

/// One run's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// How it ended.
    pub verdict: CaseVerdict,
    /// Why it was not a pass.
    pub reason: Option<String>,
}

impl Outcome {
    /// A pass.
    #[must_use]
    pub const fn pass() -> Self {
        Self {
            verdict: CaseVerdict::Pass,
            reason: None,
        }
    }
}

/// What actually runs a case; tests substitute a fake.
pub trait Execute {
    /// `./dev check <id> --case <case>`.
    fn check(&self, id: &str, case: &str) -> Outcome;
    /// One swarm seed.
    fn seed(&self, seed: u64) -> Outcome;
    /// One scenario file of the corpus directory.
    fn scenario(&self, file: &Path) -> Outcome;
}

/// One case's line of the report.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseLine {
    /// The case file's name.
    pub case: String,
    /// `check`, `seeds` or `scenario`.
    pub kind: String,
    /// How many runs it made.
    pub runs: u64,
    /// The worst of its runs.
    pub verdict: CaseVerdict,
    /// The first reason it is not a pass.
    pub reason: Option<String>,
    /// The command that re-runs it by hand.
    pub rerun: String,
}

/// `keyval.dev.corpus-run/v1`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusReport {
    /// [`SCHEMA`].
    pub schema: String,
    /// Whether at least one case ran, every case passed and the corpus verified.
    pub ok: bool,
    /// What the runs are, and are not (P6, §10.15).
    pub label: String,
    /// Which cases were selected.
    pub selection: Selection,
    /// [`protected_hash`], or `null` when the directory could not be read.
    pub corpus_sha256: Option<String>,
    /// What is wrong with the corpus; when any, no case ran.
    pub problems: Vec<String>,
    /// The verdict counts.
    pub counts: Counts,
    /// One line per case run, in file-name order.
    pub cases: Vec<CaseLine>,
}

impl Report for CorpusReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut text = format!(
            "{}\ncorpus ({:?}, {}): {} pass, {} fail, {} incomplete, {} error",
            self.label,
            self.selection,
            self.corpus_sha256.as_deref().unwrap_or("unreadable"),
            self.counts.pass,
            self.counts.fail,
            self.counts.incomplete,
            self.counts.error,
        );
        for problem in &self.problems {
            let _ = write!(text, "\nproblem: {problem}");
        }
        for line in self
            .cases
            .iter()
            .filter(|line| line.verdict != CaseVerdict::Pass)
        {
            let _ = write!(
                text,
                "\n{}: {:?}{}\n  re-run: {}",
                line.case,
                line.verdict,
                line.reason
                    .as_ref()
                    .map_or_else(String::new, |why| format!(" — {why}")),
                line.rerun,
            );
        }
        text
    }
}

fn run_case(case: &Case, dir: &Path, exec: &impl Execute) -> CaseLine {
    let outcomes: Vec<(Option<u64>, Outcome)> = match &case.run {
        Run::Check { id, case } => vec![(None, exec.check(id, case))],
        Run::Seeds(seeds) => seeds
            .iter()
            .map(|&seed| (Some(seed), exec.seed(seed)))
            .collect(),
        Run::Scenario(file) => vec![(None, exec.scenario(&dir.join(file)))],
    };
    let worst = outcomes
        .iter()
        .map(|(_, outcome)| outcome.verdict)
        .max_by_key(|verdict| verdict.severity())
        .unwrap_or(CaseVerdict::Error);
    let reason = outcomes
        .iter()
        .find(|(_, outcome)| outcome.verdict != CaseVerdict::Pass)
        .map(|(seed, outcome)| {
            let why = outcome.reason.clone().unwrap_or_default();
            seed.map_or(why.clone(), |seed| format!("seed {seed}: {why}"))
        });
    CaseLine {
        case: case.name.clone(),
        kind: case.run.kind().to_owned(),
        runs: outcomes.len() as u64,
        verdict: worst,
        reason,
        rerun: case.rerun.clone(),
    }
}

/// Runs the selected cases of `root`'s corpus with `exec`.
///
/// A corpus that does not verify runs nothing: the report says why and is not ok.
#[must_use]
pub fn run(root: &Path, selection: Selection, exec: &impl Execute) -> CorpusReport {
    let corpus_sha256 = protected_hash(root);
    let (problems, lines) = match load(root) {
        Err(problems) => (problems, Vec::new()),
        Ok(corpus) => {
            let dir = root.join(DIR);
            let lines = corpus
                .cases
                .iter()
                .filter(|case| match selection {
                    Selection::All => true,
                    Selection::Fast => corpus.fast.contains(&case.name),
                })
                .map(|case| run_case(case, &dir, exec))
                .collect();
            (Vec::new(), lines)
        }
    };
    let mut counts = Counts::default();
    for line in &lines {
        match line.verdict {
            CaseVerdict::Pass => counts.pass += 1,
            CaseVerdict::Fail => counts.fail += 1,
            CaseVerdict::Incomplete => counts.incomplete += 1,
            CaseVerdict::Error => counts.error += 1,
        }
    }
    CorpusReport {
        schema: SCHEMA.to_owned(),
        ok: problems.is_empty() && !lines.is_empty() && counts.pass == lines.len() as u64,
        label: LABEL.to_owned(),
        selection,
        corpus_sha256,
        problems,
        counts,
        cases: lines,
    }
}

/// The real executor: `./dev check` in this binary, and `keyval-sim` through cargo.
pub struct Live<'a> {
    /// The workspace root.
    pub root: &'a Path,
    /// The simulator configuration.
    pub config: PathBuf,
    /// The artifacts directory.
    pub artifacts: PathBuf,
    /// Local cargo (the trace must be on this machine).
    pub cargo: &'a Cargo,
}

impl Live<'_> {
    fn sim_run(&self, source: &Source) -> Outcome {
        let launch = |args: &[&str]| {
            let mut cmd = self.cargo.command(self.root, args);
            process::run(&mut cmd, sim::LIMIT)
                .map_err(|err| format!("{}: {err}", self.cargo.describe(args)))
        };
        match sim_campaign::run_with(source, &self.config, &self.artifacts, launch) {
            Ok(report) => {
                let reason = report
                    .failures
                    .first()
                    .or_else(|| report.incomplete.first())
                    .cloned();
                Outcome {
                    verdict: match report.verdict {
                        Verdict::Pass => CaseVerdict::Pass,
                        Verdict::Fail => CaseVerdict::Fail,
                        Verdict::Incomplete => CaseVerdict::Incomplete,
                    },
                    reason,
                }
            }
            Err(Refusal { message, .. }) => Outcome {
                verdict: CaseVerdict::Error,
                reason: Some(message),
            },
        }
    }
}

impl Execute for Live<'_> {
    fn check(&self, id: &str, case: &str) -> Outcome {
        let error = |why: String| Outcome {
            verdict: CaseVerdict::Error,
            reason: Some(why),
        };
        let mut cmd = std::process::Command::new("./dev");
        cmd.current_dir(self.root)
            .args(["check", id, "--case", case, "--json"]);
        match process::run(&mut cmd, sim::LIMIT) {
            Ok(done) => match done.status.code() {
                Some(0) => Outcome::pass(),
                Some(1) => Outcome {
                    verdict: CaseVerdict::Fail,
                    reason: Some(format!(
                        "./dev check {id} --case {case}: {}",
                        tail(&done.stderr)
                    )),
                },
                _ => error(format!(
                    "./dev check {id} --case {case} ended with {}: {}",
                    done.status,
                    tail(&done.stderr)
                )),
            },
            Err(err) => error(format!("./dev check {id} --case {case}: {err}")),
        }
    }

    fn seed(&self, seed: u64) -> Outcome {
        self.sim_run(&Source::Seed(seed))
    }

    fn scenario(&self, file: &Path) -> Outcome {
        self.sim_run(&Source::File(file.to_path_buf()))
    }
}

/// Runs the corpus for `./dev sim corpus`.
///
/// `parent_index` is the parent commit's `INDEX.toml`; when given, the index must only have
/// grown from it (P9).
///
/// # Errors
///
/// A refusal when the artifacts directory cannot be created.
pub fn run_live(
    root: &Path,
    selection: Selection,
    config: PathBuf,
    parent_index: Option<&Path>,
) -> Result<CorpusReport, Refusal> {
    let artifacts = root.join("artifacts");
    fs::create_dir_all(&artifacts).map_err(|err| {
        Refusal::new(
            ErrorKind::Internal,
            format!("cannot create {}: {err}", artifacts.display()),
        )
    })?;
    let cargo = Cargo::local();
    let live = Live {
        root,
        config,
        artifacts,
        cargo: &cargo,
    };
    let mut report = run(root, selection, &live);
    if let Some(parent) = parent_index {
        let violations = judge_append_only(root, parent);
        report.ok &= violations.is_empty();
        report.problems.extend(violations);
    }
    Ok(report)
}

/// The append-only problems of `root`'s index against the parent commit's index at `parent`.
fn judge_append_only(root: &Path, parent: &Path) -> Vec<String> {
    let read = |path: &Path| {
        fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))
            .and_then(|text| index_hashes(&text))
    };
    match (read(parent), read(&root.join(DIR).join(INDEX))) {
        (Ok(before), Ok(now)) => append_only(&before, &now),
        (Err(why), _) | (_, Err(why)) => vec![why],
    }
}

#[cfg(test)]
pub mod tests {
    // check: CORPUS
    use std::cell::RefCell;

    use super::*;
    use crate::test_support::{scratch_dir, workspace_root};

    /// Passes every case; for the schema tests.
    pub struct AlwaysPass;

    impl Execute for AlwaysPass {
        fn check(&self, _id: &str, _case: &str) -> Outcome {
            Outcome::pass()
        }

        fn seed(&self, _seed: u64) -> Outcome {
            Outcome::pass()
        }

        fn scenario(&self, _file: &Path) -> Outcome {
            Outcome::pass()
        }
    }

    /// Runs `Check` cases as `fail_on` says and counts every call.
    struct Fake {
        fail_on: Vec<&'static str>,
        seed_verdict: CaseVerdict,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn passing() -> Self {
            Self {
                fail_on: Vec::new(),
                seed_verdict: CaseVerdict::Pass,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Execute for Fake {
        fn check(&self, id: &str, case: &str) -> Outcome {
            self.calls.borrow_mut().push(format!("{id}/{case}"));
            if self.fail_on.contains(&case) {
                Outcome {
                    verdict: CaseVerdict::Fail,
                    reason: Some(format!("{case} failed")),
                }
            } else {
                Outcome::pass()
            }
        }

        fn seed(&self, seed: u64) -> Outcome {
            self.calls.borrow_mut().push(format!("seed {seed}"));
            Outcome {
                verdict: self.seed_verdict,
                reason: (self.seed_verdict != CaseVerdict::Pass).then(|| "not a pass".to_owned()),
            }
        }

        fn scenario(&self, file: &Path) -> Outcome {
            self.calls
                .borrow_mut()
                .push(format!("scenario {}", file.display()));
            Outcome::pass()
        }
    }

    const CASE: &str = "bead = \"br-x\"\nchecker = \"C\"\ndate = \"2026-10-12\"\nbuild = \"b\"\n\
                        failure = \"f\"\nshrunk = false\nclaim_class = \"simulation evidence, not proof\"\n";

    /// A corpus directory under a fresh scratch root: `(file name, text)` pairs, indexed
    /// correctly, with `fast` as the fast subset.
    fn fixture(name: &str, files: &[(&str, String)], fast: &[&str]) -> PathBuf {
        let root = scratch_dir(name);
        let dir = root.join(DIR);
        fs::create_dir_all(&dir).unwrap();
        let mut index = format!(
            "fast = [{}]\n",
            fast.iter()
                .map(|f| format!("{f:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for (file, text) in files {
            fs::write(dir.join(file), text).unwrap();
            let _ = write!(
                index,
                "\n[[file]]\nname = {file:?}\nsha256 = \"{}\"\n",
                sha256_hex(text.as_bytes())
            );
        }
        fs::write(dir.join(INDEX), index).unwrap();
        root
    }

    fn check_case(case: &str) -> String {
        format!("{CASE}rerun = \"./dev check SIM-X --case {case}\"\n")
    }

    #[test]
    fn the_committed_corpus_verifies_against_its_index_and_every_case_parses() {
        let corpus = load(&workspace_root()).unwrap_or_else(|problems| {
            panic!("rule: sim/corpus verifies against INDEX.toml (P9); problems {problems:#?}")
        });
        let names: Vec<&str> = corpus.cases.iter().map(|case| case.name.as_str()).collect();
        for earlier in [
            "br-50zd-lost-response-at-send.toml",
            "br-50zd-misuse-ordering-and-dispatch.toml",
            "br-50zd-phase-mark-placement.toml",
        ] {
            assert!(
                names.contains(&earlier),
                "rule: a case committed earlier stays in the corpus (append-only, P9); {earlier} missing from {names:?}"
            );
            assert!(
                corpus.fast.iter().any(|fast| fast == earlier),
                "rule: the reviewed fast subset names {earlier}; fast {:?}",
                corpus.fast
            );
        }
        assert_eq!(
            protected_hash(&workspace_root()).as_deref(),
            Some(corpus.hash.as_str()),
            "rule: the receipt hash is the verified corpus's hash"
        );
    }

    #[test]
    fn a_changed_removed_or_unlisted_file_is_a_problem() {
        let root = fixture(
            "corpus-tamper",
            &[("a.toml", check_case("one")), ("b.toml", check_case("two"))],
            &["a.toml"],
        );
        assert!(
            load(&root).is_ok(),
            "fixture: the untouched corpus verifies"
        );
        let dir = root.join(DIR);
        fs::write(dir.join("a.toml"), check_case("changed")).unwrap();
        fs::remove_file(dir.join("b.toml")).unwrap();
        fs::write(dir.join("c.toml"), check_case("three")).unwrap();
        let problems = load(&root).unwrap_err().join("\n");
        for needle in [
            "a.toml changed",
            "b.toml is listed in INDEX.toml but missing",
            "c.toml is not listed",
        ] {
            assert!(
                problems.contains(needle),
                "rule: tampering with the corpus is reported ({needle:?}) (P9); problems {problems}"
            );
        }
    }

    #[test]
    fn append_only_allows_additions_and_refuses_removal_and_change() {
        let parent: BTreeMap<String, String> = [("a", "1"), ("b", "2")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let mut grown = parent.clone();
        grown.insert("c".to_owned(), "3".to_owned());
        assert!(
            append_only(&parent, &grown).is_empty(),
            "rule: adding a case is the only edit"
        );
        let mut removed = grown.clone();
        removed.remove("a");
        let mut changed = grown;
        changed.insert("b".to_owned(), "9".to_owned());
        let removed = append_only(&parent, &removed);
        let changed = append_only(&parent, &changed);
        assert!(
            removed.len() == 1 && removed[0].starts_with("a was removed"),
            "rule: removing a case fails (P9); {removed:?}"
        );
        assert!(
            changed.len() == 1 && changed[0].starts_with("b changed"),
            "rule: changing a case fails (P9); {changed:?}"
        );
        let text = "fast = []\n[[file]]\nname = \"a\"\nsha256 = \"1\"\n[[file]]\nname = \"a\"\nsha256 = \"2\"\n";
        assert!(
            index_hashes(text).is_err(),
            "rule: an index lists a file once"
        );
    }

    #[test]
    fn a_case_is_one_of_three_kinds_and_a_rerun_is_never_a_shell_line() {
        let seeds = format!("{CASE}kind = \"seeds\"\nprofile = \"swarm\"\nseeds = [3, 4]\n");
        let scenario = format!("{CASE}kind = \"scenario\"\nscenario_file = \"s.json\"\n");
        let root = fixture(
            "corpus-kinds",
            &[
                ("a.toml", check_case("one")),
                ("b.toml", seeds),
                ("c.toml", scenario),
                ("s.json", "{}".to_owned()),
            ],
            &[],
        );
        let corpus = load(&root).unwrap();
        let kinds: Vec<&str> = corpus.cases.iter().map(|case| case.run.kind()).collect();
        assert_eq!(
            kinds,
            ["check", "seeds", "scenario"],
            "rule: kinds follow the case files"
        );
        for bad in [
            format!("{CASE}rerun = \"./dev check SIM-X --case one; rm -rf x\"\n"),
            format!("{CASE}rerun = \"cargo test\"\n"),
            format!("{CASE}\n"),
            format!("{CASE}kind = \"seeds\"\nprofile = \"slice\"\nseeds = [1]\n"),
            format!("{CASE}kind = \"seeds\"\nprofile = \"swarm\"\nseeds = []\n"),
            format!("{CASE}kind = \"scenario\"\nscenario_file = \"missing.json\"\n"),
            format!("{CASE}rerun = \"./dev check SIM-X --case one\"\nprofile = \"swarm\"\n"),
            format!("{CASE}rerun = \"./dev check SIM-X --case one\"\nsurprise = 1\n"),
        ] {
            let root = fixture("corpus-bad", &[("a.toml", bad.clone())], &[]);
            assert!(
                load(&root).is_err(),
                "rule: a malformed case is refused; case {bad:?}"
            );
        }
    }

    #[test]
    fn the_fast_subset_is_a_fixed_list_of_cases_and_selects_only_them() {
        let files = [
            ("a.toml", check_case("one")),
            ("b.toml", check_case("two")),
            ("c.toml", check_case("three")),
        ];
        let root = fixture("corpus-fast", &files, &["b.toml"]);
        let fake = Fake::passing();
        let fast = run(&root, Selection::Fast, &fake);
        assert!(
            fast.ok && fast.cases.len() == 1 && *fake.calls.borrow() == ["SIM-X/two"],
            "rule: --fast runs exactly the listed cases; calls {:?}; report {fast:?}",
            fake.calls
        );
        let all = run(&root, Selection::All, &Fake::passing());
        assert!(
            all.ok && all.cases.len() == 3 && all.counts.pass == 3,
            "rule: the whole corpus runs by default, and adding a case never drops one; {all:?}"
        );
        let unknown = fixture(
            "corpus-fast-unknown",
            &files,
            &["zzz.toml", "b.toml", "b.toml"],
        );
        let problems = load(&unknown).unwrap_err().join("\n");
        assert!(
            problems.contains("zzz.toml is not a case") && problems.contains("b.toml is repeated"),
            "rule: a fast entry must be a case, once; problems {problems}"
        );
    }

    #[test]
    fn only_all_passing_runs_make_a_run_ok_and_a_failure_names_its_rerun() {
        let seeds = format!("{CASE}kind = \"seeds\"\nprofile = \"swarm\"\nseeds = [3, 4]\n");
        let files = [("a.toml", check_case("one")), ("b.toml", seeds)];
        let root = fixture("corpus-verdicts", &files, &[]);
        let failing = Fake {
            fail_on: vec!["one"],
            ..Fake::passing()
        };
        let report = run(&root, Selection::All, &failing);
        let line = &report.cases[0];
        assert!(
            !report.ok
                && report.counts
                    == Counts {
                        pass: 1,
                        fail: 1,
                        incomplete: 0,
                        error: 0
                    }
                && line.verdict == CaseVerdict::Fail
                && line.rerun == "./dev check SIM-X --case one"
                && report
                    .render_human()
                    .contains("re-run: ./dev check SIM-X --case one"),
            "rule: a failed case makes the run fail and names its re-run command (P10); {report:?}"
        );
        let unjudged = Fake {
            seed_verdict: CaseVerdict::Incomplete,
            ..Fake::passing()
        };
        let report = run(&root, Selection::All, &unjudged);
        let seeds_line = &report.cases[1];
        assert!(
            !report.ok
                && seeds_line.verdict == CaseVerdict::Incomplete
                && seeds_line.runs == 2
                && seeds_line.reason.as_deref() == Some("seed 3: not a pass"),
            "rule: an incomplete seed is not a pass, and the case reports its first non-pass (P10); {report:?}"
        );
    }

    #[test]
    fn a_corpus_that_does_not_verify_or_is_empty_runs_nothing_and_is_not_ok() {
        let root = fixture("corpus-unverified", &[("a.toml", check_case("one"))], &[]);
        fs::write(root.join(DIR).join("a.toml"), check_case("edited")).unwrap();
        let fake = Fake::passing();
        let report = run(&root, Selection::All, &fake);
        assert!(
            !report.ok
                && report.cases.is_empty()
                && fake.calls.borrow().is_empty()
                && !report.problems.is_empty(),
            "rule: a corpus whose files changed runs no case (P9); {report:?}"
        );
        let empty = fixture("corpus-empty", &[], &[]);
        let report = run(&empty, Selection::All, &Fake::passing());
        assert!(
            !report.ok && report.problems.iter().any(|p| p.contains("no case")),
            "rule: zero cases is never a pass (P10); {report:?}"
        );
        let missing = scratch_dir("corpus-missing");
        let report = run(&missing, Selection::All, &Fake::passing());
        assert!(
            !report.ok && report.corpus_sha256.is_none(),
            "rule: a missing corpus directory is a problem, not a pass; {report:?}"
        );
    }

    #[test]
    fn the_parent_index_comparison_reads_files_and_refuses_a_dropped_case() {
        let root = fixture("corpus-parent", &[("a.toml", check_case("one"))], &[]);
        let grown = root.join(DIR).join(INDEX);
        let parent = root.join("parent-INDEX.toml");
        fs::copy(&grown, &parent).unwrap();
        assert!(
            judge_append_only(&root, &parent).is_empty(),
            "rule: an unchanged index is append-only"
        );
        fs::write(
            &parent,
            "fast = []\n[[file]]\nname = \"gone.toml\"\nsha256 = \"1\"\n",
        )
        .unwrap();
        let problems = judge_append_only(&root, &parent);
        assert!(
            problems.len() == 1 && problems[0].starts_with("gone.toml was removed"),
            "rule: a case the parent had is not dropped (P9); {problems:?}"
        );
        assert!(
            judge_append_only(&root, &root.join("nope")).len() == 1,
            "rule: an unreadable parent index is a problem, not a pass"
        );
    }

    #[test]
    fn the_protected_hash_follows_every_byte_of_every_file() {
        let root = fixture("corpus-hash", &[("a.toml", check_case("one"))], &[]);
        let before = protected_hash(&root).unwrap();
        assert_eq!(
            protected_hash(&root),
            Some(before.clone()),
            "rule: the hash is stable"
        );
        assert!(
            before.starts_with("sha256:") && before.len() == 71,
            "rule: sha256:<64 hex>; {before}"
        );
        fs::write(root.join(DIR).join("extra.json"), "{}").unwrap();
        assert_ne!(
            protected_hash(&root),
            Some(before),
            "rule: adding a file changes the receipt's corpus hash"
        );
    }
}
