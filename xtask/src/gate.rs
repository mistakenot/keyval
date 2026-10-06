//! `./dev gate <tier>`: run a gate tier on an immutable tree and write a receipt
//! (PLAN.md §10.10, P7, P10, P11).
//!
//! **Registry staging (W05.4).** Once the tree has `registry/checks.toml`, `./dev gate
//! fast` runs the **registry set** ([`run_fast`]):
//! - the format check and clippy with denied warnings;
//! - staging: the T0 rows of `checks.toml` this tier runs and reports ([`crate::staging`]);
//! - the forward-only status rule against the parent commit's `checks.toml` (D-56);
//! - one test run per workspace member, plus the workspace's doctests;
//! - every `implemented` T0 row, in file order: a static check runs in-process, and a
//!   cargo-test check is judged from its package's run, never re-run;
//! - UBS on the exact changed files.
//!
//! `planned` T0 rows are reported in the receipt with their work item and never fail
//! (§10.10). A tree without the registry gets W01's **fixed bootstrap set**
//! ([`BOOTSTRAP_SET`]): format check, lints with denied warnings (clippy plus the
//! LINT-ALLOWS and LINT-TIERS policy checks), the dependency rule (DEP-RULE, CD-1), the
//! dependency ledger (DEP-LEDGER, CD-12), the workspace tests, and UBS. UBS's `panic!`
//! rule is applied to production code only, because CD-8 lets tests panic
//! ([`UBS_PRODUCTION_ONLY`]).
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

use crate::candidate;
use crate::cargo::{Cargo, Runner};
use crate::check::{CATALOGUE, CheckDef, Selector, StaticCheck};
use crate::clock::Utc;
use crate::libtest;
use crate::lint;
use crate::metadata;
use crate::output::Report;
use crate::process::{self, Captured};
use crate::registry;
use crate::registry_gate;
use crate::staging::{self, Allowance, Demotion, PackageRun, Planned, Staging};
use crate::steplog::{self, StepLog};
use crate::workgraph;

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.gate/v5";

/// Schema identifier of a receipt file.
pub const RECEIPT_SCHEMA: &str = "keyval.dev.receipt/v5";

/// T0 time budget: target and alert threshold (§10.10). A run is never cut off.
const TARGET: Duration = Duration::from_mins(2);
const ALERT: Duration = Duration::from_mins(3);

/// Wall-clock limit for one step, including compilation.
const STEP_LIMIT: Duration = Duration::from_mins(30);

/// Most violation lines quoted in a step's `detail` (the full list is in its log).
const DETAIL_LINES: usize = 10;

/// The committed tracker export, which approves demotions (`--allow-demotion`).
const TRACKER_EXPORT: &str = ".beads/issues.jsonl";

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
    /// Registry staging: the `checks.toml` rows this tier runs and reports (§10.10).
    Staging,
    /// Declared check statuses never move backwards against the parent commit's
    /// `checks.toml` (D-56).
    ForwardOnly,
    /// `cargo test -p <member> --all-targets --no-fail-fast` for each workspace member,
    /// then the workspace's doctests: the one test run every cargo-test check is judged
    /// from.
    PackageTests,
    /// A registered check. A static one runs in-process; a cargo-test one is judged from
    /// [`Step::PackageTests`].
    Check {
        /// The catalogue ID.
        id: &'static str,
        /// The `checks.toml` row's `min_cases`.
        min_cases: u32,
    },
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
            Self::Tests | Self::PackageTests => "tests",
            Self::Ubs => "ubs",
            Self::Staging => "staging",
            Self::ForwardOnly => "forward-only",
            Self::Check { id, .. } => id,
        }
    }
}

/// The parent commit's `registry/checks.toml`, which the forward-only rule compares
/// against (D-56).
#[derive(Clone, Debug)]
pub enum Parent {
    /// Not known: candidate mode without `--parent-checks`. The rule fails rather than
    /// skip (P10).
    Unknown,
    /// The parent commit has no `registry/checks.toml`, or there is no parent: every row
    /// is new.
    Absent,
    /// The parent commit's file.
    Checks(String),
}

