//! `./dev gate <tier>`: run a gate tier on an immutable tree and write a receipt
//! (PLAN.md §10.10, P7, P10, P11).
//!
//! Until the verification registry (W05) drives staging and W46 implements the other
//! tiers, `./dev gate fast` runs W01's **fixed bootstrap set** ([`BOOTSTRAP_SET`]):
//! format check, lints with denied warnings (clippy plus the LINT-ALLOWS and LINT-TIERS
//! policy checks), the dependency rule (DEP-RULE, CD-1), the dependency ledger
//! (DEP-LEDGER, CD-12), the workspace tests (xtask's included), and UBS on the exact
//! changed files.
//!
//! **Exact trees only (P11).** In a git checkout the gate refuses to run unless the
//! working tree is clean, and tests `HEAD^{tree}`. A commit wrapper that has
//! materialised a candidate tree (W07, or the coordinator before W07) passes
//! `--tree <id>` and its `--changed` paths; the gate then never calls git (§11.10
//! step 4), and the receipt says the caller asserted the tree ID.
//!
//! **No vacuous passes (P10).** Every step must examine at least one case. If no
//! scannable source file changed, UBS scans every first-party source instead of none.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cargo::{Cargo, Runner};
use crate::check::StaticCheck;
use crate::clock::Utc;
use crate::libtest;
use crate::lint;
use crate::metadata;
use crate::output::Report;
use crate::process::{self, Captured};
use crate::steplog::{self, StepLog};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.gate/v1";

/// Schema identifier of a receipt file.
pub const RECEIPT_SCHEMA: &str = "keyval.dev.receipt/v1";

/// T0 time budget: target and alert threshold (§10.10). A run is never cut off.
const TARGET: Duration = Duration::from_mins(2);
const ALERT: Duration = Duration::from_mins(3);

/// Wall-clock limit for one step, including compilation.
const STEP_LIMIT: Duration = Duration::from_mins(30);

/// Most violation lines quoted in a step's `detail` (the full list is in its log).
const DETAIL_LINES: usize = 10;

/// Files whose hashes every receipt records: the policy the run was judged by.
const POLICY_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "clippy.toml",
    "DEPENDENCIES.md",
    "xtask/lint/deterministic.clippy.toml",
    "xtask/lint/deterministic-lints.rs",
];

/// Gate tiers (§10.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GateTier {
    /// T0: before every commit.
    Fast,
    /// T1: hourly integration (W46).
    Integration,
    /// T2: nightly campaign (W46).
    Campaign,
    /// T3: release (W46, W51).
    Release,
}

/// One step of a gate run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `cargo fmt --all --check`.
    Fmt,
    /// `cargo clippy` on the whole workspace with denied warnings.
    Clippy,
    /// A static check from the `./dev check` catalogue.
    Static(StaticCheck),
    /// `cargo test` on the whole workspace, doctests included.
    Tests,
    /// UBS on the exact changed files.
    Ubs,
}

/// W01's fixed bootstrap set for `./dev gate fast` (§10.10 "Staged contents").
pub const BOOTSTRAP_SET: &[Step] = &[
    Step::Fmt,
    Step::Clippy,
    Step::Static(StaticCheck::LintAllows),
    Step::Static(StaticCheck::LintTiers),
    Step::Static(StaticCheck::DepRule),
    Step::Static(StaticCheck::DepLedger),
    Step::Tests,
    Step::Ubs,
];

impl Step {
    /// The step's ID in receipts; static checks use their `./dev check` ID.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Fmt => "fmt",
            Self::Clippy => "clippy",
            Self::Static(check) => check.id(),
            Self::Tests => "tests",
            Self::Ubs => "ubs",
        }
    }
}

/// Where the tree ID came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeSource {
    /// `HEAD^{tree}` of a clean git checkout.
    GitHead,
    /// Asserted by the caller (a commit wrapper that materialised the candidate).
    Caller,
}

/// The tree a gate run judged.
#[derive(Clone, Debug, Serialize)]
pub struct Tree {
    /// Git tree object ID.
    pub id: String,
    /// Where the ID came from.
    pub source: TreeSource,
    /// The changed (staged) paths, which UBS scans.
    pub changed: Vec<String>,
}

/// Pass or fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every step passed.
    Passed,
    /// At least one step failed.
    Failed,
}

/// What one step did.
#[derive(Debug, Serialize)]
pub struct StepResult {
    /// Step ID.
    pub id: &'static str,
    /// Outcome.
    pub status: Verdict,
    /// Cases examined: crates, files, tests or dependency edges. Zero fails (P10).
    pub cases: u64,
    /// What a case is for this step.
    pub unit: &'static str,
    /// Wall-clock time.
    pub duration_ms: u64,
    /// The command run, if any.
    pub command: Option<String>,
    /// Path of the step's full log.
    pub log: Option<String>,
    /// Why it failed, briefly.
    pub detail: Option<String>,
}

