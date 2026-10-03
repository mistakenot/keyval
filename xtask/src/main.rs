//! `xtask`: the binary behind `./dev`, keyval's single entry point for repository
//! operations (PLAN.md §11.13, D-38). Every command supports `--json` (O-6); the
//! output and exit-status conventions live in [`output`].

// check: XTASK

mod cargo;
mod check;
mod doctor;
mod libtest;
mod output;
mod process;
mod workspace;

use std::env;
use std::ffi::OsString;
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
