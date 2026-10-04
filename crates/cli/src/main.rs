//! `keyval`: the command-line tool (PLAN.md §8.6). This binary only parses arguments and
//! prints; [`cli::doctor`] builds the report.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use types::{Config, NodeId};

/// The keyval command-line tool.
#[derive(Debug, Parser)]
#[command(name = "keyval", version, about = "The keyval command-line tool")]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// A keyval command.
#[derive(Debug, Subcommand)]
enum Command {
    /// Verify a node's data directory offline, without modifying it (O-7).
    Doctor {
        /// The node's data directory.
        data_dir: PathBuf,
        /// The cluster config, to verify the genesis binding as well (DUR-8).
        #[arg(long, requires = "node")]
        config: Option<PathBuf>,
        /// The node's ID in that config.
        #[arg(long, requires = "config")]
        node: Option<u64>,
        /// Print the `keyval.cli.doctor/v1` JSON report.
        #[arg(long)]
        json: bool,
    },
}

/// The configuration and node to verify the binding against, or why they cannot be read.
fn binding(config: Option<&Path>, node: Option<u64>) -> Result<Option<(Config, NodeId)>, String> {
    let (Some(path), Some(node)) = (config, node) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let config = Config::parse(&text, dir).map_err(|err| format!("{}: {err}", path.display()))?;
    let node = NodeId::new(node).map_err(|err| format!("--node {node}: {err}"))?;
    Ok(Some((config, node)))
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Doctor {
            data_dir,
            config,
            node,
            json,
        } => {
            let binding = match binding(config.as_deref(), node) {
                Ok(binding) => binding,
                Err(message) => {
                    eprintln!("keyval doctor: {message}");
                    return ExitCode::from(2);
                }
            };
            let report = cli::doctor::doctor(
                &data_dir,
                binding.as_ref().map(|(config, node)| (config, *node)),
            );
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("keyval doctor: cannot encode the report: {err}");
                        return ExitCode::from(1);
                    }
                }
            } else {
                print!("{}", report.human());
            }
            ExitCode::from(report.verdict.exit_code())
        }
    }
}