/// Inputs hashed into the build ID (§10.2 run manifest: tree, toolchain, target,
/// lockfile, features and Raft patch).
#[derive(Debug, Serialize)]
pub struct BuildInputs {
    /// `rustc -vV` output.
    pub rustc: String,
    /// `cargo --version` output.
    pub cargo: String,
    /// Host target triple.
    pub target: String,
    /// SHA-256 of `Cargo.lock`.
    pub lockfile_sha256: String,
    /// Cargo features built.
    pub features: &'static str,
    /// SHA-256 of `third_party/raft.patch`, once W02 adds it.
    pub raft_patch_sha256: Option<String>,
}

/// Tools that shaped the run.
#[derive(Debug, Serialize)]
pub struct Tools {
    /// How cargo builds were launched.
    pub cargo_runner: Runner,
    /// `ubs --version`, if UBS ran.
    pub ubs: Option<String>,
}

/// The T0 budget and whether this run exceeded its alert threshold.
#[derive(Debug, Serialize)]
pub struct Budget {
    /// Target duration.
    pub target_ms: u64,
    /// Alert threshold; a sustained overrun files a bead (§10.11, W46).
    pub alert_ms: u64,
    /// Whether this run took longer than the alert threshold.
    pub over_alert: bool,
}

/// A gate receipt (`keyval.dev.receipt/v1`): exactly what ran, on exactly which tree.
#[derive(Debug, Serialize)]
pub struct Receipt {
    /// Schema identifier.
    pub schema: &'static str,
    /// Always `gate` for gate runs.
    pub kind: &'static str,
    /// Gate tier.
    pub tier: GateTier,
    /// Which check set ran: `bootstrap` until registry staging (W05).
    pub set: &'static str,
    /// Unique run ID.
    pub run_id: String,
    /// Overall verdict.
    pub result: Verdict,
    /// The tree judged.
    pub tree: Tree,
    /// `sha256:` of the build inputs.
    pub build_id: String,
    /// What the build ID hashes.
    pub build_inputs: BuildInputs,
    /// `sha256:` of each policy file.
    pub policy_hashes: BTreeMap<&'static str, String>,
    /// Tool versions and runners.
    pub tools: Tools,
    /// Start time (UTC, RFC 3339).
    pub started_at: String,
    /// End time (UTC, RFC 3339).
    pub finished_at: String,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Budget accounting.
    pub budget: Budget,
    /// Every step, in order.
    pub steps: Vec<StepResult>,
    /// The step-record log (W01.6 standard).
    pub step_log: StepLogSummary,
}

/// Where the step records went, and how many the bound dropped (CD-9).
#[derive(Debug, Serialize)]
pub struct StepLogSummary {
    /// Path of `steps.jsonl`.
    pub path: Option<String>,
    /// Records written.
    pub records: usize,
    /// Records dropped by the bound or by write errors.
    pub dropped: usize,
}

/// The `./dev gate` report (`keyval.dev.gate/v1`).
#[derive(Debug, Serialize)]
pub struct GateReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when the run passed.
    pub ok: bool,
    /// Where the receipt was written.
    pub receipt_path: String,
    /// The receipt itself.
    pub receipt: Receipt,
}

/// Everything a gate run needs.
pub struct Context<'a> {
    /// Root of the tree being judged.
    pub root: &'a Path,
    /// The tree.
    pub tree: Tree,
    /// Directory receiving receipts (`<artifacts>/receipts/`).
    pub artifacts: PathBuf,
    /// How cargo builds run.
    pub cargo: Cargo,
    /// Whether to echo each step's record to stderr as it finishes.
    pub progress: bool,
}

/// Determines the tree to judge. With `asserted`, the caller vouches for the ID and git
/// is never consulted; otherwise `root` must be a clean git checkout.
pub fn resolve_tree(
    root: &Path,
    asserted: Option<&str>,
    changed: Vec<String>,
) -> Result<Tree, String> {
    resolve_tree_with(root, asserted, changed, &git)
}

/// Runs git with `args` in a directory and returns its stdout.
type GitRunner = dyn Fn(&Path, &[&str]) -> Result<String, String>;

