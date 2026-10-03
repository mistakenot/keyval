//! `./dev gate <tier>`: run a gate tier on an immutable tree and write a receipt
//! (PLAN.md §10.10, P7, P10, P11).
//!
//! Until the verification registry (W05) drives staging and W46 implements the other
//! tiers, `./dev gate fast` runs W01's **fixed bootstrap set** ([`BOOTSTRAP_SET`]):
//! format check, lints with denied warnings (clippy plus the LINT-ALLOWS and LINT-TIERS
//! policy checks), the dependency rule (DEP-RULE, CD-1), the dependency ledger
//! (DEP-LEDGER, CD-12), the workspace tests (xtask's included), and UBS on the exact
//! changed files. UBS's `panic!` rule is applied to production code only, because CD-8
//! lets tests panic ([`UBS_PRODUCTION_ONLY`]).
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
use std::ffi::{OsStr, OsString};
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
pub const SCHEMA: &str = "keyval.dev.gate/v2";

/// Schema identifier of a receipt file.
pub const RECEIPT_SCHEMA: &str = "keyval.dev.receipt/v2";

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
    /// The target directory the builds and tests used: this checkout's own
    /// (`cargo::checkout_target_dir`), so the tests that ran are this tree's build.
    /// `None` means cargo's default, `<root>/target`.
    pub cargo_target_dir: Option<String>,
    /// `ubs --version`, if UBS ran.
    pub ubs: Option<String>,
    /// The ast-grep UBS's rule packs ran on, if UBS ran.
    pub ast_grep: Option<AstGrep>,
}

/// The ast-grep that UBS used, and whether its rules provably ran
/// (br-gate-ubs-partial-scan-o59g).
#[derive(Clone, Debug, Serialize)]
pub struct AstGrep {
    /// Where `ast-grep` resolves on UBS's `PATH`; `None` when it is not there.
    pub path: Option<String>,
    /// First line of `ast-grep --version`, or why it could not be read.
    pub version: String,
    /// Whether UBS reported the canary's finding, which only its ast-grep rules detect.
    pub rules_ran: bool,
}

