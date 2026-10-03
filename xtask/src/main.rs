//! `xtask`: the binary behind `./dev`, keyval's single entry point for repository
//! operations (PLAN.md §11.13, D-38). Every command supports `--json` (O-6); the
//! output and exit-status conventions live in [`output`].

// check: XTASK

mod accept;
mod agents;
mod beads;
mod candidate;
mod cargo;
mod check;
mod clock;
mod closure;
mod commit;
mod doctor;
mod gate;
mod git;
mod ledger;
mod libtest;
mod lint;
mod metadata;
mod output;
mod process;
mod prune;
mod raft_audit;
mod registry;
mod registry_gate;
#[cfg(test)]
mod schemas;
mod spec;
mod spike;
mod steplog;
#[cfg(test)]
mod test_support;
mod tiers;
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
        /// The tier; only `fast` exists before W46.
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
    },
    /// Work graph → beads (§11.11): adopt or create one bead per work item, mirror the
    /// dependencies between them, and record bead IDs in registry/workgraph.toml.
    Beads {
        #[command(subcommand)]
        action: BeadsAction,
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
        } => run_gate(&root, format, tier, tree.as_deref(), changed, artifacts),
        Command::Commit {
            bead,
            close,
            failed: _,
            evidence,
            agent,
            message,
            paths,
        } => {
            let Some(agent) = agent.or_else(|| env::var("AGENT_NAME").ok()) else {
                let message = "name your Agent Mail identity with --agent or $AGENT_NAME";
                return output::report_error(format, ErrorKind::Usage, message, message);
            };
            let artifacts = root.join("artifacts");
            let git = git::Git::new(&root);
            let ports = commit::LivePorts {
                root: root.clone(),
                artifacts: artifacts.clone(),
            };
            match (bead, close) {
                (Some(bead), None) => {
                    let request = commit::Request {
                        kind: commit::CommitKind::Work,
                        bead,
                        agent,
                        message,
                        paths,
                    };
                    output::emit(format, &commit::run(&git, &request, &ports, &artifacts))
                }
                (None, Some(bead)) => {
                    let request = closure::CloseRequest {
                        bead,
                        agent,
                        message,
                        failed_evidence: evidence,
                    };
                    let tracker = tracker::LiveTracker { root };
                    let report = closure::run(&git, &request, &tracker, &ports, &artifacts);
                    output::emit(format, &report)
                }
                _ => {
                    let message = "give exactly one of --bead or --close";
                    output::report_error(format, ErrorKind::Usage, message, message)
                }
            }
        }
        Command::Beads { action } => run_beads(&root, format, action),
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

/// `./dev prune`: the `.local/` retention rule (§10.10, Q9) under this checkout's target
/// base.
fn run_prune(root: &Path, format: Format, dry_run: bool) -> Status {
    let layout = prune::Layout {
        root: root.to_path_buf(),
        base: cargo::target_base(root),
        artifacts: root.join("artifacts"),
    };
    output::emit(format, &prune::run(&layout, dry_run))
}

fn run_gate(
    root: &Path,
    format: Format,
    tier: gate::GateTier,
    tree: Option<&str>,
    changed: Vec<String>,
    artifacts: Option<PathBuf>,
) -> Status {
    if tier != gate::GateTier::Fast {
        let message = format!(
            "gate tier `{}` is not implemented yet (W46); a missing tier never counts as a \
             pass (P10)",
            gate::tier_name(tier)
        );
        return output::report_error(format, ErrorKind::NotImplemented, &message, &message);
    }
    let tree = match gate::resolve_tree(root, tree, changed) {
        Ok(tree) => tree,
        Err(why) => return output::report_error(format, ErrorKind::Refused, &why, &why),
    };
    let ctx = gate::Context {
        root,
        tree,
        artifacts: artifacts.unwrap_or_else(|| root.join("artifacts")),
        cargo: cargo::Cargo::detect(),
        tool_path: None,
        progress: true,
    };
    match gate::run(&ctx, tier, gate::BOOTSTRAP_SET) {
        Ok(report) => output::emit(format, &report),
        Err(why) => output::report_error(format, ErrorKind::Internal, &why, &why),
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