fn resolve_tree_with(
    root: &Path,
    asserted: Option<&str>,
    changed: Vec<String>,
    git: &GitRunner,
) -> Result<Tree, String> {
    if let Some(id) = asserted {
        let valid = matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(format!("--tree {id:?} is not a git tree object ID"));
        }
        return Ok(Tree {
            id: id.to_ascii_lowercase(),
            source: TreeSource::Caller,
            changed,
        });
    }
    let status = git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let dirty: Vec<&str> = status.lines().filter(|l| !l.is_empty()).collect();
    if !dirty.is_empty() {
        return Err(format!(
            "the working tree has {} uncommitted path(s) (first: {:?}); a receipt must \
             describe an exact tree (P11). Run in a clean checkout, or pass --tree from a \
             commit wrapper that materialised the candidate",
            dirty.len(),
            dirty.first().copied().unwrap_or_default()
        ));
    }
    let id = git(root, &["rev-parse", "HEAD^{tree}"])?.trim().to_owned();
    let changed = git(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "HEAD",
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect();
    Ok(Tree {
        id,
        source: TreeSource::GitHead,
        changed,
    })
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    let captured = process::run(&mut cmd, Duration::from_mins(1))
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if captured.status.success() {
        Ok(captured.stdout)
    } else {
        Err(format!(
            "git {} exited with {}: {}",
            args.join(" "),
            captured.status,
            captured.stderr.trim()
        ))
    }
}

/// Runs `steps` on the context's tree, writes the receipt and returns the report.
pub fn run(ctx: &Context<'_>, tier: GateTier, steps: &[Step]) -> Result<GateReport, String> {
    let started = Instant::now();
    let started_at = Utc::now();
    let run_id = format!(
        "{}-{}-{}-{}",
        tier_name(tier),
        started_at.compact(),
        ctx.tree.id.get(..12).unwrap_or(&ctx.tree.id),
        std::process::id()
    );
    let receipts = ctx.artifacts.join("receipts");
    let log_dir = receipts.join(format!("gate-{run_id}"));
    fs::create_dir_all(&log_dir)
        .map_err(|err| format!("cannot create {}: {err}", log_dir.display()))?;

    let build_inputs = build_inputs(ctx.root)?;
    let build_id = build_id(&ctx.tree.id, &build_inputs);
    let mut ubs_version = None;
    // Step records (W01.6 standard) go to stderr as progress and to steps.jsonl next to
    // the step logs. A gate runs every step, so one run reports every failure.
    let mut steplog = StepLog::new(
        &format!("gate {}", tier_name(tier)),
        Some(&log_dir.join("steps.jsonl")),
        ctx.progress,
    )
    .map_err(|err| format!("cannot create the step log in {}: {err}", log_dir.display()))?;
    let results: Vec<StepResult> = steps
        .iter()
        .map(|&step| {
            let result = run_step(ctx, step, &log_dir, &mut ubs_version);
            log_step(&mut steplog, step, &result);
            result
        })
        .collect();
    let result = if !results.is_empty() && results.iter().all(|r| r.status == Verdict::Passed) {
        Verdict::Passed
    } else {
        Verdict::Failed
    };
    let elapsed = started.elapsed();
    let receipt = Receipt {
        schema: RECEIPT_SCHEMA,
        kind: "gate",
        tier,
        set: "bootstrap",
        run_id: run_id.clone(),
        result,
        tree: ctx.tree.clone(),
        build_id,
        build_inputs,
        policy_hashes: policy_hashes(ctx.root),
        tools: Tools {
            cargo_runner: ctx.cargo.runner(),
            ubs: ubs_version,
        },
        started_at: started_at.rfc3339(),
        finished_at: Utc::now().rfc3339(),
        duration_ms: millis(elapsed),
        budget: Budget {
            target_ms: millis(TARGET),
            alert_ms: millis(ALERT),
            over_alert: elapsed > ALERT,
        },
        steps: results,
        step_log: StepLogSummary {
            path: steplog.path().map(|p| p.display().to_string()),
            records: steplog.written(),
            dropped: steplog.dropped(),
        },
    };
    let receipt_path = receipts.join(format!("gate-{run_id}.json"));
    let json = serde_json::to_string_pretty(&receipt)
        .map_err(|err| format!("cannot encode the receipt: {err}"))?;
    fs::write(&receipt_path, json + "\n")
        .map_err(|err| format!("cannot write {}: {err}", receipt_path.display()))?;
    Ok(GateReport {
        schema: SCHEMA,
        ok: result == Verdict::Passed,
        receipt_path: receipt_path.display().to_string(),
        receipt,
    })
}

