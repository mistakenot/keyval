//! `xtask`: the binary behind `./dev`, keyval's single entry point for repository
//! operations (PLAN.md §11.13, D-38). Every command supports `--json` (O-6); the
//! output and exit-status conventions live in [`output`].

// check: XTASK

mod accept;
mod agents;
mod audit_reports;
mod beads;
mod candidate;
mod cargo;
mod check;
mod clock;
mod closure;
mod cluster;
mod commit;
mod corpus;
mod coverage;
mod demo;
mod doctor;
#[cfg(test)]
mod fixture_lists;
mod gate;
mod git;
mod ledger;
mod libtest;
mod lifecycle;
mod lint;
mod metadata;
#[cfg(test)]
mod model_spec;
#[cfg(test)]
mod observation_schema;
mod output;
mod process;
mod prune;
mod raft_audit;
mod registry;
mod registry_gate;
mod run_tree;
#[cfg(test)]
mod schemas;
mod sim;
mod sim_campaign;
mod spec;
mod spike;
mod staging;
mod steplog;
#[cfg(test)]
mod test_support;
mod tier_plan;
mod tier_run;
mod tiers;
mod traceability;
mod tracker;
mod vendor;
mod workgraph;
mod workspace;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::error::ErrorKind as ClapErrorKind;
use clap::{Parser, Subcommand};

use crate::output::{ErrorKind, Format, Status};

/// keyval repository operations (PLAN.md §11.13).
#[derive(Debug, Parser)]
#[command(
    name = "dev",
    bin_name = "./dev",
    version,
    about,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Print one JSON object with a versioned schema identifier (robot mode, O-6).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// The `./dev` commands. Each one lands with the bead that implements it (§11.13).
