//! `xtask`: the binary behind `./dev`, keyval's single entry point for repository
//! operations (PLAN.md §11.13, D-38). Every command supports `--json` (O-6); the
//! output and exit-status conventions live in [`output`].

// check: XTASK

mod agents;
mod candidate;
mod cargo;
mod check;
mod clock;
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
mod steplog;
#[cfg(test)]
mod test_support;
mod tiers;
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
    /// Commit the named paths: gate the exact staged tree, then commit it (§11.10).
    Commit {
        /// The in-progress bead this work belongs to.
        #[arg(long, value_name = "BEAD")]
        bead: String,
        /// Your Agent Mail name, whose live reservations must cover the paths
        /// (default: `$AGENT_NAME`).
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
        /// The commit message.
        #[arg(long, short, value_name = "TEXT")]
        message: String,
        /// Files or directories to commit, relative to the workspace root.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
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
            output::emit(format, &doctor::run(&root, pin, doctor::TOOLS))
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
            agent,
            message,
            paths,
        } => {
            let Some(agent) = agent.or_else(|| env::var("AGENT_NAME").ok()) else {
                let message = "name your Agent Mail identity with --agent or $AGENT_NAME";
                return output::report_error(format, ErrorKind::Usage, message, message);
            };
            let request = commit::Request {
                bead,
                agent,
                message,
                paths,
            };
            let artifacts = root.join("artifacts");
            let ports = commit::LivePorts {
                root: root.clone(),
                artifacts: artifacts.clone(),
            };
            let report = commit::run(&git::Git::new(&root), &request, &ports, &artifacts);
            output::emit(format, &report)
        }
    }
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