fn log_step(steplog: &mut StepLog, step: Step, result: &StepResult) {
    let expected = format!("passes, examining at least one {}", result.unit);
    let rerun = match step {
        Step::Static(check) => format!("./dev check {}", check.id()),
        Step::Fmt | Step::Clippy | Step::Tests | Step::Ubs => {
            result.command.clone().unwrap_or_default()
        }
    };
    let observed = match result.status {
        Verdict::Passed => format!("passed: {} {}", result.cases, result.unit),
        Verdict::Failed => format!(
            "failed: {}{}",
            result.detail.as_deref().unwrap_or("no detail"),
            result
                .log
                .as_deref()
                .map_or_else(String::new, |log| format!(" (log: {log})"))
        ),
    };
    let record_step = steplog::Step {
        name: step.id(),
        action: result.command.as_deref().unwrap_or_else(|| step.id()),
        expected: &expected,
        rerun: &rerun,
    };
    let observed = match result.status {
        Verdict::Passed => Ok(observed),
        Verdict::Failed => Err(observed),
    };
    // The verdict lives in the receipt; this record is the step's log line.
    let _ = steplog.record(
        record_step,
        observed,
        Duration::from_millis(result.duration_ms),
    );
}

/// The tier's name as typed on the command line.
pub const fn tier_name(tier: GateTier) -> &'static str {
    match tier {
        GateTier::Fast => "fast",
        GateTier::Integration => "integration",
        GateTier::Campaign => "campaign",
        GateTier::Release => "release",
    }
}

fn run_step(
    ctx: &Context<'_>,
    step: Step,
    log_dir: &Path,
    ubs_version: &mut Option<String>,
) -> StepResult {
    let started = Instant::now();
    let mut result = StepResult {
        id: step.id(),
        status: Verdict::Failed,
        cases: 0,
        unit: "",
        duration_ms: 0,
        command: None,
        log: None,
        detail: None,
    };
    let log = log_dir.join(format!("{}.log", step.id()));
    let outcome = match step {
        Step::Fmt => fmt_step(ctx, &mut result),
        Step::Clippy => clippy_step(ctx, &mut result),
        Step::Static(check) => static_step(ctx, check, &mut result),
        Step::Tests => tests_step(ctx, &mut result),
        Step::Ubs => ubs_step(ctx, &mut result, ubs_version),
    };
    let (verdict, log_text) = match outcome {
        Ok(text) => (Verdict::Passed, text),
        Err(StepFailure { detail, log }) => {
            result.detail = Some(detail);
            (Verdict::Failed, log)
        }
    };
    result.status = if verdict == Verdict::Passed && result.cases == 0 {
        result.detail = Some(format!(
            "examined zero {}; a vacuous step is a failure (P10)",
            result.unit
        ));
        Verdict::Failed
    } else {
        verdict
    };
    if fs::write(&log, log_text).is_ok() {
        result.log = Some(log.display().to_string());
    }
    result.duration_ms = millis(started.elapsed());
    result
}

/// A failed step: a short reason and the full log text.
struct StepFailure {
    detail: String,
    log: String,
}

impl StepFailure {
    fn new(detail: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            log: log.into(),
        }
    }
}

type StepOutcome = Result<String, StepFailure>;

fn fmt_step(ctx: &Context<'_>, result: &mut StepResult) -> StepOutcome {
    let args = ["fmt", "--all", "--check"];
    // rustfmt builds nothing, so it always runs locally.
    let mut cmd = Command::new("cargo");
    cmd.args(args).current_dir(ctx.root);
    result.command = Some(format!("cargo {}", args.join(" ")));
    result.unit = "crate(s)";
    result.cases = member_count(ctx.root).map_err(|why| StepFailure::new(why, ""))?;
    let captured = run_captured(&mut cmd)?;
    expect_success(&captured, "files are not formatted; run `cargo fmt --all`")
}

fn clippy_step(ctx: &Context<'_>, result: &mut StepResult) -> StepOutcome {
    let args = [
        "clippy",
        "--locked",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--",
        "-D",
        "warnings",
    ];
    result.command = Some(ctx.cargo.describe(&args));
    result.unit = "crate(s)";
    result.cases = member_count(ctx.root).map_err(|why| StepFailure::new(why, ""))?;
    let captured = run_captured(&mut ctx.cargo.command(ctx.root, &args))?;
    expect_success(&captured, "clippy reported denied lints")
}

fn static_step(ctx: &Context<'_>, check: StaticCheck, result: &mut StepResult) -> StepOutcome {
    result.command = Some(format!("./dev check {}", check.id()));
    result.unit = check.unit();
    let outcome = check
        .run(ctx.root)
        .map_err(|why| StepFailure::new(why.clone(), why))?;
    result.cases = u64::try_from(outcome.cases).unwrap_or(u64::MAX);
    let log = outcome.violations.join("\n");
    if outcome.violations.is_empty() {
        Ok(format!(
            "{} {} examined; no violations\n",
            outcome.cases,
            check.unit()
        ))
    } else {
        let mut detail = format!("{} violation(s):", outcome.violations.len());
        for line in outcome.violations.iter().take(DETAIL_LINES) {
            let _ = write!(detail, "\n  {line}");
        }
        Err(StepFailure::new(detail, log))
    }
}