#[derive(Debug, Subcommand)]
enum Command {
    /// Verify the environment: pinned toolchain and every required tool (O-8).
    Doctor,
    /// Run registered checks by ID; unknown IDs and zero-case selections fail (P10).
    Check {
        /// Check IDs to run, e.g. XTASK or DEV-DOCTOR.
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
        /// Run only the selected cases whose names contain NAME.
        #[arg(long, value_name = "NAME")]
        case: Option<String>,
    },
    /// Commit through the exact-candidate wrapper (§11.10): `--bead <id> <PATH>...` commits
    /// work; `--close <id>` closes a bead with its accept receipt (§11.3 step 7);
    /// `--close <id> --failed --evidence <PATH>` records a failure and leaves it open.
    Commit {
        /// The in-progress bead whose work the paths carry.
        #[arg(
            long,
            value_name = "BEAD",
            conflicts_with = "close",
            required_unless_present = "close"
        )]
        bead: Option<String>,
        /// Close this bead: validate its accept receipt, close it, commit `.beads/` and
        /// registry activation.
        #[arg(long, value_name = "BEAD")]
        close: Option<String>,
        /// With --close: record a failure instead (labels failed + needs-owner; stays open).
        #[arg(long, requires = "close", requires = "evidence")]
        failed: bool,
        /// With --failed: the failure-evidence report to commit, relative to the root.
        #[arg(long, value_name = "PATH", requires = "failed")]
        evidence: Option<String>,
        /// Your Agent Mail name, whose live reservations must cover the paths
        /// (default: `$AGENT_NAME`).
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
        /// The commit message.
        #[arg(long, short, value_name = "TEXT")]
        message: String,
        /// Files or directories to commit (with --bead), relative to the workspace root.
        #[arg(
            value_name = "PATH",
            required_unless_present = "close",
            conflicts_with = "close"
        )]
        paths: Vec<String>,
        /// With --bead: `<CHECK-ID>=<bead>` lets this commit demote or remove that check, if
        /// the bead is a closed `plan-change` bead (D-56). Only on an owner decision;
        /// repeatable.
        #[arg(long, value_name = "CHECK=BEAD", requires = "bead")]
        allow_demotion: Vec<String>,
    },
    /// Run a bead's acceptance selectors on a committed tree in .local/accept/<tree-id>/
    /// and write a receipt naming that tree (§11.3 step 7).
    Accept {
        /// The bead to accept.
        #[arg(value_name = "BEAD")]
        bead: String,
        /// The committed tree to judge: the tree of the commit that carried the work.
        #[arg(long, value_name = "TREE_ID")]
        tree: String,
    },
    /// Apply the .local/ retention rule (§10.10, Q9): keep the newest accept trees and
    /// every failing run, remove the rest with their target dirs, log it in a receipt.
    Prune {
        /// Report what would be removed, and remove nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a gate tier on an exact tree and write a receipt (§10.10).
    Gate {
        /// The tier: `fast` (T0), `integration` (T1), `campaign` (T2) or `release` (T3).
        #[arg(value_enum)]
        tier: gate::GateTier,
        /// Candidate mode: the tree ID of this directory's content, recorded by the commit
        /// wrapper that materialised it. Without it, the checkout must be clean.
        #[arg(long, value_name = "TREE_ID")]
        tree: Option<String>,
        /// Candidate mode: a changed (staged) path for UBS to scan; repeatable.
        #[arg(long, value_name = "PATH", requires = "tree")]
        changed: Vec<String>,
        /// Directory for receipts and logs (default: <root>/artifacts).
        #[arg(long, value_name = "DIR")]
        artifacts: Option<PathBuf>,
        /// Candidate mode: the parent commit's `registry/checks.toml`, or an empty file if
        /// it has none, for the forward-only status rule (D-56). `./dev commit` writes it.
        #[arg(long, value_name = "FILE", requires = "tree")]
        parent_checks: Option<PathBuf>,
        /// `<CHECK-ID>=<bead>`: a closed `plan-change` bead approves demoting or removing
        /// that check (D-56); repeatable.
        #[arg(long, value_name = "CHECK=BEAD")]
        allow_demotion: Vec<String>,
    },
    /// Work graph → beads (§11.11): adopt or create one bead per work item, mirror the
    /// dependencies between them, and record bead IDs in registry/workgraph.toml.
    Beads {
        #[command(subcommand)]
        action: BeadsAction,
    },
    /// Render a report from the verification registry (§10.13).
    Report {
        #[command(subcommand)]
        report: ReportKind,
    },
    /// Simulated runs (§10.2): run one seed of a profile in a fresh process and print its
    /// report and trace hash, labelled a simulated slice (P6). W42 adds campaigns.
    Sim {
        #[command(subcommand)]
        action: SimAction,
    },
    /// Owner demos (§11.6): orchestrate what exists and say honestly what is pending (P6).
    Demo {
        #[command(subcommand)]
        action: DemoAction,
    },
    /// The local five-node cluster (O-2, §7.7 step 0): init, up, status, down, kill and restart, managed
    /// by PID receipts and never by port or process name (W22).
    Cluster {
        #[command(subcommand)]
        action: ClusterAction,
    },
    /// Scripted smokes (§10.17): the five-node lifecycle smoke on real processes (W24.3).
    Smoke {
        #[command(subcommand)]
        action: SmokeAction,
    },
}

/// `./dev smoke` actions.
#[derive(Clone, Copy, Debug, Subcommand)]
enum SmokeAction {
    /// Five real nodes: write keys, SIGKILL the leader and a follower, restart them, wait
    /// for all five to catch up, read every key from every node, stop, and examine every
    /// data directory with `keyval doctor`; the first failed step exits 1 (P10).
    Lifecycle,
}

/// `./dev cluster` actions; `kill` and `restart` also take `--node`.
#[derive(Clone, Debug, Subcommand)]
enum ClusterAction {
    /// Stamp one random instance ID into all five data directories; refuses used ones.
    Init(ClusterArgs),
    /// Start the five nodes; refuses uninitialised directories and occupied ports.
    Up(ClusterArgs),
    /// Report each node without changing anything.
    Status(ClusterArgs),
    /// Stop the nodes this launcher started; never deletes data.
    Down(ClusterArgs),
    /// SIGKILL one node this launcher started, as a crash probe does.
    Kill(ClusterNodeArgs),
    /// SIGKILL one node if it runs, then start it again; refuses occupied ports.
    Restart(ClusterNodeArgs),
}

/// Options of the `./dev cluster` actions that act on one node.
#[derive(Clone, Debug, clap::Args)]
struct ClusterNodeArgs {
    #[command(flatten)]
    cluster: ClusterArgs,
    /// The node ID, as in the config.
    #[arg(long)]
    node: u64,
}

