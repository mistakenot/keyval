//! `keyval`: the command-line tool (PLAN.md §8.6, §8.7). This binary only parses arguments
//! and prints; [`cli::doctor`] builds the doctor report, [`cli::client`] runs the client
//! operations, [`cli::command`] renders their results, and [`cli::output`] is the error
//! envelope.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use cli::client::http::TcpTransport;
use cli::client::{Client, OsRandom, Settings, SystemClock};
use cli::command::{self, Rendered};
use cli::output::{ErrorKind, ErrorReport, USAGE_EXIT, json_requested};
use cli::pending::PendingStore;
use types::{Config, ExpectedVersion, Key, NodeId, PageSize, RequestId, ScanRange, Value, Version};

/// The keyval command-line tool.
#[derive(Debug, Parser)]
#[command(name = "keyval", version, about = "The keyval command-line tool")]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// What every client command takes.
#[derive(Debug, Args)]
struct Common {
    /// The cluster config naming the nodes and the retry bounds.
    #[arg(long, default_value = "config/local.toml")]
    config: PathBuf,
    /// Where unresolved writes are kept (default: `$KEYVAL_PENDING_DIR`, else
    /// `$XDG_STATE_HOME/keyval/pending`, else `~/.local/state/keyval/pending`).
    #[arg(long)]
    pending_dir: Option<PathBuf>,
    /// Print the command's versioned JSON report instead of text.
    #[arg(long)]
    json: bool,
}