fn tests_step(ctx: &Context<'_>, result: &mut StepResult) -> StepOutcome {
    let args = ["test", "--locked", "--workspace"];
    result.command = Some(ctx.cargo.describe(&args));
    result.unit = "test(s)";
    let captured = run_captured(&mut ctx.cargo.command(ctx.root, &args))?;
    let log = combined(&captured);
    let tally = libtest::parse_tally(&captured.stdout)
        .map_err(|err| StepFailure::new(err.to_string(), log.clone()))?;
    result.cases = u64::from(tally.passed);
    if tally.failed > 0 {
        let names = libtest::failed_names(&captured.stdout).join(", ");
        return Err(StepFailure::new(
            format!("{} test(s) failed: {names}", tally.failed),
            log,
        ));
    }
    if tally.ignored > 0 {
        return Err(StepFailure::new(
            format!(
                "{} test(s) are #[ignore]d; an ignored case fails (P10)",
                tally.ignored
            ),
            log,
        ));
    }
    if !captured.status.success() {
        return Err(StepFailure::new("cargo test exited unsuccessfully", log));
    }
    Ok(log)
}

fn ubs_step(
    ctx: &Context<'_>,
    result: &mut StepResult,
    ubs_version: &mut Option<String>,
) -> StepOutcome {
    result.unit = "file(s)";
    let mut version = Command::new("ubs");
    version.arg("--version").current_dir(ctx.root);
    let version = run_captured(&mut version)?;
    *ubs_version = version.stdout.lines().next().map(str::to_owned);

    let files = ubs_scope(ctx).map_err(|why| StepFailure::new(why, ""))?;
    if files.is_empty() {
        return Err(StepFailure::new("no first-party source files to scan", ""));
    }
    let list = files.join(",");
    let mut cmd = Command::new("ubs");
    cmd.args([
        "--no-auto-update",
        "--no-color",
        "--format=json",
        &format!("--files={list}"),
        ".",
    ])
    .current_dir(ctx.root);
    result.command = Some(format!(
        "ubs --no-auto-update --format=json --files={list} ."
    ));
    let captured = run_captured(&mut cmd)?;
    let mut log = combined(&captured);
    result.cases = serde_json::from_str::<serde_json::Value>(&captured.stdout)
        .ok()
        .and_then(|report| report["totals"]["files"].as_u64())
        .unwrap_or(0);
    match ubs_verdict(&captured.stdout, captured.status.code()) {
        Ok(_) => Ok(log),
        Err(why) => {
            // UBS's Rust module emits no per-finding records in its machine formats, so
            // re-run once in text mode to put the exact locations in the log.
            let mut text = Command::new("ubs");
            text.args([
                "--no-auto-update",
                "--no-color",
                &format!("--files={list}"),
                ".",
            ])
            .current_dir(ctx.root);
            if let Ok(report) = process::run(&mut text, STEP_LIMIT) {
                log.push_str("\n--- ubs text report (for finding locations) ---\n");
                log.push_str(&report.stdout);
            }
            Err(StepFailure::new(
                format!("{why}; locations in the log"),
                log,
            ))
        }
    }
}

/// The files UBS scans: the changed first-party sources, or, when none changed, every
/// first-party source (never an empty scan, P10).
fn ubs_scope(ctx: &Context<'_>) -> Result<Vec<String>, String> {
    let scannable = |path: &str| {
        let first_party = !path.starts_with("xtask/probes/")
            && !path.contains("tests/fixtures/")
            && !path.starts_with("vendor/");
        first_party
            && (Path::new(path)
                .extension()
                .is_some_and(|e| e == "rs" || e == "sh")
                || path == "dev")
    };
    let changed: Vec<String> = ctx
        .tree
        .changed
        .iter()
        .filter(|path| scannable(path) && ctx.root.join(path).is_file())
        .cloned()
        .collect();
    if !changed.is_empty() {
        return Ok(changed);
    }
    let mut all: Vec<String> = lint::first_party_sources(ctx.root)?
        .iter()
        .filter_map(|path| path.strip_prefix(ctx.root).ok())
        .map(|path| path.display().to_string())
        .collect();
    if ctx.root.join("dev").is_file() {
        all.push("dev".to_owned());
    }
    Ok(all)
}