/// Options of every `./dev cluster` action.
#[derive(Clone, Debug, clap::Args)]
struct ClusterArgs {
    /// The configuration, relative to the workspace root (default: config/local.toml).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Where the instance record and PID receipts live (default: derived from the config).
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

/// `./dev demo` actions.
#[derive(Clone, Debug, Subcommand)]
enum DemoAction {
    /// The M1 owner demo (§12.2 W19): one real node serves PUT and GET across a kill and
    /// restart, the simulated slice runs twice with identical trace hashes, and what is
    /// implemented and pending is printed.
    M1 {
        /// Never pause between parts; every step is asserted (a failed step exits 1).
        #[arg(long)]
        assert: bool,
    },
}

/// `./dev sim` actions.
#[derive(Clone, Debug, Subcommand)]
enum SimAction {
    /// Run one seed of a profile, or one scenario file, through the `keyval-sim` binary
    /// (§12.2 W17, W42.1); a failed or incomplete run exits 1 (P10).
    Run {
        /// The master seed.
        #[arg(long, value_name = "N", required_unless_present = "scenario")]
        seed: Option<u64>,
        /// The run profile: `slice` or `swarm` (a generated scenario).
        #[arg(
            long,
            value_name = "P",
            default_value = "slice",
            conflicts_with = "scenario"
        )]
        profile: String,
        /// A scenario file to replay exactly (it carries its own seed).
        #[arg(long, value_name = "FILE", conflicts_with = "seed")]
        scenario: Option<PathBuf>,
        /// The configuration, relative to the workspace root (default: config/local.toml).
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// The heal budget in ticks; a small one forces a failing run, for a crash pack
        /// (swarm and scenario runs).
        #[arg(long, value_name = "N")]
        heal_budget_ticks: Option<u64>,
    },
    /// Run a seed range of the swarm profile in parallel processes and report every seed;
    /// exits 1 unless every seed passed (P10).
    Campaign {
        /// The seeds, `A..B` inclusive (at most 10000).
        #[arg(long, value_name = "A..B")]
        seeds: String,
        /// The most parallel `keyval-sim` processes (1..=64).
        #[arg(long, value_name = "J", default_value_t = 4)]
        jobs: usize,
        /// The run profile; campaigns run `swarm`.
        #[arg(long, value_name = "P", default_value = "swarm")]
        profile: String,
        /// The configuration, relative to the workspace root (default: config/local.toml).
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Run the append-only regression corpus `sim/corpus/` (§10.9, W42.3); exits 1 unless the
    /// corpus verifies against its protected index and every selected case passed (P9, P10).
    Corpus {
        /// Only the fixed reviewed fast subset named in `sim/corpus/INDEX.toml`.
        #[arg(long)]
        fast: bool,
        /// The parent commit's `INDEX.toml`: the index may only have grown from it (P9).
        #[arg(long, value_name = "FILE")]
        parent_index: Option<PathBuf>,
        /// The configuration, relative to the workspace root (default: config/local.toml).
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Shrink a crash pack to a minimal failing run: the engine is W44's, so this exits as
    /// not implemented until then.
    Shrink {
        /// The crash pack directory, `artifacts/crashpacks/<seed>-<hash>`.
        #[arg(value_name = "CRASH_PACK")]
        pack: PathBuf,
    },
}

/// `./dev report` kinds.
#[derive(Clone, Debug, Subcommand)]
enum ReportKind {
    /// reports/traceability.md: each requirement, its status, covering checks with claim
    /// classes and latest receipts, and whether it passed on a tree (§10.13).
    Traceability {
        /// The tree "passed" is judged on (default: this checkout's `HEAD^{tree}`).
        #[arg(long, value_name = "TREE_ID")]
        tree: Option<String>,
        /// Directory holding receipts/ (default: <root>/artifacts).
        #[arg(long, value_name = "DIR")]
        artifacts: Option<PathBuf>,
    },
    /// reports/coverage.md: the coverage matrix per build, with empty cells and the required
    /// set from registry/coverage.toml (§10.4).
    Coverage {
        /// Coverage records (keyval.coverage/v1); default: every *.json in artifacts/coverage.
        #[arg(long = "input", value_name = "FILE")]
        inputs: Vec<PathBuf>,
    },
}

/// `./dev beads` actions.
#[derive(Clone, Copy, Debug, Subcommand)]
enum BeadsAction {
    /// Import the work graph into the tracker; idempotent.
    Import {
        /// Print what would change, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let status = match Cli::try_parse_from(&args) {
        Ok(cli) => run(cli),
        Err(err) => report_parse_error(&err, Format::detect(args.iter().skip(1))),
    };
    ExitCode::from(status.code())
}

/// Runs one `./dev smoke` action.
fn run_smoke(root: &Path, format: Format, action: SmokeAction) -> Status {
    match action {
        SmokeAction::Lifecycle => {
            let options = lifecycle::Options {
                progress: format == Format::Human,
            };
            output::emit(format, &lifecycle::run(root, options))
        }
    }
}

fn run(cli: Cli) -> Status {
    let format = if cli.json {
        Format::Json
    } else {
        Format::Human
    };
    let root = match workspace::root() {
        Ok(root) => root,
        Err(err) => {
            let message = err.to_string();
            return output::report_error(format, ErrorKind::Internal, &message, &message);
        }
    };
    match cli.command {
        Command::Doctor => {
            let pin = workspace::pinned_toolchain(&root).map_err(|err| err.to_string());
            let raft_codegen = vendor::codegen_pin(&root);
            output::emit(
                format,
                &doctor::run(&root, pin, doctor::TOOLS, raft_codegen),
            )
        }
        Command::Check { ids, case } => {
            let report = check::run(
                &root,
                check::CATALOGUE,
                &ids,
                case.as_deref(),
                &cargo::Cargo::detect(),
            );
            output::emit(format, &report)
        }
        Command::Gate {
            tier,
            tree,
            changed,
            artifacts,
            parent_checks,
            allow_demotion,
        } => run_gate(
            &root,
            format,
            tier,
            GateArgs {
                tree,
                changed,
                artifacts,
                parent_checks,
                allow_demotion,
            },
        ),
        Command::Commit {
            bead,
            close,
            failed: _,
            evidence,
            agent,
            message,
            paths,
            allow_demotion,
        } => run_commit(
            &root,
            format,
            CommitArgs {
                bead,
                close,
                evidence,
                agent,
                message,
                paths,
                allow_demotion,
            },
        ),
        Command::Beads { action } => run_beads(&root, format, action),
        Command::Report { report } => run_report(&root, format, report),
        Command::Sim { action } => run_sim(&root, format, action),
        Command::Demo { action } => match action {
            DemoAction::M1 { assert } => {
                let options = demo::Options {
                    assert,
                    progress: format == Format::Human,
                };
                output::emit(format, &demo::run(&root, options))
            }
        },
        Command::Cluster { action } => run_cluster(&root, format, action),
        Command::Smoke { action } => run_smoke(&root, format, action),
        Command::Prune { dry_run } => run_prune(&root, format, dry_run),
        Command::Accept { bead, tree } => {
            let tracker = tracker::LiveTracker { root: root.clone() };
            let report = accept::run(
                &git::Git::new(&root),
                &tracker,
                &accept::LiveRunner,
                &bead,
                &tree,
                &root.join("artifacts"),
            );
            output::emit(format, &report)
        }
    }
}

fn run_cluster(root: &Path, format: Format, action: ClusterAction) -> Status {
    let (kind, args, node) = match action {
        ClusterAction::Init(args) => (cluster::Action::Init, args, None),
        ClusterAction::Up(args) => (cluster::Action::Up, args, None),
        ClusterAction::Status(args) => (cluster::Action::Status, args, None),
        ClusterAction::Down(args) => (cluster::Action::Down, args, None),
        ClusterAction::Kill(args) => (cluster::Action::Kill, args.cluster, Some(args.node)),
        ClusterAction::Restart(args) => (cluster::Action::Restart, args.cluster, Some(args.node)),
    };
    let layout = cluster::Layout::new(root, sim::config_path(root, args.config), args.state_dir);
    output::emit(format, &cluster::run(root, kind, &layout, node))
}

fn run_beads(root: &Path, format: Format, action: BeadsAction) -> Status {
    match action {
        BeadsAction::Import { dry_run } => {
            let graph = beads::LiveBeadGraph {
                root: root.to_path_buf(),
            };
            output::emit(format, &beads::run(root, &graph, dry_run))
        }
    }
}

/// `./dev sim run|campaign|shrink`: simulated runs in fresh `keyval-sim` processes (§12.2
/// W17, W42.1).
fn run_sim(root: &Path, format: Format, action: SimAction) -> Status {
    let artifacts = root.join("artifacts");
    let cargo = cargo::Cargo::local();
    let refuse = |refusal: sim::Refusal| {
        output::report_error(format, refusal.kind, &refusal.message, &refusal.message)
    };
    let launch = |args: &[&str]| {
        let mut cmd = cargo.command(root, args);
        process::run(&mut cmd, sim::LIMIT).map_err(|err| format!("{}: {err}", cargo.describe(args)))
    };
    match action {
        SimAction::Run {
            seed,
            profile,
            scenario,
            config,
            heal_budget_ticks,
        } => {
            let config = sim::config_path(root, config);
            let source = match (scenario, seed) {
                (Some(file), _) => Some(sim_campaign::Source::File(root.join(file))),
                (None, Some(seed)) if profile == sim_campaign::SWARM => {
                    Some(sim_campaign::Source::Seed(seed))
                }
                (None, _) => None,
            };
            if let Some(source) = source {
                let identity = sim_campaign::write_build_identity(root, &artifacts).map(|build| {
                    sim_campaign::Extras {
                        heal_budget_ticks,
                        crash_packs: Some((build, artifacts.join(sim_campaign::CRASHPACK_DIR))),
                    }
                });
                return match identity.and_then(|extras| {
                    sim_campaign::run_with_extras(&source, &config, &artifacts, &extras, launch)
                }) {
                    Ok(report) => output::emit(format, &report),
                    Err(refusal) => refuse(refusal),
                };
            }
            let Some(seed) = seed else {
                return refuse(sim::Refusal::new(
                    ErrorKind::Usage,
                    "give --seed N or --scenario FILE".to_owned(),
                ));
            };
            let request = sim::Request {
                seed: Some(seed),
                profile,
                scenario: None,
                config,
            };
            match sim::run_with(&request, &artifacts, launch) {
                Ok(report) => output::emit(format, &report),
                Err(refusal) => refuse(refusal),
            }
        }
        SimAction::Campaign {
            seeds,
            jobs,
            profile,
            config,
        } => match sim_campaign::parse_range(&seeds) {
            Err(why) => refuse(sim::Refusal::new(ErrorKind::Usage, why)),
            Ok((from, to)) => {
                let request = sim_campaign::CampaignRequest {
                    from,
                    to,
                    profile,
                    config: sim::config_path(root, config),
                    jobs,
                };
                match sim_campaign::campaign_with(&request, &artifacts, &launch) {
                    Ok(report) => output::emit(format, &report),
                    Err(refusal) => refuse(refusal),
                }
            }
        },
        SimAction::Corpus {
            fast,
            parent_index,
            config,
        } => {
            let selection = if fast {
                corpus::Selection::Fast
            } else {
                corpus::Selection::All
            };
            match corpus::run_live(
                root,
                selection,
                sim::config_path(root, config),
                parent_index.as_deref(),
            ) {
                Ok(report) => output::emit(format, &report),
                Err(refusal) => refuse(refusal),
            }
        }
        SimAction::Shrink { pack } => refuse(sim_campaign::shrink(&pack)),
    }
}

fn run_report(root: &Path, format: Format, report: ReportKind) -> Status {
    match report {
        ReportKind::Traceability { tree, artifacts } => {
            let layout = traceability::Layout {
                work: root.join(traceability::WORK),
                artifacts: artifacts.unwrap_or_else(|| root.join("artifacts")),
                out: root.join(traceability::REPORT),
            };
            match traceability::run(&git::Git::new(root), &layout, tree) {
                Ok(report) => output::emit(format, &report),
                Err(why) => output::report_error(format, ErrorKind::Internal, &why, &why),
            }
        }
        ReportKind::Coverage { inputs } => match coverage::run(root, &inputs) {
            Ok(report) => output::emit(format, &report),
            Err(why) => output::report_error(format, ErrorKind::Internal, &why, &why),
        },
    }
}

/// `./dev prune`: the `.local/` retention rule (§10.10, Q9) under this checkout's target
/// base.
/// The `commit` subcommand's arguments.
struct CommitArgs {
    bead: Option<String>,
    close: Option<String>,
    evidence: Option<String>,
    agent: Option<String>,
    message: String,
    paths: Vec<String>,
    allow_demotion: Vec<String>,
}

fn run_commit(root: &Path, format: Format, args: CommitArgs) -> Status {
    let Some(agent) = args.agent.or_else(|| env::var("AGENT_NAME").ok()) else {
        let message = "name your Agent Mail identity with --agent or $AGENT_NAME";
        return output::report_error(format, ErrorKind::Usage, message, message);
    };
    let artifacts = root.join("artifacts");
    let git = git::Git::new(root);
    let ports = commit::LivePorts {
        root: root.to_path_buf(),
        artifacts: artifacts.clone(),
    };
    match (args.bead, args.close) {
        (Some(bead), None) => {
            let request = commit::Request {
                kind: commit::CommitKind::Work,
                bead,
                agent,
                message: args.message,
                paths: args.paths,
                allow_demotions: args.allow_demotion,
            };
            output::emit(format, &commit::run(&git, &request, &ports, &artifacts))
        }
        (None, Some(bead)) => {
            let request = closure::CloseRequest {
                bead,
                agent,
                message: args.message,
                failed_evidence: args.evidence,
            };
            let tracker = tracker::LiveTracker {
                root: root.to_path_buf(),
            };
            let report = closure::run(&git, &request, &tracker, &ports, &artifacts);
            output::emit(format, &report)
        }
        _ => {
            let message = "give exactly one of --bead or --close";
            output::report_error(format, ErrorKind::Usage, message, message)
        }
    }
}

/// The `gate` subcommand's options.
struct GateArgs {
    tree: Option<String>,
    changed: Vec<String>,
    artifacts: Option<PathBuf>,
    parent_checks: Option<PathBuf>,
    allow_demotion: Vec<String>,
}

fn run_prune(root: &Path, format: Format, dry_run: bool) -> Status {
    let layout = prune::Layout {
        root: root.to_path_buf(),
        base: cargo::target_base(root),
        artifacts: root.join("artifacts"),
    };
    output::emit(format, &prune::run(&layout, dry_run))
}

fn run_gate(root: &Path, format: Format, tier: gate::GateTier, args: GateArgs) -> Status {
    // T1–T3 judge their own materialised tree; the process that finds none given makes it
    // and runs the tree's own `./dev` there (§10.10 "Tree isolation").
    if tier != gate::GateTier::Fast && args.tree.is_none() {
        return run_gate_in_run_tree(root, format, tier, args);
    }
    if tier != gate::GateTier::Fast && !run_tree::is_run_tree(root) {
        let why = format!(
            "{} judges a run tree, not {}: run it without --tree (§10.10 tree isolation)",
            gate::tier_name(tier),
            root.display()
        );
        return output::report_error(format, ErrorKind::Refused, &why, &why);
    }
    // The forward-only rule's baseline (D-56): a candidate's comes from the commit wrapper;
    // a clean checkout's is HEAD's parent.
    let parent = match (&args.tree, &args.parent_checks) {
        (Some(_), Some(file)) => gate::Parent::from_file(file),
        (Some(_), None) => Ok(gate::Parent::Unknown),
        (None, _) => gate::Parent::of_head(root),
    };
    let parent = match parent {
        Ok(parent) => parent,
        Err(why) => return output::report_error(format, ErrorKind::Refused, &why, &why),
    };
    let tree = match gate::resolve_tree(root, args.tree.as_deref(), args.changed) {
        Ok(tree) => tree,
        Err(why) => return output::report_error(format, ErrorKind::Refused, &why, &why),
    };
    let ctx = gate::Context {
        root,
        tree,
        artifacts: args.artifacts.unwrap_or_else(|| root.join("artifacts")),
        cargo: cargo::Cargo::detect(),
        tool_path: None,
        progress: true,
        parent,
        allow_demotions: args.allow_demotion,
    };
    match gate::run_tier(&ctx, tier) {
        Ok(report) => output::emit(format, &report),
        Err(why) => output::report_error(format, ErrorKind::Internal, &why, &why),
    }
}

/// How long one T1–T3 run may take. A tier is never cut off by a wall-clock budget
/// (§10.11); this only bounds a hung process.
const TIER_RUN_LIMIT: std::time::Duration = std::time::Duration::from_hours(24);

/// `./dev gate integration|campaign|release` from the shared checkout: materialises the
/// clean `HEAD^{tree}` into `.local/gates/<run-id>/tree`, verifies it, runs that tree's
/// own `./dev gate <tier> --tree <id>` there under git isolation, and verifies the tree
/// again afterwards (W46.1).
fn run_gate_in_run_tree(
    root: &Path,
    format: Format,
    tier: gate::GateTier,
    args: GateArgs,
) -> Status {
    let refuse = |why: String| output::report_error(format, ErrorKind::Refused, &why, &why);
    let tree = match gate::resolve_tree(root, None, Vec::new()) {
        Ok(tree) => tree,
        Err(why) => return refuse(why),
    };
    let parent = match gate::Parent::of_head(root) {
        Ok(gate::Parent::Checks(text)) => text,
        Ok(gate::Parent::Absent) => String::new(),
        Ok(gate::Parent::Unknown) => {
            return refuse("the parent commit's registry is unknown".to_owned());
        }
        Err(why) => return refuse(why),
    };
    match gate::resolve_tree(root, None, Vec::new()) {
        Ok(again) if again.id == tree.id => {}
        Ok(_) => {
            return refuse("HEAD moved while the run was being set up; run it again".to_owned());
        }
        Err(why) => return refuse(why),
    }
    let git = git::Git::new(root);
    let id = run_tree::run_id(
        tier,
        &tree.id,
        &clock::Utc::now().compact(),
        std::process::id(),
    );
    let base = root.join(run_tree::DIR);
    let run = match run_tree::prepare(&git, &base, &id, &tree.id) {
        Ok(run) => run,
        Err(why) => return refuse(why),
    };
    let parent_file = base.join(&id).join("parent-checks.toml");
    if let Err(err) = std::fs::write(&parent_file, parent) {
        return refuse(format!("cannot write {}: {err}", parent_file.display()));
    }
    let target = match cargo::checkout_target_dir(&cargo::target_base(root), &run.dir) {
        Ok(target) => target,
        Err(why) => return refuse(why),
    };
    let shared = match args.artifacts {
        Some(dir) if dir.is_relative() => root.join(dir),
        Some(dir) => dir,
        None => root.join("artifacts"),
    };
    // The inner run writes into its own artifacts directory, seeded with what is already
    // there, so a receipt reaches the shared one (and T2 can reuse it) only once the tree
    // has been verified after the run.
    let own = base.join(&id).join("artifacts");
    if let Err(why) = run_tree::copy_missing(&shared, &own) {
        return refuse(why);
    }
    let inner = run_tree::inner_args(
        tier,
        &run,
        &parent_file,
        &own,
        &args.allow_demotion,
        format == Format::Json,
    );
    let mut cmd = std::process::Command::new(run.dir.join("dev"));
    cmd.args(&inner).current_dir(&run.dir);
    for (key, value) in accept::isolated_env(&run.dir, &target) {
        cmd.env(key, value);
    }
    let captured = match process::run(&mut cmd, TIER_RUN_LIMIT) {
        Ok(captured) => captured,
        Err(err) => {
            return refuse(format!(
                "cannot run {}: {err}",
                run.dir.join("dev").display()
            ));
        }
    };
    let wrote = output::write_line(&mut std::io::stdout().lock(), captured.stdout.trim_end());
    if !captured.stderr.is_empty() {
        let _ = output::write_line(&mut std::io::stderr().lock(), captured.stderr.trim_end());
    }
    if let Err(why) = run_tree::verify(&git, &run) {
        return refuse(format!(
            "the {} run modified its run tree {}: {why}; its receipts stay in {} and are not \
             published",
            gate::tier_name(tier),
            run.dir.display(),
            own.display()
        ));
    }
    if let Err(why) = run_tree::copy_missing(&own, &shared) {
        return refuse(why);
    }
    match wrote {
        Ok(()) if captured.status.success() => Status::Ok,
        Ok(()) | Err(_) => Status::Failed,
    }
}

/// Help and version requests succeed; every other parse failure is a usage error.
fn report_parse_error(err: &clap::Error, format: Format) -> Status {
    match err.kind() {
        ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => match err.print() {
            Ok(()) => Status::Ok,
            Err(_) => Status::Failed,
        },
        _ => {
            let rendered = err.render().to_string();
            let message = rendered
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ");
            output::report_error(format, ErrorKind::Usage, message, &rendered)
        }
    }
}