/// A keyval command.
#[derive(Debug, Subcommand)]
enum Command {
    /// Write `value` under `key`.
    Put {
        /// The key.
        key: String,
        /// The value.
        value: String,
        #[command(flatten)]
        common: Common,
    },
    /// Read `key`.
    Get {
        /// The key.
        key: String,
        #[command(flatten)]
        common: Common,
    },
    /// Delete `key`.
    Delete {
        /// The key.
        key: String,
        #[command(flatten)]
        common: Common,
    },
    /// Replace `key`'s value only if it has the expected version (or is absent).
    Cas {
        /// The key.
        key: String,
        /// The new value.
        value: String,
        /// Expect the key to be absent.
        #[arg(long, conflicts_with = "version", required_unless_present = "version")]
        absent: bool,
        /// Expect the key to have exactly this version.
        #[arg(long)]
        version: Option<u64>,
        #[command(flatten)]
        common: Common,
    },
    /// Read one page of keys in order.
    Scan {
        /// The inclusive lower bound.
        #[arg(long)]
        start: Option<String>,
        /// The exclusive upper bound.
        #[arg(long)]
        end: Option<String>,
        /// Continue after this key (exclusive).
        #[arg(long)]
        after: Option<String>,
        /// The most items in the page.
        #[arg(long)]
        limit: Option<u64>,
        #[command(flatten)]
        common: Common,
    },
    /// Ask the nodes whether they have applied a request.
    Status {
        /// The request ID.
        request_id: String,
        #[command(flatten)]
        common: Common,
    },
    /// List the writes that have no terminal result yet.
    Pending {
        #[command(flatten)]
        common: Common,
    },
    /// Send a pending write again, with its own ID and its exact command.
    Resume {
        /// The request ID, as `put`, `delete`, `cas` or `pending` printed it.
        request_id: String,
        #[command(flatten)]
        common: Common,
    },
    /// Ask a node for the cluster's status.
    Cluster {
        #[command(flatten)]
        common: Common,
    },
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

/// The configuration and node to verify the binding against, or why they cannot be used:
/// an unreadable or invalid config is a configuration error, a bad `--node` a usage error.
fn binding(
    config: Option<&Path>,
    node: Option<u64>,
) -> Result<Option<(Config, NodeId)>, (ErrorKind, String)> {
    let (Some(path), Some(node)) = (config, node) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|err| {
        (
            ErrorKind::Config,
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let config = Config::parse(&text, dir)
        .map_err(|err| (ErrorKind::Config, format!("{}: {err}", path.display())))?;
    let node =
        NodeId::new(node).map_err(|err| (ErrorKind::Usage, format!("--node {node}: {err}")))?;
    Ok(Some((config, node)))
}

/// Reports an error that stopped `tool` (`keyval doctor`, `keyval put`, …) before its
/// report, and exits 2: the `keyval.cli.error/v1` envelope on stdout under `--json`, the
/// message on stderr otherwise (O-6, br-tdy6).
fn fail(tool: &str, json: bool, kind: ErrorKind, message: &str) -> ExitCode {
    if json {
        match serde_json::to_string_pretty(&ErrorReport::new(kind, message)) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("keyval: cannot encode the error ({err}): {message}"),
        }
    } else {
        eprintln!("{tool}: {message}");
    }
    ExitCode::from(USAGE_EXIT)
}

/// A command that could not run: what kind of error, and what it said.
type Failure = (ErrorKind, String);

/// Where unresolved writes are kept when `--pending-dir` is not given.
fn pending_dir(given: Option<PathBuf>) -> Result<PathBuf, Failure> {
    let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    pending_dir_from(
        given,
        env("KEYVAL_PENDING_DIR"),
        env("XDG_STATE_HOME"),
        env("HOME"),
    )
}

fn pending_dir_from(
    given: Option<PathBuf>,
    keyval: Option<OsString>,
    xdg_state: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Failure> {
    given
        .or_else(|| keyval.map(PathBuf::from))
        .or_else(|| xdg_state.map(|dir| PathBuf::from(dir).join("keyval").join("pending")))
        .or_else(|| home.map(|dir| PathBuf::from(dir).join(".local/state/keyval/pending")))
        .ok_or_else(|| {
            (
                ErrorKind::Usage,
                "no place for the pending store: pass --pending-dir or set KEYVAL_PENDING_DIR"
                    .to_owned(),
            )
        })
}

/// The config at `path`, parsed.
fn load_config(path: &Path) -> Result<Config, Failure> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        (
            ErrorKind::Config,
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    Config::parse(&text, dir)
        .map_err(|err| (ErrorKind::Config, format!("{}: {err}", path.display())))
}

fn usage<T, E: std::fmt::Display>(what: &str, result: Result<T, E>) -> Result<T, Failure> {
    result.map_err(|err| (ErrorKind::Usage, format!("{what}: {err}")))
}

/// A defect in `run_client`: a command that needs the pending store was given none.
fn missing_store() -> Failure {
    (
        ErrorKind::Pending,
        "internal error: no pending store".to_owned(),
    )
}

/// What a client command does once its config, store and client exist.
enum Work {
    Put(String, String),
    Get(String),
    Delete(String),
    Cas(String, String, Option<u64>),
    Scan {
        start: Option<String>,
        end: Option<String>,
        after: Option<String>,
        limit: Option<u64>,
    },
    Status(String),
    Pending,
    Resume(String),
    Cluster,
}

impl Work {
    /// Whether the command reads or writes the pending records.
    const fn uses_store(&self) -> bool {
        match self {
            Self::Put(..) | Self::Delete(_) | Self::Cas(..) | Self::Pending | Self::Resume(_) => {
                true
            }
            Self::Get(_) | Self::Scan { .. } | Self::Status(_) | Self::Cluster => false,
        }
    }
}

type RealClient = Client<TcpTransport, SystemClock, OsRandom>;

/// Runs one client command against the real network, clock and entropy.
fn run_client(common: &Common, work: Work) -> Result<Rendered, Failure> {
    let config = load_config(&common.config)?;
    let limits = config.key_value_limits();
    let key = |text: String| usage("key", Key::parse(text, limits));
    let value = |text: String| usage("value", Value::parse(text, limits));
    let id = |text: &str| usage("request ID", RequestId::parse(text));
    // Only the commands that write or read the pending records touch the store; a read
    // must work without a state directory.
    let store = if work.uses_store() {
        let dir = pending_dir(common.pending_dir.clone())?;
        Some(PendingStore::open(&dir).map_err(|err| (ErrorKind::Pending, err.to_string()))?)
    } else {
        None
    };
    let mut client: RealClient = Client::new(
        Settings::of(&config),
        TcpTransport,
        SystemClock::new(),
        OsRandom,
        store.clone(),
    );
    let client_failed = |err: cli::client::ClientError| (ErrorKind::Client, err.to_string());
    match work {
        Work::Put(k, v) => {
            let (key, value) = (key(k)?, value(v)?);
            let (report, request_id) = client.put(&key, &value).map_err(client_failed)?;
            Ok(command::render_write("put", request_id, &report))
        }
        Work::Delete(k) => {
            let key = key(k)?;
            let (report, request_id) = client.delete(&key).map_err(client_failed)?;
            Ok(command::render_write("delete", request_id, &report))
        }
        Work::Cas(k, v, version) => {
            let (key, value) = (key(k)?, value(v)?);
            let expected = match version {
                None => ExpectedVersion::Absent,
                Some(raw) => ExpectedVersion::Version(usage("--version", Version::new(raw))?),
            };
            let (report, request_id) = client.cas(&key, expected, &value).map_err(client_failed)?;
            Ok(command::render_write("cas", request_id, &report))
        }
        Work::Get(k) => {
            let key = key(k)?;
            let report = client.get(&key).map_err(client_failed)?;
            Ok(command::render_get(key.as_str(), &report))
        }
        Work::Scan {
            start,
            end,
            after,
            limit,
        } => {
            let limit = match limit {
                None => PageSize::DEFAULT,
                Some(raw) => usage("--limit", PageSize::new(raw))?,
            };
            let range = usage(
                "range",
                ScanRange::new(
                    start.map(key).transpose()?,
                    end.map(key).transpose()?,
                    after.map(key).transpose()?,
                    limit,
                )
                .map_err(|err| format!("{err:?}")),
            )?;
            let report = client.scan(&range).map_err(client_failed)?;
            Ok(command::render_scan(&report))
        }
        Work::Status(text) => {
            let report = client.status(id(&text)?).map_err(client_failed)?;
            Ok(command::render_status(&report))
        }
        Work::Cluster => {
            let report = client.cluster().map_err(client_failed)?;
            Ok(command::render_cluster(&report))
        }
        Work::Pending => {
            let store = store.ok_or_else(missing_store)?;
            let records = store
                .list()
                .map_err(|err| (ErrorKind::Pending, err.to_string()))?;
            Ok(command::render_pending(&store, &records))
        }
        Work::Resume(text) => {
            let store = store.ok_or_else(missing_store)?;
            command::resume(&mut client, &store, id(&text)?).map_err(|err| match err {
                command::CommandError::Pending(why) => (ErrorKind::Pending, why),
                command::CommandError::Client(err) => (ErrorKind::Client, err.to_string()),
            })
        }
    }
}

/// Prints a client command's result as it was asked for, and exits as the result says.
fn finish(name: &str, json: bool, result: Result<Rendered, Failure>) -> ExitCode {
    let rendered = match result {
        Ok(rendered) => rendered,
        Err((kind, message)) => return fail(&format!("keyval {name}"), json, kind, &message),
    };
    if json {
        match serde_json::to_string_pretty(&rendered.json) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("keyval {name}: cannot encode the report: {err}");
                return ExitCode::from(USAGE_EXIT);
            }
        }
    } else {
        print!("{}", rendered.human);
    }
    ExitCode::from(rendered.exit)
}

fn main() -> ExitCode {
    // Before parsing, so that even a usage error comes in the format asked for.
    let json = json_requested(std::env::args_os().skip(1));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and version are not errors; without --json, clap reports a usage error as
        // it always did (text on stderr, exit 2).
        Err(err) if !json || !err.use_stderr() => err.exit(),
        Err(err) => return fail("keyval", true, ErrorKind::Usage, &err.to_string()),
    };
    let (name, common, work) = match cli.command {
        Command::Doctor {
            data_dir,
            config,
            node,
            json,
        } => return doctor(&data_dir, config.as_deref(), node, json),
        Command::Put { key, value, common } => ("put", common, Work::Put(key, value)),
        Command::Get { key, common } => ("get", common, Work::Get(key)),
        Command::Delete { key, common } => ("delete", common, Work::Delete(key)),
        Command::Cas {
            key,
            value,
            version,
            common,
            absent: _,
        } => ("cas", common, Work::Cas(key, value, version)),
        Command::Scan {
            start,
            end,
            after,
            limit,
            common,
        } => (
            "scan",
            common,
            Work::Scan {
                start,
                end,
                after,
                limit,
            },
        ),
        Command::Status { request_id, common } => ("status", common, Work::Status(request_id)),
        Command::Pending { common } => ("pending", common, Work::Pending),
        Command::Resume { request_id, common } => ("resume", common, Work::Resume(request_id)),
        Command::Cluster { common } => ("cluster", common, Work::Cluster),
    };
    finish(name, common.json, run_client(&common, work))
}

/// `keyval doctor`.
fn doctor(data_dir: &Path, config: Option<&Path>, node: Option<u64>, json: bool) -> ExitCode {
    let binding = match binding(config, node) {
        Ok(binding) => binding,
        Err((kind, message)) => return fail("keyval doctor", json, kind, &message),
    };
    let report = cli::doctor::doctor(
        data_dir,
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