/// Judges UBS's JSON report: a complete scan (exit 0, status `ok`, no failed modules)
/// with zero critical findings. Returns the number of files scanned.
pub fn ubs_verdict(stdout: &str, exit_code: Option<i32>) -> Result<u64, String> {
    let report: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|err| format!("UBS output is not JSON (exit {exit_code:?}): {err}"))?;
    let critical = report["totals"]["critical"].as_u64();
    let files = report["totals"]["files"].as_u64().unwrap_or(0);
    let failed_modules = report["failed_modules"].as_array().map_or(0, Vec::len);
    if report["status"] != "ok" || failed_modules > 0 {
        return Err(format!(
            "UBS scan incomplete (status {}, {failed_modules} failed module(s)); a partial \
             scan is a failure (§11.1)",
            report["status"]
        ));
    }
    match (exit_code, critical) {
        (Some(0), Some(0)) => Ok(files),
        (_, Some(n)) if n > 0 => Err(format!("UBS found {n} critical issue(s)")),
        (code, _) => Err(format!(
            "UBS exited with {code:?} (2 = partial/refused scan, 3 = nothing scanned)"
        )),
    }
}

fn run_captured(cmd: &mut Command) -> Result<Captured, StepFailure> {
    process::run(cmd, STEP_LIMIT).map_err(|err| {
        let detail = format!("{}: {err}", cmd.get_program().to_string_lossy());
        StepFailure::new(detail.clone(), detail)
    })
}

fn expect_success(captured: &Captured, why: &str) -> StepOutcome {
    let log = combined(captured);
    if captured.status.success() {
        Ok(log)
    } else {
        Err(StepFailure::new(why, log))
    }
}

fn combined(captured: &Captured) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        captured.stdout, captured.stderr
    )
}

fn member_count(root: &Path) -> Result<u64, String> {
    let metadata = metadata::load(root)?;
    Ok(u64::try_from(metadata.members().len()).unwrap_or(u64::MAX))
}

fn build_inputs(root: &Path) -> Result<BuildInputs, String> {
    let output = |mut cmd: Command| -> Result<String, String> {
        cmd.current_dir(root);
        let shown = format!("{cmd:?}");
        let captured = process::run(&mut cmd, Duration::from_mins(1))
            .map_err(|err| format!("{shown}: {err}"))?;
        if captured.status.success() {
            Ok(captured.stdout.trim().to_owned())
        } else {
            Err(format!("{shown} exited with {}", captured.status))
        }
    };
    let mut rustc_cmd = Command::new("rustc");
    rustc_cmd.arg("-vV");
    let mut cargo_cmd = Command::new("cargo");
    cargo_cmd.arg("--version");
    let rustc = output(rustc_cmd)?;
    let target = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_owned();
    let lockfile = fs::read(root.join("Cargo.lock"))
        .map_err(|err| format!("cannot read Cargo.lock: {err}"))?;
    Ok(BuildInputs {
        cargo: output(cargo_cmd)?,
        rustc,
        target,
        lockfile_sha256: sha256_hex(&lockfile),
        features: "all",
        raft_patch_sha256: fs::read(root.join("third_party/raft.patch"))
            .ok()
            .map(|patch| sha256_hex(&patch)),
    })
}

/// `sha256:` of the tree ID and every build input, in a fixed, labelled order.
pub fn build_id(tree_id: &str, inputs: &BuildInputs) -> String {
    let canonical = format!(
        "keyval-build-id/v1\ntree {tree_id}\nrustc {}\ncargo {}\ntarget {}\nlockfile {}\n\
         features {}\nraft-patch {}\n",
        inputs.rustc,
        inputs.cargo,
        inputs.target,
        inputs.lockfile_sha256,
        inputs.features,
        inputs.raft_patch_sha256.as_deref().unwrap_or("none"),
    );
    format!("sha256:{}", sha256_hex(canonical.as_bytes()))
}