/// What the UBS step learnt about the scanner, for the receipt.
#[derive(Debug, Default)]
struct Scanner {
    /// `ubs --version`.
    ubs: Option<String>,
    /// The ast-grep it used.
    ast_grep: Option<AstGrep>,
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

/// A gate receipt (`keyval.dev.receipt/v2`): exactly what ran, on exactly which tree.
///
/// v2 adds `tools.cargo_target_dir` (br-gate-shared-target-dir-dtqu) and
/// `tools.ast_grep` (br-gate-ubs-partial-scan-o59g).
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

/// The `./dev gate` report (`keyval.dev.gate/v2`, which embeds a v2 receipt).
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
    /// The `PATH` that UBS, and the ast-grep it runs, see. `None` inherits this
    /// process's. Tests put stand-in tools first.
    pub tool_path: Option<OsString>,
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
    let mut scanner = Scanner::default();
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
            let result = run_step(ctx, step, &log_dir, &mut scanner);
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
            cargo_target_dir: ctx
                .cargo
                .target_dir()
                .map(|dir| PathBuf::from(dir).display().to_string()),
            ubs: scanner.ubs,
            ast_grep: scanner.ast_grep,
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

fn run_step(ctx: &Context<'_>, step: Step, log_dir: &Path, scanner: &mut Scanner) -> StepResult {
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
        Step::Ubs => ubs_step(ctx, log_dir, &mut result, scanner),
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

/// The UBS category that holds `rust.ownership.panic-macro`, the rule that rejects
/// `panic!`. CD-8 lets tests panic (owner decision on br-gate-ubs-test-panics-brzy), so
/// the gate judges this category on production code only: UBS 5.4's only critical rule
/// in it is that one, and its other rules (unwrap/expect, todo!, dbg! and the like) are
/// warnings or infos, which never fail the gate.
const UBS_PRODUCTION_ONLY: &str = "rust.ownership";

/// The UBS canary (br-gate-ubs-partial-scan-o59g): one production `panic!`, which only
/// UBS's ast-grep rule pack detects. UBS reports a scan whose ast-grep rules never ran
/// (ast-grep missing, failing, or exiting 0 without output) as complete and clean, so
/// every UBS step first scans this file and requires exactly this one critical finding.
const UBS_CANARY: &str = "\
//! UBS canary (br-gate-ubs-partial-scan-o59g): one production `panic!`, which only
//! UBS's ast-grep rule pack detects. A scan that does not report it ran no ast-grep rules.

/// Returns `value`, which must not be zero.
pub fn canary(value: u32) -> u32 {
    if value == 0 {
        panic!(\"canary\");
    }
    value
}
";

/// Wall-clock limit for `ast-grep --version`.
const AST_GREP_PROBE_LIMIT: Duration = Duration::from_secs(30);

/// Runs UBS three times, every run uncached. First the canary ([`UBS_CANARY`]) proves
/// that UBS's ast-grep rules run; if it fails, so does the step. Pass 1 then scans the exact changed files with every
/// rule except the [`UBS_PRODUCTION_ONLY`] category. Pass 2 runs every rule on a
/// production view of the Rust files, with their test code blanked (see
/// `lint::test_code_ranges`). The canary and the views are kept under the step's log
/// directory. Each run is judged by its JSON report; its text report goes to the log,
/// so the log shows every rule that ran and where any finding is.
fn ubs_step(
    ctx: &Context<'_>,
    log_dir: &Path,
    result: &mut StepResult,
    scanner: &mut Scanner,
) -> StepOutcome {
    result.unit = "file(s)";
    let tool_path = ctx
        .tool_path
        .clone()
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let mut version = Command::new("ubs");
    version
        .arg("--version")
        .env("PATH", &tool_path)
        .current_dir(ctx.root);
    let version = run_captured(&mut version)?;
    scanner.ubs = version.stdout.lines().next().map(str::to_owned);
    let files = ubs_scope(ctx).map_err(|why| StepFailure::new(why, ""))?;
    if files.is_empty() {
        return Err(StepFailure::new("no first-party source files to scan", ""));
    }
    let canary = ubs_canary(log_dir, &tool_path)?;
    let mut log = canary.log;
    let mut commands = vec![canary.command];
    let rules_ran = canary.verdict.is_ok();
    let ast_grep = scanner.ast_grep.insert(AstGrep {
        rules_ran,
        ..probe_ast_grep(&tool_path)
    });
    let shown = format!(
        "ast-grep on UBS's PATH: {} ({})",
        ast_grep.path.as_deref().unwrap_or("not found"),
        ast_grep.version
    );
    log.insert_str(0, &format!("{shown}\n"));
    if let Err(why) = canary.verdict {
        // Without its ast-grep rules UBS cannot judge the real files either, so the
        // step fails here rather than spend two more scans on untrustworthy results.
        log.push_str("\n=== passes 1 and 2: not run, the canary failed ===\n");
        result.command = Some(commands.join("; "));
        return Err(StepFailure::new(format!("canary: {why}; {shown}"), log));
    }

    let mut failures = Vec::new();
    let skip = format!("--skip={UBS_PRODUCTION_ONLY}");
    let exact = ubs_pass(
        "pass 1: the exact files, all rules except the production-only category",
        ctx.root,
        &files,
        &[&skip],
        &tool_path,
        (&log_dir.join("ubs-exact.json"), ubs_verdict),
    )?;
    result.cases = exact.files;
    log.push_str(&exact.log);
    commands.push(exact.command);
    if let Err(why) = exact.verdict {
        failures.push(format!("pass 1 (exact files): {why}"));
    }
    let view_dir = log_dir.join("ubs-production");
    let production = production_views(ctx.root, &files, &view_dir)
        .map_err(|why| StepFailure::new(why, log.clone()))?;
    if production.is_empty() {
        log.push_str("\n=== pass 2: not run, no scanned Rust file holds production code ===\n");
    } else {
        let pass = ubs_pass(
            "pass 2: the production view (test code blanked), all rules",
            &view_dir,
            &production,
            &[],
            &tool_path,
            (&log_dir.join("ubs-production.json"), ubs_verdict),
        )?;
        log.push_str(&pass.log);
        commands.push(format!("(cd {} && {})", view_dir.display(), pass.command));
        if let Err(why) = pass.verdict {
            failures.push(format!(
                "pass 2 (production code): {why}; test code counts only inside \
                 #[test]/#[cfg(test)] items and #![cfg(test)] files (CD-8)"
            ));
        }
    }
    result.command = Some(commands.join("; "));
    if failures.is_empty() {
        Ok(log)
    } else {
        Err(StepFailure::new(
            format!("{}; locations in the log", failures.join("; ")),
            log,
        ))
    }
}

/// Scans the canary ([`UBS_CANARY`]), written under `log_dir`, with every rule.
fn ubs_canary(log_dir: &Path, tool_path: &OsStr) -> Result<UbsPass, StepFailure> {
    let dir = log_dir.join("ubs-canary");
    let file = "src/canary.rs";
    fs::create_dir_all(dir.join("src"))
        .and_then(|()| fs::write(dir.join(file), UBS_CANARY))
        .map_err(|err| StepFailure::new(format!("cannot write the UBS canary: {err}"), ""))?;
    let mut pass = ubs_pass(
        "canary: one production panic! that only UBS's ast-grep rules detect",
        &dir,
        &[file.to_owned()],
        &[],
        tool_path,
        (&log_dir.join("ubs-canary.json"), canary_verdict),
    )?;
    pass.command = format!("(cd {} && {})", dir.display(), pass.command);
    Ok(pass)
}

/// The ast-grep that UBS will find on `tool_path` (UBS looks it up on `PATH`), and its
/// version. Whether its rules ran is for the canary to show.
fn probe_ast_grep(tool_path: &OsStr) -> AstGrep {
    let Some(path) = process::find_in(tool_path, "ast-grep") else {
        return AstGrep {
            path: None,
            version: "not found".to_owned(),
            rules_ran: false,
        };
    };
    // A fixed program name, looked up on the child's `PATH`: the one UBS will see.
    let mut cmd = Command::new("ast-grep");
    cmd.arg("--version").env("PATH", tool_path);
    let version = match process::run(&mut cmd, AST_GREP_PROBE_LIMIT) {
        Ok(out) if out.status.success() => out
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(|| "printed no version".to_owned(), str::to_owned),
        Ok(out) => format!("`ast-grep --version` exited with {}", out.status),
        Err(err) => format!("`ast-grep --version`: {err}"),
    };
    AstGrep {
        path: Some(path.display().to_string()),
        version,
        rules_ran: false,
    }
}

/// One UBS run and how it was judged.
struct UbsPass {
    /// The command line, relative to the directory it ran in.
    command: String,
    /// Files UBS reports it scanned.
    files: u64,
    /// The judge's verdict on its JSON report.
    verdict: Result<u64, String>,
    /// Its text report and stderr, for the step log.
    log: String,
}

/// Judges a UBS JSON report, its exit code and the number of files it was given.
type UbsJudge = fn(&str, Option<i32>, u64) -> Result<u64, String>;

/// Runs UBS in text mode on `files` (relative to `dir`) with `tool_path` as its `PATH`,
/// writing its JSON report to `report.0`, which `report.1` judges (§11.5 rule 14: the
/// text is only logged).
fn ubs_pass(
    title: &str,
    dir: &Path,
    files: &[String],
    extra: &[&str],
    tool_path: &OsStr,
    (report, judge): (&Path, UbsJudge),
) -> Result<UbsPass, StepFailure> {
    let files_arg = format!("--files={}", files.join(","));
    let report_arg = format!("--report-json={}", report.display());
    // `--no-cache`: UBS's scan cache is keyed by file content, not by the ast-grep that
    // produced an entry, so a hit can replay a scan whose rules never ran
    // (br-gate-ubs-partial-scan-o59g). It costs nothing measurable here.
    let mut args = vec!["--no-auto-update", "--no-color", "--no-cache"];
    args.extend_from_slice(extra);
    args.extend([report_arg.as_str(), files_arg.as_str(), "."]);
    let command = format!("ubs {}", args.join(" "));
    // A stale report from an earlier run must never judge this one.
    let _ = fs::remove_file(report);
    let mut cmd = Command::new("ubs");
    cmd.args(&args)
        .env("PATH", tool_path)
        .env("NO_COLOR", "1")
        .current_dir(dir);
    let captured = run_captured(&mut cmd)?;
    let json = fs::read_to_string(report).unwrap_or_default();
    let scanned = serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|value| value["totals"]["files"].as_u64())
        .unwrap_or(0);
    let given = u64::try_from(files.len()).unwrap_or(u64::MAX);
    let verdict = judge(&json, captured.status.code(), given);
    let log = format!(
        "\n=== {title} ===\n$ {command}\n(exit {:?}; JSON report {})\n{}",
        captured.status.code(),
        report.display(),
        combined(&captured)
    );
    Ok(UbsPass {
        command,
        files: scanned,
        verdict,
        log,
    })
}

/// Writes the production view of each scanned Rust file that has production code to
/// the same relative path under `dir`, and returns those paths.
fn production_views(root: &Path, files: &[String], dir: &Path) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    let rust = |file: &&String| Path::new(file).extension().is_some_and(|e| e == "rs");
    for file in files.iter().filter(rust) {
        let text = fs::read_to_string(root.join(file))
            .map_err(|err| format!("cannot read {file}: {err}"))?;
        let view = lint::production_view(&text);
        if view.trim().is_empty() {
            continue;
        }
        let target = dir.join(file);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        fs::write(&target, view)
            .map_err(|err| format!("cannot write {}: {err}", target.display()))?;
        written.push(file.clone());
    }
    Ok(written)
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

/// Judges UBS's JSON report: a complete scan (exit 0, status `ok`, no failed modules,
/// every one of the `given` files scanned) with zero critical findings. Returns the
/// number of files scanned.
pub fn ubs_verdict(stdout: &str, exit_code: Option<i32>, given: u64) -> Result<u64, String> {
    let report: serde_json::Value = serde_json::from_str(stdout).map_err(|err| {
        format!("UBS's report is missing or not JSON (exit {exit_code:?}): {err}")
    })?;
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
    if files != given {
        return Err(format!(
            "UBS scanned {files} of the {given} file(s) it was given; a partial scan is a \
             failure (§11.1)"
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

/// Judges the canary scan ([`UBS_CANARY`]): a complete scan of the one given file that
/// reports exactly the canary's critical finding, and exits 1 for it. A clean canary
/// means UBS's ast-grep rules did not run. Returns the number of files scanned.
pub fn canary_verdict(stdout: &str, exit_code: Option<i32>, given: u64) -> Result<u64, String> {
    let report: serde_json::Value = serde_json::from_str(stdout).map_err(|err| {
        format!("UBS's canary report is missing or not JSON (exit {exit_code:?}): {err}")
    })?;
    let critical = report["totals"]["critical"].as_u64();
    let files = report["totals"]["files"].as_u64().unwrap_or(0);
    let failed_modules = report["failed_modules"].as_array().map_or(0, Vec::len);
    if report["status"] != "ok" || failed_modules > 0 || files != given {
        return Err(format!(
            "UBS's canary scan is incomplete (status {}, {failed_modules} failed module(s), \
             {files} of {given} file(s)); a partial scan is a failure (§11.1)",
            report["status"]
        ));
    }
    match (exit_code, critical) {
        (Some(1), Some(1)) => Ok(files),
        (_, Some(0)) => Err(
            "UBS did not report the canary's panic!, so its ast-grep rules did not run \
             (ast-grep missing, broken or silent); a partial scan is a failure (§11.1)"
                .to_owned(),
        ),
        (code, found) => Err(format!(
            "the canary expects exactly 1 critical finding and exit 1, but UBS reported \
             {found:?} with exit {code:?}; re-check UBS_CANARY against the installed UBS"
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
        crate::test_support::manifest_dir()
            .join("tests/fixtures")
            .join(name)
    }

    fn gate(fixture_name: &str, steps: &[Step]) -> GateReport {
        gate_changed(fixture_name, &["src/lib.rs"], steps)
    }

    fn gate_changed(fixture_name: &str, changed: &[&str], steps: &[Step]) -> GateReport {
        gate_with_path(fixture_name, changed, steps, None)
    }

    fn gate_with_path(
        fixture_name: &str,
        changed: &[&str],
        steps: &[Step],
        tool_path: Option<OsString>,
    ) -> GateReport {
        let root = fixture(fixture_name);
        let changed = changed.iter().map(|&path| path.to_owned()).collect();
        let ctx = Context {
            root: &root,
            tree: resolve_tree(&root, Some(TREE), changed).unwrap(),
            artifacts: scratch_dir(&format!("gate-{fixture_name}")),
            cargo: nested_cargo("keyval-gate"),
            tool_path,
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
            written["tools"]["cargo_target_dir"]
                .as_str()
                .is_some_and(|dir| dir.ends_with("keyval-gate")),
            "rule: the receipt records the target dir the run built into \
             (br-gate-shared-target-dir-dtqu); receipt={written:#}"
        );
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
        assert_eq!(ubs_verdict(ok, Some(0), 4), Ok(4));
        let cases = [
            (critical, Some(1), 4, "2 critical"),
            (partial, Some(2), 4, "partial scan is a failure"),
            (ok, Some(3), 4, "nothing scanned"),
            ("UBS crashed", Some(2), 4, "not JSON"),
            (ok, Some(0), 5, "scanned 4 of the 5 file(s)"),
        ];
        for (stdout, code, given, expected) in cases {
            let observed = ubs_verdict(stdout, code, given);
            assert!(
                observed.as_ref().is_err_and(|e| e.contains(expected)),
                "stdout={stdout} code={code:?} given={given} expected error containing \
                 {expected:?}; observed={observed:?}"
            );
        }
    }

    /// The probe for br-gate-ubs-test-panics-brzy, with the real UBS: test code may
    /// panic (CD-8), production code may not. The fixture panics in a `#[cfg(test)]`
    /// module (src/lib.rs:18), in a `#![cfg(test)]` file (tests/doubling.rs:6) and in
    /// production code (src/halve.rs:7); only the last one may fail the gate.
    #[test]
    fn ubs_lets_tests_panic_but_never_production_code() {
        let changed = ["src/lib.rs", "tests/doubling.rs", "src/halve.rs"];
        let report = gate_changed("gate-ubs", &changed, &[Step::Ubs]);
        let ubs = step(&report, "ubs");
        let log = fs::read_to_string(ubs.log.as_deref().unwrap_or_default()).unwrap_or_default();
        let detail = ubs.detail.as_deref().unwrap_or_default();
        assert!(
            ubs.status == Verdict::Failed && ubs.cases == 3,
            "rule: the step scans every changed file and fails; observed={ubs:#?}"
        );
        assert!(
            detail.contains("pass 2 (production code): UBS found 1 critical issue(s)")
                && !detail.contains("pass 1"),
            "rule: only the production panic! fails the gate (CD-8); detail={detail}\n\
             log:\n{log}"
        );
        assert!(
            log.contains("src/halve.rs:7:9 [rule:rust.ownership.panic-macro]")
                && !log.contains("src/lib.rs:18")
                && !log.contains("tests/doubling.rs:6"),
            "rule: the log locates the production panic! and no test panic!; log:\n{log}"
        );
        let ast_grep = report.receipt.tools.ast_grep.as_ref();
        assert!(
            ast_grep.is_some_and(|a| a.rules_ran && a.path.is_some()),
            "rule: the receipt records the ast-grep UBS used and that its rules ran \
             (br-gate-ubs-partial-scan-o59g); observed={ast_grep:?}"
        );
    }

    /// The fixture for br-gate-ubs-partial-scan-o59g, with the real UBS. UBS reports a
    /// scan whose ast-grep rules never ran as complete and clean; the canary must catch
    /// it, both when `ast-grep` fails and when it exits 0 having done nothing. (The
    /// self-spawning stub of the incident is never run here: it is a fork bomb, and is
    /// only ever exercised by hand under a hard task cap.)
    #[test]
    fn ubs_fails_when_its_ast_grep_rules_do_not_run() {
        let stubs = scratch_dir("ast-grep-stubs");
        for (name, body, version) in [
            (
                "failing",
                "echo 'stub ast-grep: broken' >&2\nexit 1",
                "`ast-grep --version` exited with exit status: 1",
            ),
            ("silent", "exit 0", "printed no version"),
        ] {
            let bin = stubs.join(name);
            fs::create_dir_all(&bin).unwrap();
            let stub = bin.join("ast-grep");
            fs::write(&stub, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let mut tool_path = OsString::from(&bin);
            tool_path.push(":");
            tool_path.push(std::env::var_os("PATH").unwrap_or_default());
            let report =
                gate_with_path("gate-ubs", &["src/halve.rs"], &[Step::Ubs], Some(tool_path));
            let ubs = step(&report, "ubs");
            let detail = ubs.detail.as_deref().unwrap_or_default();
            assert!(
                ubs.status == Verdict::Failed
                    && detail.contains("canary")
                    && detail.contains("ast-grep rules did not run"),
                "rule: a UBS scan whose ast-grep rules never ran fails the gate (§11.1); \
                 stub={name} observed={ubs:#?}"
            );
            let ast_grep = report.receipt.tools.ast_grep.as_ref();
            assert!(
                ast_grep.is_some_and(|a| {
                    !a.rules_ran
                        && a.path.as_deref() == Some(stub.display().to_string().as_str())
                        && a.version == version
                }),
                "rule: the receipt names the ast-grep UBS used and that its rules did not \
                 run; stub={name} observed={ast_grep:?}"
            );
        }
    }

    #[test]
    fn canary_verdicts() {
        let report = |status: &str, critical: u64, files: u64| {
            format!(
                r#"{{"status":"{status}","failed_modules":[],"totals":{{"critical":{critical},"files":{files}}}}}"#
            )
        };
        assert_eq!(canary_verdict(&report("ok", 1, 1), Some(1), 1), Ok(1));
        let cases = [
            (report("ok", 0, 1), Some(0), "ast-grep rules did not run"),
            (
                report("partial", 1, 1),
                Some(2),
                "partial scan is a failure",
            ),
            (report("ok", 1, 0), Some(1), "0 of 1 file(s)"),
            (report("ok", 2, 1), Some(1), "expects exactly 1 critical"),
            (report("ok", 1, 1), Some(0), "expects exactly 1 critical"),
            (String::new(), Some(2), "not JSON"),
        ];
        for (stdout, code, expected) in cases {
            let observed = canary_verdict(&stdout, code, 1);
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