impl Parent {
    /// `--parent-checks <file>`, as `./dev commit` writes it: the parent's
    /// `registry/checks.toml`, or an empty file when the parent has none.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|err| format!("cannot read --parent-checks {}: {err}", path.display()))?;
        Ok(if text.trim().is_empty() {
            Self::Absent
        } else {
            Self::Checks(text)
        })
    }

    /// The parent of `HEAD` in a clean checkout (the gate's git mode).
    pub fn of_head(root: &Path) -> Result<Self, String> {
        if git(root, &["rev-parse", "--verify", "--quiet", "HEAD^"]).is_err() {
            return Ok(Self::Absent);
        }
        let spec = format!("HEAD^:{}", registry_gate::CHECKS);
        if git(root, &["cat-file", "-e", &spec]).is_err() {
            return Ok(Self::Absent);
        }
        git(root, &["show", &spec]).map(Self::Checks)
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
/// lockfile, features and Raft patch; T-3: generated-code provenance).
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
    /// SHA-256 of `vendor/raft/VENDOR.toml`: the vendored Raft library's per-file hashes
    /// and its pinned code generation (protoc, generator and runtime versions; W02.1).
    pub raft_vendor_sha256: Option<String>,
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

/// A gate receipt (`keyval.dev.receipt/v5`): exactly what ran, on exactly which tree. v5
/// names the field of each approved demotion (xo5i).
///
/// v2 adds `tools.cargo_target_dir` (br-gate-shared-target-dir-dtqu) and
/// `tools.ast_grep` (br-gate-ubs-partial-scan-o59g); v4 adds `planned` and `demotions`,
/// and `set` becomes `registry` once the tree has the registry (W05.4).
#[derive(Debug, Serialize)]
pub struct Receipt {
    /// Schema identifier.
    pub schema: &'static str,
    /// Always `gate` for gate runs.
    pub kind: &'static str,
    /// Gate tier.
    pub tier: GateTier,
    /// Which check set ran: `registry` (W05.4), or `bootstrap` for a tree without
    /// `registry/checks.toml`.
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
    /// The tier's `planned` rows, reported and never failed (§10.10).
    pub planned: Vec<Planned>,
    /// The demotions an owner-approved plan change covered (D-56).
    pub demotions: Vec<Demotion>,
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

/// The `./dev gate` report (`keyval.dev.gate/v5`, which embeds a v5 receipt).
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
    /// The parent commit's registry, for the forward-only rule.
    pub parent: Parent,
    /// `--allow-demotion <CHECK-ID>=<bead>` values (D-56).
    pub allow_demotions: Vec<String>,
}

/// Determines the tree to judge. With `asserted`, the caller vouches for the ID, git is
/// never asked for it, and `root` must be git-isolated; otherwise `root` must be a clean
/// git checkout.
pub fn resolve_tree(
    root: &Path,
    asserted: Option<&str>,
    changed: Vec<String>,
) -> Result<Tree, String> {
    resolve_tree_with(root, asserted, changed, &git)
}

/// Runs git with `args` in a directory and returns its stdout.
type GitRunner<'a> = dyn Fn(&Path, &[&str]) -> Result<String, String> + 'a;

fn resolve_tree_with(
    root: &Path,
    asserted: Option<&str>,
    changed: Vec<String>,
    git: &GitRunner<'_>,
) -> Result<Tree, String> {
    if let Some(id) = asserted {
        let valid = matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(format!("--tree {id:?} is not a git tree object ID"));
        }
        // Candidate mode: the tree ID is the caller's record, and git is never asked for it,
        // so the build ID derives from the recorded tree (§11.10 step 4). The one git call
        // is a probe that must fail: if git can see any repository from here, the run is
        // not isolated, and something in it could judge the shared checkout instead of
        // this tree (P11).
        if let Ok(found) = git(root, &["rev-parse", "--absolute-git-dir"]) {
            return Err(format!(
                "candidate mode (--tree) needs a git-isolated tree, but git sees the repository \
                 {} from {}. Run it with GIT_DIR={} and GIT_CEILING_DIRECTORIES covering the \
                 candidate and its parent, as ./dev commit does (§11.10 step 4)",
                found.trim(),
                root.display(),
                candidate::ISOLATED_GIT_DIR
            ));
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
    run_with(ctx, tier, steps, "bootstrap", None, CATALOGUE)
}

/// `./dev gate fast`: the registry set once the tree or its parent commit has
/// `registry/checks.toml`, and W01's bootstrap set before (§10.10 "Staged contents").
pub fn run_fast(ctx: &Context<'_>) -> Result<GateReport, String> {
    let tree_has_registry = ctx.root.join(registry_gate::CHECKS).is_file();
    if registry_set_applies(tree_has_registry, &ctx.parent) {
        run_registry(ctx, CATALOGUE, &[Step::Fmt, Step::Clippy], &[Step::Ubs])
    } else {
        run(ctx, GateTier::Fast, BOOTSTRAP_SET)
    }
}

/// Whether the registry set judges the tree. The switch from W01's bootstrap set is
/// one-way (§10.10: "registry-driven staging takes over when W05 lands";
/// br-gate-bootstrap-fallback-0z0a): a tree that drops the registry its parent commit had
/// is still judged by the registry set, whose staging cannot read it and whose
/// forward-only rule reports every parent row removed. With the parent unknown (candidate
/// mode without `--parent-checks`) the registry set runs too, so the forward-only rule
/// fails rather than skips (P10). Only when the parent has no registry does the tree
/// decide.
const fn registry_set_applies(tree_has_registry: bool, parent: &Parent) -> bool {
    match parent {
        Parent::Absent => tree_has_registry,
        Parent::Unknown | Parent::Checks(_) => true,
    }
}

/// The registry set: `before`, then staging, the forward-only rule and the package test
/// runs, then each `implemented` T0 row of `checks.toml` in order, then `after`. Rows
/// resolve against `catalogue`.
pub fn run_registry(
    ctx: &Context<'_>,
    catalogue: &'static [CheckDef],
    before: &[Step],
    after: &[Step],
) -> Result<GateReport, String> {
    let present = ctx.root.join(registry_gate::CHECKS).is_file();
    let missing = || {
        format!(
            "the tree has no {} although its parent commit's registry (or an unknown parent) \
             requires one: registry staging never falls back to the bootstrap set once W05 \
             has landed (§10.10)",
            registry_gate::CHECKS
        )
    };
    let checks = if present {
        read(ctx.root, registry_gate::CHECKS)
    } else {
        Err(missing())
    };
    let staging = checks.and_then(|checks| {
        // An unreadable work graph only costs planned rows their work item; WORKGRAPH and
        // REGISTRY fail on it.
        let items = read(ctx.root, registry::WORKGRAPH)
            .and_then(|text| workgraph::parse(&text))
            .unwrap_or_default();
        staging::stage(&checks, &items, "T0", catalogue)
    });
    let mut steps = before.to_vec();
    steps.extend([Step::Staging, Step::ForwardOnly, Step::PackageTests]);
    if let Ok(staged) = &staging {
        steps.extend(staged.run.iter().map(|s| Step::Check {
            id: s.def.id,
            min_cases: s.min_cases,
        }));
    }
    steps.extend_from_slice(after);
    run_with(
        ctx,
        GateTier::Fast,
        &steps,
        "registry",
        Some(&staging),
        catalogue,
    )
}

fn read(root: &Path, name: &str) -> Result<String, String> {
    fs::read_to_string(root.join(name)).map_err(|err| format!("cannot read {name}: {err}"))
}

/// What a run carries from step to step.
struct State<'a> {
    /// What the UBS step learnt about the scanner.
    scanner: Scanner,
    /// The checks `Step::Check` resolves against.
    catalogue: &'static [CheckDef],
    /// The registry staging, in a registry run.
    staging: Option<&'a Result<Staging, String>>,
    /// The package test runs, once `Step::PackageTests` ran.
    runs: Vec<PackageRun>,
    /// Demotions the forward-only step approved.
    approved: Vec<Demotion>,
}

fn run_with(
    ctx: &Context<'_>,
    tier: GateTier,
    steps: &[Step],
    set: &'static str,
    staging: Option<&Result<Staging, String>>,
    catalogue: &'static [CheckDef],
) -> Result<GateReport, String> {
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
    let mut state = State {
        scanner: Scanner::default(),
        catalogue,
        staging,
        runs: Vec::new(),
        approved: Vec::new(),
    };
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
            let result = run_step(ctx, step, &log_dir, &mut state);
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
        set,
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
            ubs: state.scanner.ubs,
            ast_grep: state.scanner.ast_grep,
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
        planned: match staging {
            Some(Ok(staged)) => staged.planned.clone(),
            Some(Err(_)) | None => Vec::new(),
        },
        demotions: state.approved,
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
        Step::Check { id, .. } => format!("./dev check {id}"),
        Step::Staging | Step::ForwardOnly => "./dev gate fast".to_owned(),
        Step::Fmt | Step::Clippy | Step::Tests | Step::PackageTests | Step::Ubs => {
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

fn run_step(ctx: &Context<'_>, step: Step, log_dir: &Path, state: &mut State<'_>) -> StepResult {
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
        Step::Ubs => ubs_step(ctx, log_dir, &mut result, &mut state.scanner),
        Step::Staging => staging_step(state, &mut result),
        Step::ForwardOnly => forward_step(ctx, state, &mut result),
        Step::PackageTests => package_tests_step(ctx, state, &mut result),
        Step::Check { id, min_cases } => check_step(ctx, state, id, min_cases, &mut result),
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

/// A failure whose detail quotes the first [`DETAIL_LINES`] of `lines` and whose log holds
/// them all.
fn violations_failure(headline: &str, lines: &[String]) -> StepFailure {
    let mut detail = headline.to_owned();
    for line in lines.iter().take(DETAIL_LINES) {
        let _ = write!(detail, "\n  {line}");
    }
    StepFailure::new(detail, lines.join("\n"))
}

fn staging_step(state: &State<'_>, result: &mut StepResult) -> StepOutcome {
    result.unit = "row(s)";
    let staged = match state.staging {
        Some(Ok(staged)) => staged,
        Some(Err(why)) => return Err(StepFailure::new(why.clone(), why.clone())),
        None => {
            let why = "a staging step runs only in the registry set";
            return Err(StepFailure::new(why, why));
        }
    };
    result.cases = u64::try_from(staged.rows).unwrap_or(u64::MAX);
    if !staged.problems.is_empty() {
        return Err(violations_failure(
            &format!("{} T0 row(s) cannot be staged:", staged.problems.len()),
            &staged.problems,
        ));
    }
    let mut log = format!(
        "{} T0 row(s): {} run, {} planned\n",
        staged.rows,
        staged.run.len(),
        staged.planned.len()
    );
    for planned in &staged.planned {
        let _ = writeln!(
            log,
            "planned {} (work item {})",
            planned.id,
            planned.work_item.as_deref().unwrap_or("none yet")
        );
    }
    Ok(log)
}

fn forward_step(ctx: &Context<'_>, state: &mut State<'_>, result: &mut StepResult) -> StepOutcome {
    result.unit = "row(s)";
    let fail = |why: String| StepFailure::new(why.clone(), why);
    // A tree without a registry keeps none of its parent's rows: each is reported removed.
    let tree = if ctx.root.join(registry_gate::CHECKS).is_file() {
        read(ctx.root, registry_gate::CHECKS).map_err(fail)?
    } else {
        String::new()
    };
    let parent = match &ctx.parent {
        Parent::Unknown => {
            return Err(fail(
                "candidate mode needs --parent-checks <the parent commit's \
                 registry/checks.toml>, which ./dev commit passes; the forward-only rule never \
                 silently skips (D-56, P10)"
                    .to_owned(),
            ));
        }
        Parent::Absent => None,
        Parent::Checks(text) => Some(text.as_str()),
    };
    let (cases, found) = staging::demotions(parent, &tree).map_err(fail)?;
    result.cases = u64::try_from(cases).unwrap_or(u64::MAX);
    let allowances = ctx
        .allow_demotions
        .iter()
        .map(|text| text.parse::<Allowance>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(fail)?;
    let export = match fs::read_to_string(ctx.root.join(TRACKER_EXPORT)) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(fail(format!("cannot read {TRACKER_EXPORT}: {err}"))),
    };
    let (approved, violations) = staging::approve(found, &allowances, &export);
    let mut log = format!("{cases} parent row(s) compared\n");
    for demotion in &approved {
        let _ = writeln!(
            log,
            "approved: {} {} (plan change {})",
            demotion.id,
            demotion.describe(),
            demotion.plan_change.as_deref().unwrap_or("?")
        );
    }
    state.approved = approved;
    if violations.is_empty() {
        Ok(log)
    } else {
        Err(violations_failure(
            &format!("{} status violation(s) (D-56):", violations.len()),
            &violations,
        ))
    }
}

fn package_tests_step(
    ctx: &Context<'_>,
    state: &mut State<'_>,
    result: &mut StepResult,
) -> StepOutcome {
    result.unit = "test(s)";
    let metadata = metadata::load(ctx.root).map_err(|why| StepFailure::new(why.clone(), why))?;
    let members: Vec<String> = metadata.members().iter().map(|p| p.name.clone()).collect();
    let mut runs: Vec<(Option<String>, Vec<String>)> = members
        .iter()
        .map(|name| {
            let args = [
                "test",
                "--locked",
                "-p",
                name,
                "--all-targets",
                "--no-fail-fast",
            ];
            (
                Some(name.clone()),
                args.iter().map(|&a| a.to_owned()).collect(),
            )
        })
        .collect();
    let doc = ["test", "--locked", "--workspace", "--doc"];
    runs.push((None, doc.iter().map(|&a| a.to_owned()).collect()));
    let (mut log, mut commands, mut problems) = (String::new(), Vec::new(), Vec::new());
    let mut passed: u64 = 0;
    for (package, args) in runs {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let command = ctx.cargo.describe(&args);
        let label = package.as_deref().unwrap_or("doctests").to_owned();
        let _ = writeln!(log, "=== {command}");
        let reading = match run_captured(&mut ctx.cargo.command(ctx.root, &args)) {
            Err(failure) => {
                log.push_str(&failure.log);
                RunReading {
                    outcomes: Vec::new(),
                    passed: 0,
                    failure: Some(failure.detail.clone()),
                    incomplete: Some(failure.detail),
                }
            }
            Ok(captured) => {
                log.push_str(&combined(&captured));
                read_run(&captured, package.is_some())
            }
        };
        passed = passed.saturating_add(reading.passed);
        let RunReading {
            outcomes,
            failure,
            incomplete,
            ..
        } = reading;
        if let Some(why) = &failure {
            problems.push(format!("{label}: {why}"));
        }
        if let Some(why) = &incomplete
            && failure.as_deref() != Some(why.as_str())
        {
            problems.push(format!("{label}: incomplete run: {why}"));
        }
        if let Some(package) = package {
            state.runs.push(PackageRun {
                package,
                outcomes,
                failure,
                incomplete,
            });
        }
        commands.push(command);
    }
    result.command = Some(commands.join("; "));
    result.cases = passed;
    if problems.is_empty() {
        Ok(log)
    } else {
        let mut detail = format!("{} test run(s) failed:", problems.len());
        for line in problems.iter().take(DETAIL_LINES) {
            let _ = write!(detail, "\n  {line}");
        }
        Err(StepFailure::new(detail, log))
    }
}

/// What one `cargo test` run of the tests step showed.
struct RunReading {
    /// Every case's outcome.
    outcomes: Vec<(String, libtest::Outcome)>,
    /// Passed cases, as libtest's summaries count them.
    passed: u64,
    /// Why the run did not finish cleanly, if it did not.
    failure: Option<String>,
    /// Why its outcomes cannot be trusted, if they cannot (br-judge-cargo-partial-run-afvw).
    incomplete: Option<String>,
}

/// Reads one run's output: libtest's tally and every case's outcome. A package run's
/// outcomes are reconciled with libtest's and cargo's own counts; the doctest run starts no
/// `Running` binaries and is judged by its tally alone.
fn read_run(captured: &Captured, package_run: bool) -> RunReading {
    let mut passed = 0;
    let failure = match libtest::parse_tally(&captured.stdout) {
        Err(err) => Some(err.to_string()),
        Ok(tally) => {
            passed = u64::from(tally.passed);
            if tally.failed > 0 {
                Some(format!(
                    "{} test(s) failed: {}",
                    tally.failed,
                    libtest::failed_names(&captured.stdout).join(", ")
                ))
            } else if tally.ignored > 0 {
                Some(format!(
                    "{} test(s) are #[ignore]d; an ignored case fails (P10)",
                    tally.ignored
                ))
            } else if captured.status.success() {
                None
            } else {
                Some("cargo test exited unsuccessfully".to_owned())
            }
        }
    };
    let reconciled = package_run.then(|| {
        libtest::reconcile(
            &captured.stdout,
            &captured.stderr,
            captured.status.success(),
        )
    });
    let (outcomes, incomplete) = match reconciled {
        Some(Ok(outcomes)) => (outcomes, None),
        Some(Err(why)) => (libtest::outcomes(&captured.stdout), Some(why)),
        None => (libtest::outcomes(&captured.stdout), None),
    };
    RunReading {
        outcomes,
        passed,
        failure: failure.or_else(|| incomplete.clone()),
        incomplete,
    }
}

fn check_step(
    ctx: &Context<'_>,
    state: &State<'_>,
    id: &str,
    min_cases: u32,
    result: &mut StepResult,
) -> StepOutcome {
    let Some(def) = state.catalogue.iter().find(|def| def.id == id) else {
        let why = format!("check {id} is not in the catalogue");
        return Err(StepFailure::new(why.clone(), why));
    };
    match def.selector {
        Selector::Static(check) => {
            let log = static_step(ctx, check, result)?;
            if result.cases < u64::from(min_cases) {
                return Err(StepFailure::new(
                    format!(
                        "{} {} examined, fewer than the registered min_cases {min_cases} \
                         (§10.11)",
                        result.cases, result.unit
                    ),
                    log,
                ));
            }
            Ok(log)
        }
        Selector::CargoTest { package, filters } => {
            result.command = Some(format!("./dev check {id}"));
            result.unit = "test(s)";
            let passed = staging::judge_cargo(package, filters, min_cases, &state.runs)
                .map_err(|why| StepFailure::new(why.clone(), why))?;
            result.cases = passed;
            Ok(format!(
                "{passed} case(s) of package {package} (filters {filters:?}) passed in this \
                 gate's test run\n"
            ))
        }
    }
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

pub fn build_inputs(root: &Path) -> Result<BuildInputs, String> {
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
        raft_vendor_sha256: fs::read(root.join(crate::vendor::DIR).join(crate::vendor::MANIFEST))
            .ok()
            .map(|manifest| sha256_hex(&manifest)),
    })
}

/// `sha256:` of the tree ID and every build input, in a fixed, labelled order.
pub fn build_id(tree_id: &str, inputs: &BuildInputs) -> String {
    let canonical = format!(
        "keyval-build-id/v2\ntree {tree_id}\nrustc {}\ncargo {}\ntarget {}\nlockfile {}\n\
         features {}\nraft-patch {}\nraft-vendor {}\n",
        inputs.rustc,
        inputs.cargo,
        inputs.target,
        inputs.lockfile_sha256,
        inputs.features,
        inputs.raft_patch_sha256.as_deref().unwrap_or("none"),
        inputs.raft_vendor_sha256.as_deref().unwrap_or("none"),
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
                "  {status:<7} {:<18} {:>5} {:<17} {seconds:>6.1}s",
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
        for planned in &receipt.planned {
            let _ = writeln!(
                out,
                "  planned {:<18} reported, not run (work item {})",
                planned.id,
                planned.work_item.as_deref().unwrap_or("none yet")
            );
        }
        for demotion in &receipt.demotions {
            let _ = writeln!(
                out,
                "  demoted {:<18} {} → {} (plan change {})",
                demotion.id,
                demotion.from,
                demotion.to.as_deref().unwrap_or("removed"),
                demotion.plan_change.as_deref().unwrap_or("?")
            );
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
pub mod tests {
    // check: DEV-GATE
    use super::*;
    use crate::check::{ClaimClass, Kind, Tier};
    use crate::staging::Field;
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
            tree: resolve_tree_with(&root, Some(TREE), changed, &no_repository).unwrap(),
            artifacts: scratch_dir(&format!("gate-{fixture_name}")),
            cargo: nested_cargo("keyval-gate"),
            tool_path,
            progress: false,
            parent: Parent::Absent,
            allow_demotions: Vec::new(),
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
    fn the_build_inputs_record_the_vendored_raft_manifest() {
        let root = crate::test_support::workspace_root();
        let inputs = build_inputs(&root).unwrap();
        let manifest =
            fs::read(root.join(crate::vendor::DIR).join(crate::vendor::MANIFEST)).unwrap();
        assert_eq!(
            inputs.raft_vendor_sha256.as_deref(),
            Some(sha256_hex(&manifest).as_str()),
            "rule: the build ID covers the vendored Raft library's code-generation \
             provenance (T-3, W02.1); inputs={inputs:#?}"
        );
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
            raft_vendor_sha256: None,
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
            build_id(
                TREE,
                &BuildInputs {
                    raft_vendor_sha256: Some("dd".to_owned()),
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

    /// git as seen from an isolated candidate: there is no repository.
    fn no_repository(_root: &Path, _args: &[&str]) -> Result<String, String> {
        Err("fatal: not a git repository (or any of the parent directories): .git".to_owned())
    }

    #[test]
    fn asserted_tree_ids_are_validated() {
        let root = workspace_root();
        for bad in ["", "xyz", "0123", &"g".repeat(40)] {
            assert!(
                resolve_tree_with(&root, Some(bad), vec![], &no_repository).is_err(),
                "tree={bad:?}"
            );
        }
        let tree =
            resolve_tree_with(&root, Some(&TREE.to_uppercase()), vec![], &no_repository).unwrap();
        assert_eq!(tree.id, TREE, "IDs are normalised to lower case");
    }

    #[test]
    fn candidate_mode_takes_the_tree_from_the_caller_never_from_git() {
        let calls = std::cell::RefCell::new(Vec::new());
        let recording = |root: &Path, args: &[&str]| {
            calls.borrow_mut().push(args.join(" "));
            no_repository(root, args)
        };
        let changed = vec!["xtask/src/gate.rs".to_owned()];
        let tree =
            resolve_tree_with(&workspace_root(), Some(TREE), changed.clone(), &recording).unwrap();
        assert_eq!(
            (tree.id.as_str(), tree.source, tree.changed),
            (TREE, TreeSource::Caller, changed),
            "rule: the receipt's tree, and so its build ID, is the recorded tree ID"
        );
        assert_eq!(
            calls.borrow().as_slice(),
            ["rev-parse --absolute-git-dir"],
            "rule: candidate mode never asks git for the tree; its one git call is the \
             isolation probe (§11.10 step 4)"
        );
    }

    #[test]
    fn candidate_mode_refuses_a_tree_where_git_sees_a_repository() {
        let visible = |_root: &Path, _args: &[&str]| Ok("/w/.git\n".to_owned());
        let refused = resolve_tree_with(&workspace_root(), Some(TREE), vec![], &visible);
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("git-isolated") && e.contains("/w/.git")),
            "rule: a candidate run that can see a repository is refused (P11); \
             observed={refused:?}"
        );
    }

    #[test]
    fn a_real_candidate_passes_the_probe_only_under_the_isolation_environment() {
        let repo = scratch_dir("gate-isolation-probe");
        fixture_git(&repo, &["init", "--quiet"]).unwrap();
        let candidate = repo.join(".local/candidate");
        fs::create_dir_all(&candidate).unwrap();
        let isolated = |root: &Path, args: &[&str]| {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
            for (key, value) in candidate::git_isolation(root) {
                cmd.env(key, value);
            }
            let out = cmd.output().map_err(|err| err.to_string())?;
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&out.stderr).into_owned())
            }
        };
        let accepted = resolve_tree_with(&candidate, Some(TREE), vec![], &isolated);
        assert!(accepted.is_ok(), "observed={accepted:?}");
        let refused = resolve_tree_with(&candidate, Some(TREE), vec![], &fixture_git);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("git-isolated")),
            "control: without isolation the nested candidate sees the repository; \
             observed={refused:?}"
        );
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

    /// The catalogue of [`registry_fixture`]: a static check and a cargo-test check of the
    /// `gate-ok` package.
    pub const REGISTRY_CATALOGUE: &[CheckDef] = &[
        CheckDef {
            id: "R-STATIC",
            summary: "a static check",
            kind: Kind::Static,
            covers: &["P5"],
            class: ClaimClass::Invariant,
            tier: Tier::T0,
            selector: Selector::Static(StaticCheck::LintAllows),
            min_cases: 1,
        },
        CheckDef {
            id: "R-TESTS",
            summary: "a test group",
            kind: Kind::TestGroup,
            covers: &["P5"],
            class: ClaimClass::Invariant,
            tier: Tier::T0,
            selector: Selector::CargoTest {
                package: "gate-ok",
                filters: &["tests::"],
            },
            min_cases: 2,
        },
    ];

    /// A `checks.toml` with one row per `(id, status, min_cases)`.
    pub fn registry_checks(rows: &[(&str, &str, u32)]) -> String {
        let mut text = "schema = \"keyval.registry.checks/v1\"\n".to_owned();
        for (id, status, min_cases) in rows {
            let _ = write!(
                text,
                "\n[[check]]\nid = \"{id}\"\nsummary = \"x\"\nkind = \"static\"\n\
                 covers = [\"P5\"]\nclass = \"invariant\"\ntier = \"T0\"\n\
                 run = \"./dev check {id}\"\nmin_cases = {min_cases}\nstatus = \"{status}\"\n"
            );
        }
        text
    }

    /// The `gate-ok` crate plus a registry: `checks` as `registry/checks.toml`, a work graph
    /// whose W05 accepts `R-LATER`, and a tracker export with the closed plan-change bead
    /// `br-pc`.
    pub fn registry_fixture(name: &str, checks: &str) -> PathBuf {
        let root = scratch_dir(&format!("gate-registry-{name}"));
        for file in ["Cargo.toml", "Cargo.lock", "src/lib.rs"] {
            let to = root.join(file);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(fixture("gate-ok").join(file), to).unwrap();
        }
        fs::create_dir_all(root.join("registry")).unwrap();
        fs::create_dir_all(root.join(".beads")).unwrap();
        fs::write(root.join(registry_gate::CHECKS), checks).unwrap();
        let workgraph = format!(
            "schema = \"{}\"\n\n[[item]]\nid = \"W05\"\ntitle = \"Registry\"\ndeps = []\n\
             reqs = []\narea = [\"registry\"]\nmilestone = \"m0\"\n\
             accept = [\"./dev check R-LATER\"]\nbead = \"\"\n",
            workgraph::SCHEMA
        );
        fs::write(root.join(registry::WORKGRAPH), workgraph).unwrap();
        fs::write(
            root.join(TRACKER_EXPORT),
            "{\"id\":\"br-pc\",\"status\":\"closed\",\"labels\":[\"plan-change\"]}\n",
        )
        .unwrap();
        root
    }

    /// The registry set (without fmt, clippy and UBS) on a [`registry_fixture`].
    pub fn registry_run(root: &Path, parent: Parent, allow: &[&str]) -> GateReport {
        let ctx = Context {
            root,
            tree: resolve_tree_with(
                root,
                Some(TREE),
                vec!["src/lib.rs".to_owned()],
                &no_repository,
            )
            .unwrap(),
            artifacts: scratch_dir("gate-registry-artifacts"),
            cargo: nested_cargo("keyval-gate"),
            tool_path: None,
            progress: false,
            parent,
            allow_demotions: allow.iter().map(|&a| a.to_owned()).collect(),
        };
        run_registry(&ctx, REGISTRY_CATALOGUE, &[], &[]).unwrap()
    }

    #[test]
    fn registry_set_runs_implemented_rows_once_and_reports_planned_ones() {
        let checks = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
            ("R-LATER", "planned", 1),
        ]);
        let root = registry_fixture("runs", &checks);
        let report = registry_run(&root, Parent::Checks(checks), &[]);
        assert!(report.ok, "observed={report:#?}");
        let ids: Vec<&str> = report.receipt.steps.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            ["staging", "forward-only", "tests", "R-STATIC", "R-TESTS"],
            "rule: staging, the forward-only rule and one test run, then each implemented \
             row in checks.toml order (§10.10)"
        );
        assert_eq!(report.receipt.set, "registry");
        let tests = step(&report, "R-TESTS");
        assert_eq!(
            (tests.cases, tests.command.as_deref()),
            (2, Some("./dev check R-TESTS")),
            "rule: a cargo-test check is judged from the package run, never re-run"
        );
        assert!(
            step(&report, "tests")
                .command
                .as_deref()
                .is_some_and(|c| c.contains("-p gate-ok --all-targets --no-fail-fast")),
            "observed={report:#?}"
        );
        assert_eq!(
            report.receipt.planned,
            [Planned {
                id: "R-LATER".to_owned(),
                work_item: Some("W05".to_owned()),
            }],
            "rule: a planned row is reported with its work item and never fails (§10.10)"
        );
        assert_eq!(report.receipt.demotions, Vec::<Demotion>::new());
    }

    #[test]
    fn registry_set_fails_a_selection_short_of_its_min_cases() {
        let checks = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 3),
        ]);
        let root = registry_fixture("short", &checks);
        let report = registry_run(&root, Parent::Checks(checks), &[]);
        assert!(!report.ok, "observed={report:#?}");
        let tests = step(&report, "R-TESTS");
        assert!(
            tests.status == Verdict::Failed
                && tests
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("fewer than the registered min_cases 3")),
            "rule: every selected check executes at least its min_cases (§10.11); \
             observed={tests:#?}"
        );
        assert_eq!(step(&report, "R-STATIC").status, Verdict::Passed);
    }

    #[test]
    fn registry_set_applies_once_the_tree_or_its_parent_has_a_registry() {
        let parent = || Parent::Checks(registry_checks(&[("R-STATIC", "implemented", 1)]));
        let table = [
            (true, Parent::Absent, true, "the tree has a registry"),
            (
                false,
                Parent::Absent,
                false,
                "neither has one: W01's bootstrap set",
            ),
            (true, parent(), true, "both have one"),
            (
                false,
                parent(),
                true,
                "the tree dropped its parent's registry (0z0a)",
            ),
            (
                true,
                Parent::Unknown,
                true,
                "candidate mode, parent unknown",
            ),
            (
                false,
                Parent::Unknown,
                true,
                "parent unknown: never fall back (P10)",
            ),
        ];
        for (tree_has_registry, parent, expected, why) in table {
            assert_eq!(
                registry_set_applies(tree_has_registry, &parent),
                expected,
                "rule: {why} (§10.10: the switch to registry staging is one-way)"
            );
        }
    }

    #[test]
    fn registry_set_judges_a_tree_that_dropped_its_parents_registry() {
        let parent = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
        ]);
        // The gate-ok crate after a commit that deleted registry/checks.toml.
        let root = scratch_dir("gate-registry-dropped");
        for file in ["Cargo.toml", "Cargo.lock", "src/lib.rs"] {
            let to = root.join(file);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(fixture("gate-ok").join(file), to).unwrap();
        }
        let ctx = Context {
            root: &root,
            tree: resolve_tree_with(
                &root,
                Some(TREE),
                vec!["src/lib.rs".to_owned()],
                &no_repository,
            )
            .unwrap(),
            artifacts: scratch_dir("gate-registry-dropped-artifacts"),
            cargo: nested_cargo("keyval-gate-dropped"),
            tool_path: None,
            progress: false,
            parent: Parent::Checks(parent),
            allow_demotions: Vec::new(),
        };
        let report = run_fast(&ctx).unwrap();
        assert_eq!(
            report.receipt.set, "registry",
            "rule: a tree that drops its parent's registry never falls back to the bootstrap \
             set (§10.10, 0z0a); receipt={:#?}",
            report.receipt
        );
        let staging = step(&report, "staging");
        assert!(
            staging.status == Verdict::Failed
                && staging
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("never falls back")),
            "observed={staging:#?}"
        );
        let forward = step(&report, "forward-only");
        assert!(
            !report.ok
                && forward.status == Verdict::Failed
                && forward.detail.as_deref().is_some_and(|d| {
                    d.contains("R-STATIC") && d.contains("R-TESTS") && d.contains("removed")
                }),
            "rule: every row of the parent's registry is reported removed (D-56); \
             observed={forward:#?}"
        );
    }

    #[test]
    fn registry_set_fails_a_check_whose_package_run_was_cut_short() {
        let checks = registry_checks(&[("R-TESTS", "implemented", 1)]);
        let root = scratch_dir("gate-registry-aborted");
        for file in ["Cargo.toml", "Cargo.lock"] {
            fs::copy(fixture("gate-ok").join(file), root.join(file)).unwrap();
        }
        fs::create_dir_all(root.join("src")).unwrap();
        // tests::a_passes reports `ok`; then the binary aborts mid-run, so
        // tests::z_aborts_the_binary never reports and the summary never prints.
        fs::write(
            root.join("src/lib.rs"),
            "pub fn one() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    \
             #[test]\n    fn a_passes() {\n        assert_eq!(super::one(), 1);\n    }\n\n    \
             #[test]\n    fn z_aborts_the_binary() {\n        \
             std::thread::sleep(std::time::Duration::from_millis(800));\n        \
             std::process::abort();\n    }\n}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("registry")).unwrap();
        fs::write(root.join(registry_gate::CHECKS), &checks).unwrap();
        // Its own target dir: this crate is also named gate-ok, and cargo names a path
        // package's build by its workspace-relative path, so a shared target dir could run
        // another fixture's gate-ok binary instead.
        let ctx = Context {
            root: &root,
            tree: resolve_tree_with(
                &root,
                Some(TREE),
                vec!["src/lib.rs".to_owned()],
                &no_repository,
            )
            .unwrap(),
            artifacts: scratch_dir("gate-registry-aborted-artifacts"),
            cargo: nested_cargo("keyval-gate-aborted"),
            tool_path: None,
            progress: false,
            parent: Parent::Checks(checks),
            allow_demotions: Vec::new(),
        };
        let report = run_registry(&ctx, REGISTRY_CATALOGUE, &[], &[]).unwrap();
        let check = step(&report, "R-TESTS");
        assert!(
            !report.ok
                && check.status == Verdict::Failed
                && check.detail.as_deref().is_some_and(|d| {
                    d.contains("incomplete") && d.contains("stopped before its `test result:`")
                }),
            "rule: a check whose package run was cut short is never reported passed, though \
             the case that did report passed and meets min_cases 1 (§10.13, afvw); \
             observed={check:#?}"
        );
        assert_eq!(step(&report, "tests").status, Verdict::Failed);
    }

    #[test]
    fn registry_forward_only_refuses_a_lowered_min_cases_unless_a_closed_plan_change_allows_it() {
        let parent = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
        ]);
        let tree = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 1),
        ]);
        let root = registry_fixture("forward-min-cases", &tree);
        let refused = registry_run(&root, Parent::Checks(parent.clone()), &[]);
        let forward = step(&refused, "forward-only");
        assert!(
            !refused.ok
                && forward
                    .detail
                    .as_deref()
                    .is_some_and(|d| { d.contains("R-TESTS") && d.contains("min_cases 2 → 1") }),
            "rule: lowering an implemented check's min_cases fails the gate, though every \
             selected case still passes (§10.13, xo5i); observed={forward:#?}"
        );
        let allowed = registry_run(&root, Parent::Checks(parent), &["R-TESTS=br-pc"]);
        assert!(allowed.ok, "observed={allowed:#?}");
        assert_eq!(
            allowed.receipt.demotions,
            [Demotion {
                id: "R-TESTS".to_owned(),
                field: Field::MinCases,
                from: "2".to_owned(),
                to: Some("1".to_owned()),
                plan_change: Some("br-pc".to_owned()),
            }],
            "rule: the approved demotion is recorded with its field and plan change"
        );
    }

    #[test]
    fn registry_forward_only_refuses_a_removed_check_unless_a_closed_plan_change_allows_it() {
        let parent = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
            ("R-OLD", "implemented", 1),
        ]);
        let tree = registry_checks(&[
            ("R-STATIC", "implemented", 1),
            ("R-TESTS", "implemented", 2),
        ]);
        let root = registry_fixture("forward", &tree);
        let refused = registry_run(&root, Parent::Checks(parent.clone()), &[]);
        let forward = step(&refused, "forward-only");
        assert!(
            !refused.ok
                && forward
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("R-OLD") && d.contains("--allow-demotion")),
            "rule: deleting a registered check fails the gate (D-56, kdo9); observed={forward:#?}"
        );
        let allowed = registry_run(&root, Parent::Checks(parent), &["R-OLD=br-pc"]);
        assert!(allowed.ok, "observed={allowed:#?}");
        assert_eq!(
            allowed.receipt.demotions,
            [Demotion {
                id: "R-OLD".to_owned(),
                field: Field::Status,
                from: "implemented".to_owned(),
                to: None,
                plan_change: Some("br-pc".to_owned()),
            }],
            "rule: an approved demotion is recorded with its plan change"
        );
        let unknown = registry_run(&root, Parent::Unknown, &[]);
        assert!(
            !unknown.ok
                && step(&unknown, "forward-only")
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("--parent-checks")),
            "rule: without the parent's registry the rule fails, never skips (P10)"
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