fn policy_hashes(root: &Path) -> BTreeMap<&'static str, String> {
    POLICY_FILES
        .iter()
        .map(|&file| {
            let hash = fs::read(root.join(file)).map_or_else(
                |_| "missing".to_owned(),
                |bytes| format!("sha256:{}", sha256_hex(&bytes)),
            );
            (file, hash)
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Report for GateReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let receipt = &self.receipt;
        let mut out = format!(
            "gate {} ({} set) on tree {} ({})\n",
            tier_name(receipt.tier),
            receipt.set,
            receipt.tree.id,
            match receipt.tree.source {
                TreeSource::GitHead => "clean checkout HEAD",
                TreeSource::Caller => "candidate, asserted by the caller",
            }
        );
        for step in &receipt.steps {
            let status = match step.status {
                Verdict::Passed => "passed",
                Verdict::Failed => "FAILED",
            };
            let seconds = Duration::from_millis(step.duration_ms).as_secs_f64();
            let _ = writeln!(
                out,
                "  {status:<7} {:<12} {:>5} {:<17} {seconds:>6.1}s",
                step.id, step.cases, step.unit
            );
            if let Some(detail) = &step.detail {
                for line in detail.lines() {
                    let _ = writeln!(out, "          {line}");
                }
            }
            if step.status == Verdict::Failed
                && let Some(log) = &step.log
            {
                let _ = writeln!(out, "          log: {log}");
            }
        }
        let total = Duration::from_millis(receipt.duration_ms).as_secs_f64();
        let _ = writeln!(
            out,
            "result: {} in {total:.1}s (target {}s, alert {}s{})",
            match receipt.result {
                Verdict::Passed => "passed",
                Verdict::Failed => "FAILED",
            },
            TARGET.as_secs(),
            ALERT.as_secs(),
            if receipt.budget.over_alert {
                "; OVER the alert threshold"
            } else {
                ""
            }
        );
        let _ = write!(out, "receipt: {}", self.receipt_path);
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-GATE
    use super::*;
    use crate::test_support::{nested_cargo, scratch_dir, workspace_root};

    const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn gate(fixture_name: &str, steps: &[Step]) -> GateReport {
        let root = fixture(fixture_name);
        let ctx = Context {
            root: &root,
            tree: resolve_tree(&root, Some(TREE), vec!["src/lib.rs".to_owned()]).unwrap(),
            artifacts: scratch_dir(&format!("gate-{fixture_name}")),
            cargo: nested_cargo("keyval-gate"),
            progress: false,
        };
        run(&ctx, GateTier::Fast, steps).unwrap()
    }

    fn step<'a>(report: &'a GateReport, id: &str) -> &'a StepResult {
        report
            .receipt
            .steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no step {id} in {report:#?}"))
    }

    #[test]
    fn a_clean_fixture_passes_and_the_receipt_names_the_tree() {
        let report = gate("gate-ok", &[Step::Fmt, Step::Clippy, Step::Tests]);
        assert!(report.ok, "observed={report:#?}");
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&report.receipt_path).unwrap()).unwrap();
        assert_eq!(written["schema"], RECEIPT_SCHEMA, "receipt={written:#}");
        assert_eq!(written["tree"]["id"], TREE, "receipt={written:#}");
        assert_eq!(written["tree"]["source"], "caller", "receipt={written:#}");
        assert_eq!(written["result"], "passed", "receipt={written:#}");
        assert!(
            written["build_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("sha256:")),
            "receipt={written:#}"
        );
        assert_eq!(step(&report, "tests").cases, 2, "observed={report:#?}");
        let receipt_path = Path::new(&report.receipt_path);
        let steps_log = receipt_path.with_extension("").join("steps.jsonl");
        let records: Vec<serde_json::Value> = fs::read_to_string(&steps_log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records.len(),
            3,
            "rule: one step record per step (W01.6); records={records:#?}"
        );
        assert_eq!(records[2]["step"], "tests");
        assert_eq!(records[2]["outcome"], "passed");
        assert_eq!(
            records[2]["duration_ms"],
            step(&report, "tests").duration_ms,
            "rule: the record carries the step's real duration"
        );
        for s in &report.receipt.steps {
            assert!(s.cases > 0 && s.log.is_some(), "step={s:#?}");
        }
    }

    #[test]
    fn an_unformatted_fixture_fails_the_format_step() {
        let report = gate("gate-bad", &[Step::Fmt]);
        assert!(!report.ok, "observed={report:#?}");
        assert_eq!(step(&report, "fmt").status, Verdict::Failed);
        let written = fs::read_to_string(&report.receipt_path).unwrap();
        assert!(
            written.contains("\"result\": \"failed\""),
            "rule: a failing run still writes its receipt; receipt={written}"
        );
    }

    #[test]
    fn a_workspace_with_no_tests_fails_the_test_step() {
        let report = gate("gate-bad", &[Step::Tests]);
        let tests = step(&report, "tests");
        assert_eq!(tests.status, Verdict::Failed, "observed={tests:#?}");
        assert!(
            tests.detail.as_deref().is_some_and(|d| d.contains("zero")),
            "rule: zero tests is never a pass (P10); observed={tests:#?}"
        );
    }

    #[test]
    fn an_empty_step_list_fails() {
        let report = gate("gate-ok", &[]);
        assert!(!report.ok, "rule: a gate that ran nothing has not passed");
    }

    #[test]
    fn ubs_verdicts() {
        let ok =
            r#"{"status":"ok","failed_modules":[],"totals":{"critical":0,"warning":3,"files":4}}"#;
        let critical = r#"{"status":"ok","failed_modules":[],"totals":{"critical":2,"files":4}}"#;
        let partial =
            r#"{"status":"partial","failed_modules":["rust"],"totals":{"critical":0,"files":4}}"#;
        assert_eq!(ubs_verdict(ok, Some(0)), Ok(4));
        let cases = [
            (critical, Some(1), "2 critical"),
            (partial, Some(2), "partial scan is a failure"),
            (ok, Some(3), "nothing scanned"),
            ("UBS crashed", Some(2), "not JSON"),
        ];
        for (stdout, code, expected) in cases {
            let observed = ubs_verdict(stdout, code);
            assert!(
                observed.as_ref().is_err_and(|e| e.contains(expected)),
                "stdout={stdout} code={code:?} expected error containing {expected:?}; \
                 observed={observed:?}"
            );
        }
    }

    #[test]
    fn the_build_id_covers_every_input() {
        let inputs = || BuildInputs {
            rustc: "rustc 1.99.0".to_owned(),
            cargo: "cargo 1.99.0".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            lockfile_sha256: "aa".to_owned(),
            features: "all",
            raft_patch_sha256: None,
        };
        let base = build_id(TREE, &inputs());
        assert_eq!(
            base,
            build_id(TREE, &inputs()),
            "rule: the build ID is deterministic"
        );
        let variants = [
            build_id("f".repeat(40).as_str(), &inputs()),
            build_id(
                TREE,
                &BuildInputs {
                    rustc: "rustc 1.100.0".to_owned(),
                    ..inputs()
                },
            ),
            build_id(
                TREE,
                &BuildInputs {
                    cargo: "cargo 1.100.0".to_owned(),
                    ..inputs()
                },
            ),
            build_id(
                TREE,
                &BuildInputs {
                    target: "aarch64".to_owned(),
                    ..inputs()
                },
            ),
            build_id(
                TREE,
                &BuildInputs {
                    lockfile_sha256: "bb".to_owned(),
                    ..inputs()
                },
            ),
            build_id(
                TREE,
                &BuildInputs {
                    raft_patch_sha256: Some("cc".to_owned()),
                    ..inputs()
                },
            ),
        ];
        for (index, variant) in variants.iter().enumerate() {
            assert_ne!(
                *variant, base,
                "rule: input #{index} must change the build ID"
            );
        }
    }

    #[test]
    fn asserted_tree_ids_are_validated() {
        let root = workspace_root();
        for bad in ["", "xyz", "0123", &"g".repeat(40)] {
            assert!(
                resolve_tree(&root, Some(bad), vec![]).is_err(),
                "tree={bad:?}"
            );
        }
        let tree = resolve_tree(&root, Some(&TREE.to_uppercase()), vec![]).unwrap();
        assert_eq!(tree.id, TREE, "IDs are normalised to lower case");
    }

    /// git for a fixture repository, immune to the caller's git environment: inside a
    /// git-isolated candidate (`GIT_DIR=/nonexistent`, §11.10 step 4) a fixture repo
    /// must still be its own repository.
    fn fixture_git(repo: &Path, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .map_err(|err| format!("git {args:?}: {err}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("git {args:?}: {out:?}"))
        }
    }

    #[test]
    fn a_dirty_checkout_is_refused_and_a_clean_one_uses_head() {
        let repo = scratch_dir("gate-git-repo");
        let git = |args: &[&str]| fixture_git(&repo, args).unwrap();
        git(&["init", "--quiet"]);
        fs::write(repo.join("a.rs"), "fn main() {}\n").unwrap();
        git(&["add", "a.rs"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        let resolved = resolve_tree_with(&repo, None, vec![], &fixture_git).unwrap();
        let head_tree = git(&["rev-parse", "HEAD^{tree}"]);
        assert_eq!(resolved.source, TreeSource::GitHead);
        assert_eq!(resolved.id, head_tree.trim());
        assert_eq!(resolved.changed, ["a.rs"]);

        fs::write(repo.join("b.rs"), "fn f() {}\n").unwrap();
        let refused = resolve_tree_with(&repo, None, vec![], &fixture_git);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("P11")),
            "rule: a dirty checkout never yields an exact-tree receipt; observed={refused:?}"
        );
    }

    #[test]
    fn the_bootstrap_set_matches_the_plan() {
        let ids: Vec<&str> = BOOTSTRAP_SET.iter().map(|s| s.id()).collect();
        assert_eq!(
            ids,
            [
                "fmt",
                "clippy",
                "LINT-ALLOWS",
                "LINT-TIERS",
                "DEP-RULE",
                "DEP-LEDGER",
                "tests",
                "ubs"
            ],
            "rule: §10.10 bootstrap set = format, lints, dependency rules, xtask tests, UBS"
        );
    }
}
